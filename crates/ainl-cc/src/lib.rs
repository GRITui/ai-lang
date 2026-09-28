//! AOT compiler for AINL: AST (`ainl_core::Node`) -> a single self-contained
//! C file -> `cc`. Zero external dependencies (reuses ainl-core's AST).
//!
//! Design:
//! - Top-level `def`s use dense global slots (`g_top[]`) so the 40k hot loop
//!   stays tight (no name-based lookup).
//! - Function locals use the runtime's name-based `Scope` chain (correct for
//!   closures / `let` / `def`).
//! - Name resolution: local (scope chain) > global (dense slot) > builtin >
//!   free variable (scope chain, resolved at runtime).
//! - Refcounting: every expression produces an owned `Value`; the caller
//!   unrefs it when done.
//!
//! Two output buffers:
//! - `self.out`  -> file scope (runtime, global slots, fn bodies, main).
//! - `self.code` -> expression code, spliced into `main` or a fn body.

use ainl_core::Node;
use std::collections::{HashMap, HashSet};

/// (name, builtin id) for the 27 prelude builtins (must match runtime.c).
const BUILTIN_IDS: &[(&str, i32)] = &[
    ("+", 0),
    ("*", 1),
    ("-", 2),
    ("/", 3),
    ("=", 4),
    ("<", 5),
    (">", 6),
    ("<=", 7),
    (">=", 8),
    ("not", 9),
    ("mod", 10),
    ("print", 11),
    ("str", 12),
    ("list", 13),
    ("len", 14),
    ("first", 15),
    ("rest", 16),
    ("nth", 17),
    ("cons", 18),
    ("push", 19),
    ("hash", 20),
    ("get", 21),
    ("assoc", 22),
    ("has", 23),
    ("keys", 24),
    ("vals", 25),
    ("error", 26),
];

/// Compile AINL forms to a self-contained C file (runtime + generated code).
pub fn generate(forms: &[Node]) -> String {
    let mut g = Gen::new();
    // Pass 1: collect top-level def names -> global slots.
    for f in forms {
        g.collect_globals(f, false);
    }
    let n_globals = g.global_names.len();
    // File scope: runtime, global slots.
    g.emit(include_str!("runtime.c"));
    g.emit("\n");
    g.emit(&format!("static Value g_top[{n_globals}];\n\n"));
    // Generate top-level code (goes into main) + collect fn bodies.
    let mut main_code = String::new();
    for f in forms {
        main_code.push_str(&g.gen_toplevel(f));
    }
    // Forward declarations for all generated functions.
    for i in 0..g.fn_count {
        g.emit(&format!(
            "static Value fn_{i}(Scope *env, Value *args, int nargs);\n"
        ));
    }
    g.emit("\n");
    // Function bodies.
    let bodies = std::mem::take(&mut g.fn_bodies);
    for body in &bodies {
        g.emit(body);
        g.emit("\n");
    }
    // main.
    g.emit_main(&main_code, n_globals);
    g.out
}

struct Gen {
    out: String,
    code: String,
    tmp_count: usize,
    fn_count: usize,
    let_count: usize,
    /// Top-level def name -> dense slot index in g_top[].
    globals: HashMap<String, usize>,
    /// Global names in order (for the g_top[] array).
    global_names: Vec<String>,
    /// Stack of local scope names (for name resolution).
    scoping: Vec<HashSet<String>>,
    /// Collected function bodies (emitted after forward declarations).
    fn_bodies: Vec<String>,
}

impl Gen {
    fn new() -> Self {
        Gen {
            out: String::new(),
            code: String::new(),
            tmp_count: 0,
            fn_count: 0,
            let_count: 0,
            globals: HashMap::new(),
            global_names: Vec::new(),
            scoping: Vec::new(),
            fn_bodies: Vec::new(),
        }
    }

    fn emit(&mut self, s: &str) {
        self.out.push_str(s);
    }

    fn emit_code(&mut self, s: &str) {
        self.code.push_str(s);
    }

    fn fresh(&mut self) -> String {
        let n = self.tmp_count;
        self.tmp_count += 1;
        format!("t{n}")
    }

    fn fresh_arr(&mut self) -> String {
        let n = self.tmp_count;
        self.tmp_count += 1;
        format!("arr{n}")
    }

    fn intern_global(&mut self, name: &str) {
        if !self.globals.contains_key(name) {
            let i = self.global_names.len();
            self.global_names.push(name.to_string());
            self.globals.insert(name.to_string(), i);
        }
    }

    /// Collect top-level def names (not inside a let/fn).
    fn collect_globals(&mut self, node: &Node, in_scope: bool) {
        if in_scope {
            return;
        }
        if let Node::List(items, _) = node {
            if let Some(Node::Sym(op, _)) = items.first() {
                match op.as_str() {
                    "def" => {
                        if let Some(Node::Sym(name, _)) = items.get(1) {
                            self.intern_global(name);
                        }
                        return;
                    }
                    "let" | "fn" => {
                        for item in &items[1..] {
                            self.collect_globals(item, true);
                        }
                        return;
                    }
                    "quote" => return,
                    _ => {}
                }
            }
            for item in items {
                self.collect_globals(item, false);
            }
        }
    }

    /// Add a name to the current (top) local scope.
    fn add_local(&mut self, name: &str) {
        if let Some(scope) = self.scoping.last_mut() {
            scope.insert(name.to_string());
        }
    }

    /// Resolve a name to a C expression that yields an owned Value.
    fn resolve_name(&self, name: &str, env: &str) -> String {
        // 1. Local (in the scoping stack).
        for scope in self.scoping.iter().rev() {
            if scope.contains(name) {
                return format!("scope_lookup({}, \"{}\")", env, c_escape(name));
            }
        }
        // 2. Global.
        if let Some(&i) = self.globals.get(name) {
            return format!("slot_ref(g_top, {i})");
        }
        // 3. Builtin.
        for (bn, id) in BUILTIN_IDS {
            if *bn == name {
                return format!("v_builtin({id})");
            }
        }
        // 4. Free variable (resolved via scope chain at runtime).
        format!("scope_lookup({}, \"{}\")", env, c_escape(name))
    }

    /// Generate C code evaluating `node`, leaving the result (an owned Value)
    /// in a fresh temporary. Returns the temporary's name.
    fn gen_expr(&mut self, node: &Node, env: &str) -> String {
        match node {
            Node::Int(i, _) => {
                let t = self.fresh();
                self.emit_code(&format!("Value {t} = v_int({i});\n"));
                t
            }
            Node::Float(x, _) => {
                let t = self.fresh();
                self.emit_code(&format!("Value {t} = v_float({x:?});\n"));
                t
            }
            Node::Str(s, _) => {
                let t = self.fresh();
                self.emit_code(&format!("Value {t} = v_str({});\n", c_string(s)));
                t
            }
            Node::Sym(name, _) => {
                let t = self.fresh();
                match name.as_str() {
                    "true" => self.emit_code(&format!("Value {t} = v_bool(1);\n")),
                    "false" => self.emit_code(&format!("Value {t} = v_bool(0);\n")),
                    "nil" => self.emit_code(&format!("Value {t} = v_nil();\n")),
                    _ => {
                        let access = self.resolve_name(name, env);
                        self.emit_code(&format!("Value {t} = {access};\n"));
                    }
                }
                t
            }
            Node::List(items, _) => {
                if items.is_empty() {
                    let t = self.fresh();
                    self.emit_code(&format!("Value {t} = v_nil();\n"));
                    return t;
                }
                if let Node::Sym(op, _) = &items[0] {
                    match op.as_str() {
                        "def" => return self.gen_def(items, env),
                        "fn" => return self.gen_fn(items, env),
                        "if" => return self.gen_if(items, env),
                        "do" => return self.gen_do(items, env),
                        "let" => return self.gen_let(items, env),
                        "while" => return self.gen_while(items, env),
                        "quote" => return self.gen_quote(items),
                        "and" => return self.gen_and(items, env),
                        "or" => return self.gen_or(items, env),
                        _ => {}
                    }
                }
                self.gen_call(items, env)
            }
        }
    }

    /// Generate C code evaluating `node`, storing the result (owned) in
    /// `target`.
    fn gen_expr_into(&mut self, node: &Node, env: &str, target: &str) {
        let tmp = self.gen_expr(node, env);
        self.emit_code(&format!(
            "v_ref(&{tmp});\n{target} = {tmp};\nv_unref(&{tmp});\n"
        ));
    }

    fn gen_toplevel(&mut self, form: &Node) -> String {
        let start = self.code.len();
        let tmp = self.gen_expr(form, "g_env");
        let mut code = self.code.split_off(start);
        code.push_str(&format!("v_unref(&{tmp});\n"));
        code
    }

    fn gen_call(&mut self, items: &[Node], env: &str) -> String {
        let callee = &items[0];
        // Fast path: (op a b) where op is a known 2-arg operator -> emit the
        // tight inline directly (no v_call dispatch, no args array).
        if let Node::Sym(op, _) = callee {
            if let Some(fn_name) = two_arg_inline(op) {
                let a = self.gen_expr(&items[1], env);
                let b = self.gen_expr(&items[2], env);
                let t = self.fresh();
                self.emit_code(&format!("Value {t} = {fn_name}({a}, {b});\n"));
                self.emit_code(&format!("v_unref(&{a});\n"));
                self.emit_code(&format!("v_unref(&{b});\n"));
                return t;
            }
        }
        let callee_tmp = self.gen_expr(callee, env);
        let mut arg_tmps = Vec::new();
        for arg in &items[1..] {
            arg_tmps.push(self.gen_expr(arg, env));
        }
        let nargs = arg_tmps.len();
        let t = self.fresh();
        if nargs == 0 {
            self.emit_code(&format!(
                "Value {t} = v_call({callee_tmp}, NULL, 0);\n"
            ));
        } else {
            let args_arr = self.fresh_arr();
            self.emit_code(&format!(
                "Value {args_arr}[{nargs}] = {{ {} }};\n",
                arg_tmps.join(", ")
            ));
            self.emit_code(&format!(
                "Value {t} = v_call({callee_tmp}, {args_arr}, {nargs});\n"
            ));
        }
        self.emit_code(&format!("v_unref(&{callee_tmp});\n"));
        for arg in &arg_tmps {
            self.emit_code(&format!("v_unref(&{arg});\n"));
        }
        t
    }

    fn gen_def(&mut self, items: &[Node], env: &str) -> String {
        let name = match &items[1] {
            Node::Sym(n, _) => n.clone(),
            _ => {
                self.emit_code("/* def: name must be a symbol */\n");
                return self.fresh_nil();
            }
        };
        let val_tmp = self.gen_expr(&items[2], env);
        if self.scoping.is_empty() {
            // Top-level: bind to a global slot.
            let i = *self
                .globals
                .entry(name.clone())
                .or_insert_with(|| {
                    let i = self.global_names.len();
                    self.global_names.push(name.clone());
                    i
                });
            self.emit_code(&format!("slot_set(g_top, {i}, {val_tmp});\n"));
        } else {
            self.add_local(&name);
            self.emit_code(&format!(
                "scope_define({}, \"{}\", {val_tmp});\n",
                env,
                c_escape(&name)
            ));
        }
        let t = self.fresh();
        self.emit_code(&format!(
            "Value {t} = v_sym(\"{}\");\n",
            c_escape(&name)
        ));
        t
    }

    fn fresh_nil(&mut self) -> String {
        let t = self.fresh();
        self.emit_code(&format!("Value {t} = v_nil();\n"));
        t
    }

    fn parse_params(&self, params_node: &Node) -> (Vec<String>, Option<String>) {
        let mut params = Vec::new();
        let mut variadic = None;
        if let Node::List(param_nodes, _) = params_node {
            let mut i = 0;
            while i < param_nodes.len() {
                if let Node::Sym(p, _) = &param_nodes[i] {
                    if p == "&" {
                        if let Some(Node::Sym(rest, _)) = param_nodes.get(i + 1) {
                            variadic = Some(rest.clone());
                        }
                        break;
                    }
                    params.push(p.clone());
                }
                i += 1;
            }
        }
        (params, variadic)
    }

    fn gen_fn(&mut self, items: &[Node], env: &str) -> String {
        let params_node = &items[1];
        let body = &items[2..];
        let (params, variadic) = self.parse_params(params_node);
        let fn_idx = self.fn_count;
        self.fn_count += 1;
        // Push a scope with the params.
        let mut scope = HashSet::new();
        for p in &params {
            scope.insert(p.clone());
        }
        if let Some(v) = &variadic {
            scope.insert(v.clone());
        }
        self.scoping.push(scope);
        // Generate the body (emits to self.code).
        let start = self.code.len();
        let mut last_tmp = String::new();
        for form in body {
            let tmp = self.gen_expr(form, "env");
            if !last_tmp.is_empty() {
                self.emit_code(&format!("v_unref(&{last_tmp});\n"));
            }
            last_tmp = tmp;
        }
        let body_code = self.code.split_off(start);
        self.scoping.pop();
        let body_str = if last_tmp.is_empty() {
            "  return v_nil();\n".to_string()
        } else {
            format!("  return {last_tmp};\n")
        };
        let fn_body = format!(
            "static Value fn_{fn_idx}(Scope *env, Value *args, int nargs) {{\n  (void)args; (void)nargs;\n{body_code}{body_str}}}\n"
        );
        self.fn_bodies.push(fn_body);
        // Emit the closure creation (into self.code).
        let t = self.fresh();
        let nparams = params.len();
        if nparams == 0 && variadic.is_none() {
            self.emit_code(&format!(
                "Value {t} = v_closure(0, NULL, NULL, fn_{fn_idx}, {env});\n"
            ));
        } else {
            let parr = self.fresh_arr();
            self.emit_code(&format!(
                "char **{parr} = malloc({nparams} * sizeof(char *));\n"
            ));
            for (k, p) in params.iter().enumerate() {
                self.emit_code(&format!(
                    "{parr}[{k}] = strdup(\"{}\");\n",
                    c_escape(p)
                ));
            }
            let variadic_c = match &variadic {
                Some(v) => format!("strdup(\"{}\")", c_escape(v)),
                None => "NULL".to_string(),
            };
            self.emit_code(&format!(
                "Value {t} = v_closure({nparams}, {parr}, {variadic_c}, fn_{fn_idx}, {env});\n"
            ));
        }
        t
    }

    fn gen_if(&mut self, items: &[Node], env: &str) -> String {
        let cond_tmp = self.gen_expr(&items[1], env);
        let t = self.fresh();
        self.emit_code(&format!("Value {t};\n"));
        self.emit_code(&format!("if (v_truthy(&{cond_tmp})) {{\n"));
        self.gen_expr_into(&items[2], env, &t);
        self.emit_code("} else {\n");
        if items.len() == 4 {
            self.gen_expr_into(&items[3], env, &t);
        } else {
            self.emit_code(&format!("{t} = v_nil();\n"));
        }
        self.emit_code("}\n");
        self.emit_code(&format!("v_unref(&{cond_tmp});\n"));
        t
    }

    fn gen_do(&mut self, items: &[Node], env: &str) -> String {
        let mut last_tmp = String::new();
        for form in &items[1..] {
            let tmp = self.gen_expr(form, env);
            if !last_tmp.is_empty() {
                self.emit_code(&format!("v_unref(&{last_tmp});\n"));
            }
            last_tmp = tmp;
        }
        if last_tmp.is_empty() {
            self.fresh_nil()
        } else {
            last_tmp
        }
    }

    fn gen_let(&mut self, items: &[Node], env: &str) -> String {
        let binds_node = &items[1];
        let body = &items[2..];
        let let_env = format!("let_env_{}", self.let_count);
        self.let_count += 1;
        self.emit_code(&format!("Scope *{let_env} = scope_new({env});\n"));
        // Push a scope with the let bindings.
        let mut scope = HashSet::new();
        if let Node::List(binds, _) = binds_node {
            for bind in binds {
                if let Node::List(pair, _) = bind {
                    if let Some(Node::Sym(name, _)) = pair.first() {
                        scope.insert(name.clone());
                    }
                }
            }
        }
        self.scoping.push(scope);
        // Evaluate the bindings.
        if let Node::List(binds, _) = binds_node {
            for bind in binds {
                if let Node::List(pair, _) = bind {
                    if let (Some(Node::Sym(name, _)), Some(val_node)) =
                        (pair.first(), pair.get(1))
                    {
                        let val_tmp = self.gen_expr(val_node, &let_env);
                        self.emit_code(&format!(
                            "scope_define({let_env}, \"{}\", {val_tmp});\n",
                            c_escape(name)
                        ));
                    }
                }
            }
        }
        // Evaluate the body.
        let mut last_tmp = String::new();
        for form in body {
            let tmp = self.gen_expr(form, &let_env);
            if !last_tmp.is_empty() {
                self.emit_code(&format!("v_unref(&{last_tmp});\n"));
            }
            last_tmp = tmp;
        }
        self.scoping.pop();
        self.emit_code(&format!("scope_unref({let_env});\n"));
        if last_tmp.is_empty() {
            self.fresh_nil()
        } else {
            last_tmp
        }
    }

    fn gen_while(&mut self, items: &[Node], env: &str) -> String {
        let cond_node = &items[1];
        let body = &items[2..];
        let t = self.fresh();
        self.emit_code(&format!("Value {t} = v_nil();\n"));
        self.emit_code("while (1) {\n");
        self.emit_code("  {\n");
        self.emit_code("  tick(); if (g_err) break;\n");
        let cond_tmp = self.gen_expr(cond_node, env);
        self.emit_code(&format!(
            "  if (!v_truthy(&{cond_tmp})) {{ v_unref(&{cond_tmp}); break; }}\n"
        ));
        self.emit_code(&format!("  v_unref(&{cond_tmp});\n"));
        let mut last_tmp = String::new();
        for form in body {
            let tmp = self.gen_expr(form, env);
            if !last_tmp.is_empty() {
                self.emit_code(&format!("  v_unref(&{last_tmp});\n"));
            }
            last_tmp = tmp;
        }
        if !last_tmp.is_empty() {
            self.emit_code(&format!("  v_unref(&{t});\n"));
            self.emit_code(&format!("  v_ref(&{last_tmp});\n"));
            self.emit_code(&format!("  {t} = {last_tmp};\n"));
            self.emit_code(&format!("  v_unref(&{last_tmp});\n"));
        }
        self.emit_code("  }\n");
        self.emit_code("}\n");
        t
    }

    fn gen_quote(&mut self, items: &[Node]) -> String {
        let node = &items[1];
        let t = self.fresh();
        self.gen_quote_into(node, &t);
        t
    }

    fn gen_quote_into(&mut self, node: &Node, target: &str) {
        self.emit_code(&format!("Value {target};\n"));
        match node {
            Node::Int(i, _) => self.emit_code(&format!("{target} = v_int({i});\n")),
            Node::Float(x, _) => self.emit_code(&format!("{target} = v_float({x:?});\n")),
            Node::Str(s, _) => {
                self.emit_code(&format!("{target} = v_str({});\n", c_string(s)))
            }
            Node::Sym(name, _) => {
                let expr = match name.as_str() {
                    "true" => "v_bool(1)",
                    "false" => "v_bool(0)",
                    "nil" => "v_nil()",
                    _ => {
                        self.emit_code(&format!(
                            "{target} = v_sym({});\n",
                            c_string(name)
                        ));
                        return;
                    }
                };
                self.emit_code(&format!("{target} = {expr};\n"));
            }
            Node::List(items, _) => {
                if items.is_empty() {
                    self.emit_code(&format!("{target} = v_list_empty();\n"));
                    return;
                }
                let mut item_tmps = Vec::new();
                for item in items {
                    let tmp = self.fresh();
                    self.gen_quote_into(item, &tmp);
                    item_tmps.push(tmp);
                }
                let n = item_tmps.len();
                for tmp in &item_tmps {
                    self.emit_code(&format!("v_ref(&{tmp});\n"));
                }
                let arr = self.fresh_arr();
                self.emit_code(&format!(
                    "Value {arr}[{n}] = {{ {} }};\n",
                    item_tmps.join(", ")
                ));
                self.emit_code(&format!(
                    "{target} = v_list_from_array({arr}, {n});\n"
                ));
                for tmp in &item_tmps {
                    self.emit_code(&format!("v_unref(&{tmp});\n"));
                }
            }
        }
    }

    fn gen_and(&mut self, items: &[Node], env: &str) -> String {
        let t = self.fresh();
        let label = format!("and_done_{t}");
        self.emit_code(&format!("Value {t} = v_bool(1);\n"));
        for a in &items[1..] {
            let tmp = self.gen_expr(a, env);
            self.emit_code(&format!("if (!v_truthy(&{tmp})) {{\n"));
            self.emit_code(&format!("  v_unref(&{t});\n"));
            self.emit_code(&format!("  {t} = {tmp};\n"));
            self.emit_code(&format!("  v_ref(&{t});\n"));
            self.emit_code(&format!("  v_unref(&{tmp});\n"));
            self.emit_code(&format!("  goto {label};\n"));
            self.emit_code("}\n");
            self.emit_code(&format!("v_unref(&{tmp});\n"));
        }
        self.emit_code(&format!("{label}:\n"));
        t
    }

    fn gen_or(&mut self, items: &[Node], env: &str) -> String {
        let t = self.fresh();
        let label = format!("or_done_{t}");
        self.emit_code(&format!("Value {t} = v_bool(0);\n"));
        for a in &items[1..] {
            let tmp = self.gen_expr(a, env);
            self.emit_code(&format!("if (v_truthy(&{tmp})) {{\n"));
            self.emit_code(&format!("  v_unref(&{t});\n"));
            self.emit_code(&format!("  {t} = {tmp};\n"));
            self.emit_code(&format!("  v_ref(&{t});\n"));
            self.emit_code(&format!("  v_unref(&{tmp});\n"));
            self.emit_code(&format!("  goto {label};\n"));
            self.emit_code("}\n");
            self.emit_code(&format!("v_unref(&{tmp});\n"));
        }
        self.emit_code(&format!("{label}:\n"));
        t
    }

    fn emit_main(&mut self, main_code: &str, n_globals: usize) {
        self.emit("int main(void) {\n");
        self.emit("  steps_init();\n");
        self.emit(&format!(
            "  for (int i = 0; i < {n_globals}; i++) {{ Value v; v.tag = V_NIL; g_top[i] = v; }}\n"
        ));
        self.emit("  Scope *g_env = scope_new(NULL);\n");
        self.emit("  scope_install_prelude(g_env);\n");
        self.emit(
            "  if (g_err) { fprintf(stderr, \"%s\\n\", g_errmsg); return 1; }\n",
        );
        self.emit(main_code);
        self.emit(
            "  if (g_err) { fprintf(stderr, \"%s\\n\", g_errmsg); return 1; }\n",
        );
        self.emit("  scope_unref(g_env);\n");
        self.emit("  return 0;\n");
        self.emit("}\n");
    }
}

fn c_string(s: &str) -> String {
    format!("\"{}\"", c_escape(s))
}

/// If `op` is a known 2-arg operator, return the tight runtime inline to emit
/// directly (bypassing v_call dispatch). None otherwise.
fn two_arg_inline(op: &str) -> Option<&'static str> {
    Some(match op {
        "+" => "a_add",
        "-" => "a_sub2",
        "*" => "a_mul",
        "/" => "a_div",
        "=" => "a_eq2",
        "<" => "a_lt",
        ">" => "a_gt",
        "<=" => "a_le",
        ">=" => "a_ge",
        "mod" => "a_mod",
        _ => return None,
    })
}

fn c_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}
