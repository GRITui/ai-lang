//! Environment + tree-walking evaluator.

use crate::error::{Error, Result};
use crate::parser::Node;
use crate::value::{Closure, ConsCell, Value};
use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

// ---- resource limits --------------------------------------------------------
//
// The tree-walking evaluator recurses through native Rust call frames with no
// inherent bound, and `while` has no iteration cap. Both are reachable from a
// single AINL source form (deep recursion, deeply nested calls, `(while true
// ...)`), and a Rust stack overflow is not a catchable `Result` — it aborts
// the process outright. These two thread-local counters turn both failure
// modes into a clean `Error::Runtime` instead.

thread_local! {
    static DEPTH: Cell<usize> = const { Cell::new(0) };
    static STEPS: Cell<u64> = const { Cell::new(0) };
}

/// Max live `eval` call frames (recursion + nested-expression depth combined).
/// Kept well below where a native stack overflow could occur even on a
/// constrained thread stack (e.g. `cargo test`'s worker threads default to a
/// couple MiB, smaller than a typical main-thread stack).
pub(crate) const MAX_DEPTH: usize = 512;
/// Max total `eval` invocations per top-level `run_str`/`run_in` call — bounds
/// unbounded loops (`while true`) and runaway iteration generally.
pub(crate) const MAX_STEPS: u64 = 2_000_000;

/// Reset the step budget for a fresh top-level run. Depth is guaranteed back
/// at 0 between runs (the RAII guard below always decrements on the way out,
/// success or error), so only steps need an explicit reset.
pub(crate) fn reset_limits() {
    STEPS.with(|s| s.set(0));
}

struct DepthGuard;

impl DepthGuard {
    fn enter() -> Result<DepthGuard> {
        let exceeded = DEPTH.with(|d| {
            let v = d.get() + 1;
            d.set(v);
            v > MAX_DEPTH
        });
        if exceeded {
            DEPTH.with(|d| d.set(d.get() - 1));
            return Err(Error::runtime(format!(
                "recursion limit exceeded (max depth {MAX_DEPTH})"
            )));
        }
        Ok(DepthGuard)
    }
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get() - 1));
    }
}

fn tick() -> Result<()> {
    let exceeded = STEPS.with(|s| {
        let v = s.get() + 1;
        s.set(v);
        v > MAX_STEPS
    });
    if exceeded {
        return Err(Error::runtime(format!(
            "step limit exceeded (max {MAX_STEPS} evaluation steps) — likely an infinite loop or runaway recursion"
        )));
    }
    Ok(())
}

/// A lexical scope with an optional parent. `Env` is a cheap `Rc` handle so
/// closures can share and outlive the scope that created them.
#[derive(Clone)]
pub struct Env(Rc<Scope>);

struct Scope {
    vars: RefCell<HashMap<String, Value>>,
    parent: Option<Env>,
}

#[cfg(test)]
thread_local! {
    /// Count of currently-live `Scope` allocations, for whiteboxing the
    /// cycle-breaking fix below — see `tests::self_referential_closure_does_not_leak`.
    static LIVE_SCOPES: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
impl Drop for Scope {
    fn drop(&mut self) {
        LIVE_SCOPES.with(|c| c.set(c.get() - 1));
    }
}

impl Env {
    pub fn new() -> Env {
        #[cfg(test)]
        LIVE_SCOPES.with(|c| c.set(c.get() + 1));
        Env(Rc::new(Scope {
            vars: RefCell::new(HashMap::new()),
            parent: None,
        }))
    }

    pub fn child(&self) -> Env {
        #[cfg(test)]
        LIVE_SCOPES.with(|c| c.set(c.get() + 1));
        Env(Rc::new(Scope {
            vars: RefCell::new(HashMap::new()),
            parent: Some(self.clone()),
        }))
    }

    pub fn define(&self, name: impl Into<String>, val: Value) {
        self.0.vars.borrow_mut().insert(name.into(), val);
    }

    pub fn get(&self, name: &str) -> Option<Value> {
        if let Some(v) = self.0.vars.borrow().get(name) {
            return Some(v.clone());
        }
        self.0.parent.as_ref().and_then(|p| p.get(name))
    }

    /// A fresh global environment with all builtins bound.
    pub fn with_prelude() -> Env {
        let env = Env::new();
        crate::eval::install_prelude(&env);
        env
    }

    /// True if `self` is `other` or one of `other`'s ancestors — i.e.
    /// something reachable through `other`'s parent chain still needs
    /// `self`'s bindings.
    fn is_ancestor_of(&self, other: &Env) -> bool {
        let mut cur = other.clone();
        loop {
            if Rc::ptr_eq(&self.0, &cur.0) {
                return true;
            }
            match cur.0.parent.clone() {
                Some(p) => cur = p,
                None => return false,
            }
        }
    }

    /// Drop all bindings in this scope, breaking any `Rc` cycle rooted here.
    ///
    /// A closure that re-`def`s itself into its own call scope (the common
    /// named-recursive-function pattern) makes that scope's `vars` map hold
    /// a `Closure` whose `env` points right back at the scope — an `Rc`
    /// cycle that never frees on its own. Called only once a call/`let`
    /// scope's body has finished evaluating and nothing in its result still
    /// needs it (see `apply` and `sf_let`), so this never removes bindings a
    /// live closure could still look up.
    pub(crate) fn clear(&self) {
        self.0.vars.borrow_mut().clear();
    }
}

/// True if some closure reachable from `val` (directly, or nested inside a
/// list) was defined in `env` or in a scope that has `env` as an ancestor —
/// i.e. `env`'s bindings are still needed by something the caller now holds.
pub(crate) fn value_keeps_env_alive(val: &Value, env: &Env) -> bool {
    match val {
        Value::Closure(c) => env.is_ancestor_of(&c.env),
        Value::List(items) => {
            let mut cur = items;
            loop {
                match cur.first() {
                    Some(v) => {
                        if value_keeps_env_alive(v, env) {
                            return true;
                        }
                        match cur.rest() {
                            Some(next) => cur = next,
                            None => return false,
                        }
                    }
                    None => return false,
                }
            }
        }
        Value::Map(pairs) => pairs
            .iter()
            .any(|(k, v)| value_keeps_env_alive(k, env) || value_keeps_env_alive(v, env)),
        _ => false,
    }
}

impl Default for Env {
    fn default() -> Self {
        Env::new()
    }
}

pub fn eval(node: &Node, env: &Env) -> Result<Value> {
    let _guard = DepthGuard::enter()?;
    tick()?;
    match node {
        Node::Int(i, _) => Ok(Value::Int(*i)),
        Node::Float(x, _) => Ok(Value::Float(*x)),
        Node::Str(s, _) => Ok(Value::str(s.clone())),
        Node::Sym(name, _) => match name.as_str() {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            "nil" => Ok(Value::Nil),
            _ => env
                .get(name)
                .ok_or_else(|| Error::runtime(format!("unbound symbol '{name}'"))),
        },
        Node::List(items, _) => eval_list(items, env),
    }
}

fn eval_list(items: &[Node], env: &Env) -> Result<Value> {
    let Some(head) = items.first() else {
        // empty list evaluates to nil
        return Ok(Value::Nil);
    };

    // Special forms are dispatched by the head symbol before evaluating args.
    if let Node::Sym(op, _) = head {
        match op.as_str() {
            "def" => return sf_def(&items[1..], env),
            "fn" => return sf_fn(&items[1..], env),
            "if" => return sf_if(&items[1..], env),
            "do" => return sf_do(&items[1..], env),
            "let" => return sf_let(&items[1..], env),
            "while" => return sf_while(&items[1..], env),
            "quote" => return sf_quote(&items[1..]),
            "and" => return sf_and(&items[1..], env),
            "or" => return sf_or(&items[1..], env),
            _ => {}
        }
    }

    // Otherwise it's a call: evaluate head + args, then apply.
    let callee = eval(head, env)?;
    let mut args = Vec::with_capacity(items.len() - 1);
    for a in &items[1..] {
        args.push(eval(a, env)?);
    }
    apply(callee, &args)
}

pub fn apply(callee: Value, args: &[Value]) -> Result<Value> {
    match callee {
        Value::Builtin { f, .. } => f(args),
        Value::Closure(clos) => {
            let call_env = clos.env.child();
            let np = clos.params.len();
            if clos.variadic.is_some() {
                if args.len() < np {
                    return Err(Error::runtime(format!(
                        "fn expects at least {np} args, got {}",
                        args.len()
                    )));
                }
            } else if args.len() != np {
                return Err(Error::runtime(format!(
                    "fn expects {np} args, got {}",
                    args.len()
                )));
            }
            for (name, val) in clos.params.iter().zip(args.iter()) {
                call_env.define(name.clone(), val.clone());
            }
            if let Some(rest) = &clos.variadic {
                let extra: Vec<Value> = args[np..].to_vec();
                call_env.define(rest.clone(), Value::List(ConsCell::from_values(extra)));
            }
            let mut last = Value::Nil;
            let mut err = None;
            for form in &clos.body {
                match eval(form, &call_env) {
                    Ok(v) => last = v,
                    Err(e) => {
                        err = Some(e);
                        break;
                    }
                }
            }
            // Break a self-referential-def cycle rooted in this call's own
            // scope, unless the result still needs it (a closure escaped
            // that was defined in — or under — call_env).
            if err.is_some() || !value_keeps_env_alive(&last, &call_env) {
                call_env.clear();
            }
            match err {
                Some(e) => Err(e),
                None => Ok(last),
            }
        }
        other => Err(Error::runtime(format!(
            "cannot call a {}",
            other.type_name()
        ))),
    }
}

// ---- special forms ---------------------------------------------------------

fn sf_def(args: &[Node], env: &Env) -> Result<Value> {
    let [name_node, val_node] = args else {
        return Err(Error::runtime("def expects (def name value)"));
    };
    let Node::Sym(name, _) = name_node else {
        return Err(Error::runtime("def name must be a symbol"));
    };
    let val = eval(val_node, env)?;
    env.define(name.clone(), val);
    Ok(Value::Sym(Rc::new(name.clone())))
}

fn sf_fn(args: &[Node], env: &Env) -> Result<Value> {
    let Some((params_node, body)) = args.split_first() else {
        return Err(Error::runtime("fn expects (fn (params...) body...)"));
    };
    let Node::List(param_nodes, _) = params_node else {
        return Err(Error::runtime("fn params must be a list"));
    };
    let mut params = Vec::new();
    let mut variadic = None;
    let mut i = 0;
    while i < param_nodes.len() {
        let Node::Sym(p, _) = &param_nodes[i] else {
            return Err(Error::runtime("fn params must be symbols"));
        };
        if p == "&" {
            let Some(Node::Sym(rest, _)) = param_nodes.get(i + 1) else {
                return Err(Error::runtime("'&' must be followed by a rest parameter"));
            };
            variadic = Some(rest.clone());
            break;
        }
        params.push(p.clone());
        i += 1;
    }
    Ok(Value::Closure(Rc::new(Closure {
        params,
        variadic,
        body: body.to_vec(),
        env: env.clone(),
        code: None,
    })))
}

fn sf_if(args: &[Node], env: &Env) -> Result<Value> {
    match args {
        [cond, then] => {
            if eval(cond, env)?.is_truthy() {
                eval(then, env)
            } else {
                Ok(Value::Nil)
            }
        }
        [cond, then, els] => {
            if eval(cond, env)?.is_truthy() {
                eval(then, env)
            } else {
                eval(els, env)
            }
        }
        _ => Err(Error::runtime("if expects (if cond then [else])")),
    }
}

fn sf_do(args: &[Node], env: &Env) -> Result<Value> {
    let mut last = Value::Nil;
    for form in args {
        last = eval(form, env)?;
    }
    Ok(last)
}

fn sf_let(args: &[Node], env: &Env) -> Result<Value> {
    let Some((binds_node, body)) = args.split_first() else {
        return Err(Error::runtime("let expects (let ((n v)...) body...)"));
    };
    let Node::List(binds, _) = binds_node else {
        return Err(Error::runtime("let bindings must be a list"));
    };
    let scope = env.child();
    let result = sf_let_body(binds, body, &scope);
    // Break a self-referential-def cycle rooted in this let's own scope
    // (e.g. `(let () (def loop (fn () (loop))) ...)`), unless the result
    // still needs it (a closure escaped that was defined in — or under —
    // scope).
    let last = match &result {
        Ok(v) => v,
        Err(_) => &Value::Nil,
    };
    if result.is_err() || !value_keeps_env_alive(last, &scope) {
        scope.clear();
    }
    result
}

fn sf_let_body(binds: &[Node], body: &[Node], scope: &Env) -> Result<Value> {
    for b in binds {
        let Node::List(pair, _) = b else {
            return Err(Error::runtime("each let binding must be (name value)"));
        };
        let [Node::Sym(name, _), val_node] = &pair[..] else {
            return Err(Error::runtime("each let binding must be (name value)"));
        };
        let val = eval(val_node, scope)?;
        scope.define(name.clone(), val);
    }
    let mut last = Value::Nil;
    for form in body {
        last = eval(form, scope)?;
    }
    Ok(last)
}

fn sf_while(args: &[Node], env: &Env) -> Result<Value> {
    let Some((cond, body)) = args.split_first() else {
        return Err(Error::runtime("while expects (while cond body...)"));
    };
    let mut last = Value::Nil;
    while eval(cond, env)?.is_truthy() {
        for form in body {
            last = eval(form, env)?;
        }
    }
    Ok(last)
}

fn sf_quote(args: &[Node]) -> Result<Value> {
    let [node] = args else {
        return Err(Error::runtime("quote expects one form"));
    };
    Ok(quote_node(node))
}

fn quote_node(node: &Node) -> Value {
    match node {
        Node::Int(i, _) => Value::Int(*i),
        Node::Float(x, _) => Value::Float(*x),
        Node::Str(s, _) => Value::str(s.clone()),
        Node::Sym(s, _) => Value::Sym(Rc::new(s.clone())),
        Node::List(items, _) => Value::List(ConsCell::from_values(items.iter().map(quote_node))),
    }
}

fn sf_and(args: &[Node], env: &Env) -> Result<Value> {
    let mut last = Value::Bool(true);
    for a in args {
        last = eval(a, env)?;
        if !last.is_truthy() {
            return Ok(last);
        }
    }
    Ok(last)
}

fn sf_or(args: &[Node], env: &Env) -> Result<Value> {
    for a in args {
        let v = eval(a, env)?;
        if v.is_truthy() {
            return Ok(v);
        }
    }
    Ok(Value::Bool(false))
}

// ---- builtins --------------------------------------------------------------

fn install_prelude(env: &Env) {
    macro_rules! b {
        ($name:literal, $f:expr) => {
            env.define($name, Value::Builtin { name: $name, f: $f });
        };
    }

    b!("+", |a| numeric_fold(
        a,
        0.0,
        0,
        |x, y| x + y,
        |x, y| x.checked_add(y)
    ));
    b!("*", |a| numeric_fold(
        a,
        1.0,
        1,
        |x, y| x * y,
        |x, y| x.checked_mul(y)
    ));
    b!("-", builtin_sub);
    b!("/", builtin_div);
    b!("=", |a| Ok(Value::Bool(a.windows(2).all(|w| w[0] == w[1]))));
    b!("<", |a| compare(a, |o| o == std::cmp::Ordering::Less));
    b!(">", |a| compare(a, |o| o == std::cmp::Ordering::Greater));
    b!("<=", |a| compare(a, |o| o != std::cmp::Ordering::Greater));
    b!(">=", |a| compare(a, |o| o != std::cmp::Ordering::Less));
    b!("not", |a| Ok(Value::Bool(!arg1(a)?.is_truthy())));
    b!("mod", builtin_mod);

    b!("print", |a| {
        let parts: Vec<String> = a.iter().map(|v| v.to_string()).collect();
        println!("{}", parts.join(" "));
        Ok(Value::Nil)
    });
    b!("str", |a| {
        let s: String = a.iter().map(|v| v.to_string()).collect();
        Ok(Value::str(s))
    });

    b!("list", |a| Ok(Value::List(ConsCell::from_values(
        a.to_vec()
    ))));
    b!("len", builtin_len);
    b!("first", builtin_first);
    b!("rest", builtin_rest);
    b!("nth", builtin_nth);
    b!("cons", builtin_cons);
    b!("push", builtin_push);

    b!("hash", builtin_hash);
    b!("get", builtin_get);
    b!("assoc", builtin_assoc);
    b!("has", builtin_has);
    b!("keys", builtin_keys);
    b!("vals", builtin_vals);

    b!("error", |a| Err(Error::runtime(
        a.iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    )));

    // ---- stdlib (Stage 3.1) -----------------------------------------------
    //
    // Every builtin here has a byte-for-byte twin in the AOT C runtime
    // (crates/ainl-cc/src/runtime.c) and a per-target mapping in all three
    // transpilers. Two deliberate design rules keep the four backends
    // provably identical rather than merely similar:
    //
    // * ASCII-only case folding and whitespace trimming. The C runtime has no
    //   locale-independent `toupper` guarantee and Python/JS/Ruby each strip a
    //   *different* Unicode whitespace set, so `upcase`/`downcase`/`trim`
    //   operate on this explicit ASCII set in all four. See `is_ascii_ws`.
    // * No backend is allowed to invent a value where another errors. So the
    //   cases the three host languages disagree on — an empty `split`
    //   separator, an empty `replace` target, a negative `sqrt`/`sleep` — are
    //   *rejected* with one shared message instead of returning a
    //   backend-specific result.
    install_stdlib(env);
}

/// Bind the Stage 3.1 standard library (file I/O, strings, env/process, time,
/// math) into `env`. Split out of [`install_prelude`] so the prelude's original
/// 27 core builtins stay readable as one block.
fn install_stdlib(env: &Env) {
    macro_rules! b {
        ($name:literal, $f:expr) => {
            env.define($name, Value::Builtin { name: $name, f: $f });
        };
    }

    // file I/O
    b!("read-file", builtin_read_file);
    b!("write-file", builtin_write_file);
    b!("append-file", builtin_append_file);

    // strings
    b!("split", builtin_split);
    b!("join", builtin_join);
    b!("trim", builtin_trim);
    b!("replace", builtin_replace);
    b!("upcase", |a| builtin_case(a, true));
    b!("downcase", |a| builtin_case(a, false));
    b!("contains", builtin_contains);

    // env / process
    b!("env-get", builtin_env_get);
    b!("exit", builtin_exit);

    // time
    b!("now", builtin_now);
    b!("sleep", builtin_sleep);

    // math
    b!("abs", builtin_abs);
    b!("min", |a| builtin_minmax(a, false));
    b!("max", |a| builtin_minmax(a, true));
    b!("floor", builtin_floor);
    b!("sqrt", builtin_sqrt);
}

// ---- stdlib: shared argument coercion --------------------------------------

/// The characters `trim` strips: ASCII space, tab, LF, CR, FF, VT.
///
/// Deliberately *not* `char::is_whitespace` (which also accepts U+00A0, U+2028,
/// …) and deliberately locale-independent in C. Ruby/JS/Python each strip a
/// different Unicode set, so pinning the interpreter to ASCII is what lets the
/// three transpiler targets match it exactly. The transpilers use the same
/// six-character class.
fn is_ascii_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0C' | '\x0B')
}

fn as_str_arg<'a>(v: &'a Value, who: &str) -> Result<&'a str> {
    match v {
        Value::Str(s) => Ok(s),
        other => Err(Error::runtime(format!(
            "{who} expects a str, got {}",
            other.type_name()
        ))),
    }
}

/// `(read-file path)` / `(write-file path content)` / … — the path operand,
/// with the "a str path" wording the file builtins share.
fn as_path_arg<'a>(v: &'a Value, who: &str) -> Result<&'a str> {
    match v {
        Value::Str(s) => Ok(s),
        other => Err(Error::runtime(format!(
            "{who} expects a str path, got {}",
            other.type_name()
        ))),
    }
}

/// The content operand of the two writing file builtins.
fn as_content_arg<'a>(v: &'a Value, who: &str) -> Result<&'a str> {
    match v {
        Value::Str(s) => Ok(s),
        other => Err(Error::runtime(format!(
            "{who} expects str content, got {}",
            other.type_name()
        ))),
    }
}

/// The numeric operand of the stdlib math builtins, reported under the builtin's
/// own name (`"sqrt expects a number, got str"`) rather than `as_f64`'s generic
/// wording — a new builtin should name itself in its own error.
fn as_num_arg(v: &Value, who: &str) -> Result<f64> {
    match v {
        Value::Int(i) => Ok(*i as f64),
        Value::Float(x) => Ok(*x),
        other => Err(Error::runtime(format!(
            "{who} expects a number, got {}",
            other.type_name()
        ))),
    }
}

// ---- stdlib: file I/O ------------------------------------------------------

/// `(read-file path)` → the file's contents as one string.
///
/// A missing or unreadable file is a runtime error, not a nil result: a script
/// that silently reads "" from a typo'd path is the classic way to waste an
/// afternoon. The C runtime raises the same message, and the transpiler targets
/// raise their host's own file exception (documented in docs/SYNTAX.md §3).
fn builtin_read_file(args: &[Value]) -> Result<Value> {
    let [p] = args else {
        return Err(Error::runtime("read-file expects (read-file path)"));
    };
    let path = as_path_arg(p, "read-file")?;
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Value::str(s)),
        Err(_) => Err(Error::runtime(format!("read-file: cannot read '{path}'"))),
    }
}

/// `(write-file path content)` / `(append-file path content)` → `nil`.
///
/// One implementation for both: the only difference is the `OpenOptions` mode
/// and the verb the error message uses.
fn write_file_inner(args: &[Value], who: &str, verb: &str, append: bool) -> Result<Value> {
    let [p, c] = args else {
        return Err(Error::runtime(format!(
            "{who} expects ({who} path content)"
        )));
    };
    let path = as_path_arg(p, who)?;
    let content = as_content_arg(c, who)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true);
    if append {
        opts.append(true);
    } else {
        opts.truncate(true);
    }
    match opts.open(path) {
        Err(_) => Err(Error::runtime(format!("{who}: cannot {verb} '{path}'"))),
        Ok(mut f) => match std::io::Write::write_all(&mut f, content.as_bytes()) {
            Err(_) => Err(Error::runtime(format!("{who}: cannot {verb} '{path}'"))),
            Ok(()) => Ok(Value::Nil),
        },
    }
}

fn builtin_write_file(args: &[Value]) -> Result<Value> {
    write_file_inner(args, "write-file", "write", false)
}

fn builtin_append_file(args: &[Value]) -> Result<Value> {
    write_file_inner(args, "append-file", "append to", true)
}

// ---- stdlib: strings -------------------------------------------------------

/// `(split str separator)` → list of strings.
///
/// An empty separator is rejected rather than defined: Python raises, JS splits
/// into characters, and Ruby raises — three answers for one call. Erroring is
/// the only choice the four backends can share.
fn builtin_split(args: &[Value]) -> Result<Value> {
    let [s, sep] = args else {
        return Err(Error::runtime("split expects (split str separator)"));
    };
    let s = as_str_arg(s, "split")?;
    let sep = as_str_arg(sep, "split")?;
    if sep.is_empty() {
        return Err(Error::runtime("split expects a non-empty separator"));
    }
    let parts: Vec<Value> = s.split(sep).map(Value::str).collect();
    Ok(Value::List(ConsCell::from_values(parts)))
}

/// `(join list separator)` → the elements concatenated, `separator` between
/// each pair. Every element must be a string, again so that a non-string
/// element is a defined error instead of JS's silent `"1,2"` coercion.
fn builtin_join(args: &[Value]) -> Result<Value> {
    let [lst, sep] = args else {
        return Err(Error::runtime("join expects (join list separator)"));
    };
    let Value::List(l) = lst else {
        return Err(Error::runtime(format!(
            "join expects a list, got {}",
            lst.type_name()
        )));
    };
    let sep = match sep {
        Value::Str(s) => s,
        other => {
            return Err(Error::runtime(format!(
                "join expects a str separator, got {}",
                other.type_name()
            )))
        }
    };
    let mut out = String::new();
    let mut cur = l;
    let mut first = true;
    while let Some(h) = cur.first() {
        let Value::Str(s) = h else {
            return Err(Error::runtime("join expects a list of str"));
        };
        if !first {
            out.push_str(sep);
        }
        first = false;
        out.push_str(s);
        match cur.rest() {
            Some(next) => cur = next,
            None => break,
        }
    }
    Ok(Value::str(out))
}

/// `(trim str)` — strip leading and trailing ASCII whitespace (see
/// [`is_ascii_ws`]).
fn builtin_trim(args: &[Value]) -> Result<Value> {
    let [s] = args else {
        return Err(Error::runtime("trim expects (trim str)"));
    };
    let s = as_str_arg(s, "trim")?;
    Ok(Value::str(s.trim_matches(is_ascii_ws)))
}

/// `(replace str old new)` — every non-overlapping occurrence of `old`.
///
/// An empty `old` is rejected: Python inserts at every position, JS and Ruby
/// return the string unchanged, and the C runtime would need its own choice.
/// The four backends agree by refusing.
fn builtin_replace(args: &[Value]) -> Result<Value> {
    let [s, old, new] = args else {
        return Err(Error::runtime("replace expects (replace str old new)"));
    };
    let s = as_str_arg(s, "replace")?;
    let old = as_str_arg(old, "replace")?;
    let new = as_str_arg(new, "replace")?;
    if old.is_empty() {
        return Err(Error::runtime("replace expects a non-empty target"));
    }
    Ok(Value::str(s.replace(old, new)))
}

/// `(upcase str)` / `(downcase str)` — ASCII case folding only (see
/// [`is_ascii_ws`] for why).
fn builtin_case(args: &[Value], up: bool) -> Result<Value> {
    let who = if up { "upcase" } else { "downcase" };
    let [s] = args else {
        return Err(Error::runtime(format!("{who} expects ({who} str)")));
    };
    let s = as_str_arg(s, who)?;
    Ok(Value::str(if up {
        s.to_ascii_uppercase()
    } else {
        s.to_ascii_lowercase()
    }))
}

/// `(contains haystack needle)` → bool. An empty needle is `true`, matching
/// every host language.
fn builtin_contains(args: &[Value]) -> Result<Value> {
    let [hay, needle] = args else {
        return Err(Error::runtime("contains expects (contains str sub)"));
    };
    let hay = as_str_arg(hay, "contains")?;
    let needle = as_str_arg(needle, "contains")?;
    Ok(Value::Bool(hay.contains(needle)))
}

// ---- stdlib: env / process -------------------------------------------------

/// `(env-get name)` → the variable's value, or `nil` when unset. `nil` (rather
/// than an error) is what makes `(if (env-get "X") ...)` usable.
fn builtin_env_get(args: &[Value]) -> Result<Value> {
    let [name] = args else {
        return Err(Error::runtime("env-get expects (env-get name)"));
    };
    let name = as_str_arg(name, "env-get")?;
    Ok(match std::env::var(name) {
        Ok(v) => Value::str(v),
        Err(_) => Value::Nil,
    })
}

/// `(exit code)` — terminate the process with `code`. Does not return.
///
/// Not callable from a library context (it ends the process), so it is tested
/// through a subprocess rather than in-process. stdout is flushed first:
/// `process::exit` skips destructors, and while Rust's `Stdout` is a
/// `LineWriter` (so whole lines are already out), an explicit flush keeps the
/// guarantee local to this function instead of resting on that detail.
fn builtin_exit(args: &[Value]) -> Result<Value> {
    let [code] = args else {
        return Err(Error::runtime("exit expects (exit code)"));
    };
    let Value::Int(i) = code else {
        return Err(Error::runtime(format!(
            "exit expects an int, got {}",
            code.type_name()
        )));
    };
    use std::io::Write;
    let _ = std::io::stdout().flush();
    std::process::exit(*i as i32);
}

// ---- stdlib: time ----------------------------------------------------------

/// `(now)` → whole seconds since the Unix epoch.
///
/// Seconds (not milliseconds) keeps the value inside AINL's i64 range for the
/// next ~292 billion years, and matches what the three target runtimes' own
/// idioms return.
fn builtin_now(args: &[Value]) -> Result<Value> {
    if !args.is_empty() {
        return Err(Error::runtime("now expects (now)"));
    }
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Ok(Value::Int(secs))
}

/// `(sleep seconds)` → `nil`. Negative and NaN are rejected so all four
/// backends agree (Python raises on negative, JS ignores it, Ruby raises).
fn builtin_sleep(args: &[Value]) -> Result<Value> {
    let [s] = args else {
        return Err(Error::runtime("sleep expects (sleep seconds)"));
    };
    let secs = as_num_arg(s, "sleep")?;
    // NaN is rejected too: it is neither >= 0 nor < 0, and a "sleep that never
    // sleeps" is exactly the silent-wrong-answer case this check prevents.
    if secs.is_nan() || secs < 0.0 {
        return Err(Error::runtime("sleep expects a non-negative number"));
    }
    if secs > 0.0 {
        use std::thread::sleep;
        use std::time::Duration;
        // Cap each nap at a year so an absurdly large request (reachable as
        // `(sleep 1e300)`) still sleeps instead of overflowing Duration's
        // ~584-year ceiling, and so the loop always makes progress.
        const MAX_NAP: f64 = 31_536_000.0;
        let mut left = secs;
        while left > 0.0 {
            let chunk = if left > MAX_NAP { MAX_NAP } else { left };
            if let Ok(d) = Duration::try_from_secs_f64(chunk) {
                sleep(d);
            }
            left -= chunk;
        }
    }
    Ok(Value::Nil)
}

// ---- stdlib: math ----------------------------------------------------------

/// `(abs n)` — integer-preserving, promoting to float only where i64 cannot
/// represent the answer (`abs` of i64::MIN), exactly like unary `-`.
fn builtin_abs(args: &[Value]) -> Result<Value> {
    let [n] = args else {
        return Err(Error::runtime("abs expects (abs n)"));
    };
    match n {
        Value::Int(i) => match i.checked_abs() {
            Some(r) => Ok(Value::Int(r)),
            None => Ok(Value::Float((*i as f64).abs())),
        },
        Value::Float(x) => Ok(Value::Float(x.abs())),
        other => Err(Error::runtime(format!(
            "abs expects a number, got {}",
            other.type_name()
        ))),
    }
}

/// `(min a b ...)` / `(max a b ...)` — variadic, at least one argument, folding
/// pairwise. Ties keep the *first* of the equal values (the `<`/`>` comparison
/// is strict), which is what `min`/`max` do in Python, Ruby and `Math.min`/
/// `Math.max` too.
fn builtin_minmax(args: &[Value], max: bool) -> Result<Value> {
    let who = if max { "max" } else { "min" };
    let Some(first) = args.first() else {
        return Err(Error::runtime(format!("{who} expects at least 1 argument")));
    };
    let mut best = first.clone();
    let mut b = as_num_arg(&best, who)?;
    for v in &args[1..] {
        let x = as_num_arg(v, who)?;
        if if max { x > b } else { x < b } {
            best = v.clone();
            b = x;
        }
    }
    Ok(best)
}

/// `(floor n)` → an int, like Python's `math.floor` and Ruby's `Float#floor`.
/// An int argument is returned unchanged (no float round-trip, so no precision
/// surprise on large i64s). A float that floors outside i64 range stays a float.
fn builtin_floor(args: &[Value]) -> Result<Value> {
    let [n] = args else {
        return Err(Error::runtime("floor expects (floor n)"));
    };
    if let Value::Int(i) = n {
        return Ok(Value::Int(*i));
    }
    let x = as_num_arg(n, "floor")?;
    let f = x.floor();
    if (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&f) {
        Ok(Value::Int(f as i64))
    } else {
        Ok(Value::Float(f))
    }
}

/// `(sqrt n)` → a float, always. A negative argument is an error rather than
/// the NaN Python/Ruby would raise and JS/AOT would quietly return.
fn builtin_sqrt(args: &[Value]) -> Result<Value> {
    let [n] = args else {
        return Err(Error::runtime("sqrt expects (sqrt n)"));
    };
    let x = as_num_arg(n, "sqrt")?;
    if x < 0.0 {
        return Err(Error::runtime("sqrt expects a non-negative number"));
    }
    Ok(Value::Float(x.sqrt()))
}

fn arg1(a: &[Value]) -> Result<&Value> {
    a.first()
        .ok_or_else(|| Error::runtime("expected 1 argument"))
}

pub(crate) fn as_f64(v: &Value) -> Result<f64> {
    match v {
        Value::Int(i) => Ok(*i as f64),
        Value::Float(x) => Ok(*x),
        other => Err(Error::runtime(format!(
            "expected a number, got {}",
            other.type_name()
        ))),
    }
}

/// Fold numeric args, staying in integer arithmetic until a float appears (or
/// an integer op overflows), matching typical dynamic-language semantics.
pub(crate) fn numeric_fold(
    args: &[Value],
    _f_id: f64,
    i_id: i64,
    ff: fn(f64, f64) -> f64,
    fi: fn(i64, i64) -> Option<i64>,
) -> Result<Value> {
    let mut acc_i = i_id;
    let mut acc_f = 0.0f64;
    let mut is_float = false;
    let mut first = true;
    for v in args {
        match v {
            Value::Int(i) if !is_float => match fi(acc_i, *i) {
                Some(r) => acc_i = r,
                None => {
                    is_float = true;
                    acc_f = ff(acc_i as f64, *i as f64);
                }
            },
            _ => {
                let x = as_f64(v)?;
                if !is_float {
                    is_float = true;
                    acc_f = acc_i as f64;
                }
                acc_f = ff(acc_f, x);
            }
        }
        first = false;
    }
    let _ = first;
    if is_float {
        Ok(Value::Float(acc_f))
    } else {
        Ok(Value::Int(acc_i))
    }
}

pub(crate) fn builtin_sub(args: &[Value]) -> Result<Value> {
    match args {
        [] => Err(Error::runtime("- expects at least 1 argument")),
        [one] => match one {
            // i64::MIN has no positive i64 counterpart; checked_neg catches
            // that (rather than silently wrapping in release builds) and we
            // promote to float, matching every other arithmetic op's overflow
            // behavior.
            Value::Int(i) => match i.checked_neg() {
                Some(r) => Ok(Value::Int(r)),
                None => Ok(Value::Float(-(*i as f64))),
            },
            Value::Float(x) => Ok(Value::Float(-*x)),
            other => Err(Error::runtime(format!(
                "- expected number, got {}",
                other.type_name()
            ))),
        },
        [first, rest @ ..] => {
            let mut all_int = matches!(first, Value::Int(_));
            for v in rest {
                all_int &= matches!(v, Value::Int(_));
            }
            if all_int {
                let mut acc = if let Value::Int(i) = first {
                    *i
                } else {
                    unreachable!()
                };
                for v in rest {
                    if let Value::Int(i) = v {
                        match acc.checked_sub(*i) {
                            Some(r) => acc = r,
                            None => return float_sub(first, rest),
                        }
                    }
                }
                Ok(Value::Int(acc))
            } else {
                float_sub(first, rest)
            }
        }
    }
}

fn float_sub(first: &Value, rest: &[Value]) -> Result<Value> {
    let mut acc = as_f64(first)?;
    for v in rest {
        acc -= as_f64(v)?;
    }
    Ok(Value::Float(acc))
}

pub(crate) fn builtin_div(args: &[Value]) -> Result<Value> {
    let [first, rest @ ..] = args else {
        return Err(Error::runtime("/ expects at least 1 argument"));
    };
    let mut acc = as_f64(first)?;
    if rest.is_empty() {
        return Ok(Value::Float(1.0 / acc));
    }
    for v in rest {
        let d = as_f64(v)?;
        if d == 0.0 {
            return Err(Error::runtime("division by zero"));
        }
        acc /= d;
    }
    Ok(Value::Float(acc))
}

pub(crate) fn builtin_mod(args: &[Value]) -> Result<Value> {
    let [Value::Int(a), Value::Int(b)] = args else {
        return Err(Error::runtime("mod expects (mod int int)"));
    };
    if *b == 0 {
        return Err(Error::runtime("mod by zero"));
    }
    // `i64::MIN.rem_euclid(-1)` panics: the *quotient* (i64::MAX + 1) doesn't
    // fit in i64, even though the mathematical remainder of dividing by ±1 is
    // always 0. Special-case it rather than letting the overflow through.
    if *b == -1 {
        return Ok(Value::Int(0));
    }
    Ok(Value::Int(a.rem_euclid(*b)))
}

pub(crate) fn compare(args: &[Value], keep: fn(std::cmp::Ordering) -> bool) -> Result<Value> {
    for w in args.windows(2) {
        let a = as_f64(&w[0])?;
        let b = as_f64(&w[1])?;
        let ord = a
            .partial_cmp(&b)
            .ok_or_else(|| Error::runtime("cannot compare NaN"))?;
        if !keep(ord) {
            return Ok(Value::Bool(false));
        }
    }
    Ok(Value::Bool(true))
}

fn builtin_len(args: &[Value]) -> Result<Value> {
    match arg1(args)? {
        Value::List(l) => Ok(Value::Int(l.len as i64)),
        Value::Str(s) => Ok(Value::Int(s.chars().count() as i64)),
        Value::Map(m) => Ok(Value::Int(m.len() as i64)),
        other => Err(Error::runtime(format!(
            "len expects list, str, or hash, got {}",
            other.type_name()
        ))),
    }
}

/// `(hash k v k v ...)` — build a map from variadic key/value pairs. A
/// repeated key keeps its *last* value and its *first* position, matching
/// the everyday "later assignment wins" expectation.
fn builtin_hash(args: &[Value]) -> Result<Value> {
    if !args.len().is_multiple_of(2) {
        return Err(Error::runtime(format!(
            "hash expects an even number of key/value arguments, got {}",
            args.len()
        )));
    }
    let mut pairs: Vec<(Value, Value)> = Vec::with_capacity(args.len() / 2);
    for i in (0..args.len()).step_by(2) {
        let (k, v) = (args[i].clone(), args[i + 1].clone());
        match pairs.iter_mut().find(|(ek, _)| *ek == k) {
            Some((_, ev)) => *ev = v,
            None => pairs.push((k, v)),
        }
    }
    Ok(Value::Map(Rc::new(pairs)))
}

fn as_map<'a>(v: &'a Value, who: &str) -> Result<&'a Rc<Vec<(Value, Value)>>> {
    match v {
        Value::Map(m) => Ok(m),
        other => Err(Error::runtime(format!(
            "{who} expects a hash, got {}",
            other.type_name()
        ))),
    }
}

/// `(get h k)` — look up a key; `nil` if absent, matching `nth`'s
/// out-of-range convention.
fn builtin_get(args: &[Value]) -> Result<Value> {
    let [h, k] = args else {
        return Err(Error::runtime("get expects (get hash key)"));
    };
    let m = as_map(h, "get")?;
    Ok(m.iter()
        .find(|(ek, _)| ek == k)
        .map(|(_, v)| v.clone())
        .unwrap_or(Value::Nil))
}

/// `(assoc h k v)` — a *new* map with `k` bound to `v`, like `cons`/`push`
/// leaving the original untouched. Updates in place (keeps position) if `k`
/// already exists, else appends.
fn builtin_assoc(args: &[Value]) -> Result<Value> {
    let [h, k, v] = args else {
        return Err(Error::runtime("assoc expects (assoc hash key value)"));
    };
    let m = as_map(h, "assoc")?;
    let mut pairs = (**m).clone();
    match pairs.iter_mut().find(|(ek, _)| ek == k) {
        Some((_, ev)) => *ev = v.clone(),
        None => pairs.push((k.clone(), v.clone())),
    }
    Ok(Value::Map(Rc::new(pairs)))
}

fn builtin_has(args: &[Value]) -> Result<Value> {
    let [h, k] = args else {
        return Err(Error::runtime("has expects (has hash key)"));
    };
    let m = as_map(h, "has")?;
    Ok(Value::Bool(m.iter().any(|(ek, _)| ek == k)))
}

fn builtin_keys(args: &[Value]) -> Result<Value> {
    let m = as_map(arg1(args)?, "keys")?;
    Ok(Value::List(ConsCell::from_values(
        m.iter().map(|(k, _)| k.clone()),
    )))
}

fn builtin_vals(args: &[Value]) -> Result<Value> {
    let m = as_map(arg1(args)?, "vals")?;
    Ok(Value::List(ConsCell::from_values(
        m.iter().map(|(_, v)| v.clone()),
    )))
}

fn builtin_first(args: &[Value]) -> Result<Value> {
    match arg1(args)? {
        Value::List(l) => Ok(l.first().cloned().unwrap_or(Value::Nil)),
        other => Err(Error::runtime(format!(
            "first expects list, got {}",
            other.type_name()
        ))),
    }
}

fn builtin_rest(args: &[Value]) -> Result<Value> {
    match arg1(args)? {
        Value::List(l) => Ok(Value::List(match l.rest() {
            Some(rest) => rest.clone(),
            None => ConsCell::empty(),
        })),
        other => Err(Error::runtime(format!(
            "rest expects list, got {}",
            other.type_name()
        ))),
    }
}

fn builtin_nth(args: &[Value]) -> Result<Value> {
    let [Value::List(l), Value::Int(i)] = args else {
        return Err(Error::runtime("nth expects (nth list int)"));
    };
    if *i < 0 {
        return Ok(Value::Nil);
    }
    Ok(l.nth(*i as usize).cloned().unwrap_or(Value::Nil))
}

fn builtin_cons(args: &[Value]) -> Result<Value> {
    let [head, Value::List(l)] = args else {
        return Err(Error::runtime("cons expects (cons value list)"));
    };
    // O(1): a fresh cell pointing at the existing tail.
    Ok(Value::List(ConsCell::cons(head.clone(), l)))
}

fn builtin_push(args: &[Value]) -> Result<Value> {
    let [Value::List(l), tail @ ..] = args else {
        return Err(Error::runtime("push expects (push list value...)"));
    };
    // O(n + k): one traversal of the existing list, then append every value.
    // (Acceptable — the O(n²) case was repeated `cons`, which is now O(1).)
    let mut items = Vec::with_capacity(l.len + tail.len());
    let mut cur = l;
    while let Some(h) = cur.first() {
        items.push(h.clone());
        match cur.rest() {
            Some(next) => cur = next,
            None => break,
        }
    }
    for v in tail {
        items.push(v.clone());
    }
    Ok(Value::List(ConsCell::from_values(items)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_scopes() -> usize {
        LIVE_SCOPES.with(|c| c.get())
    }

    #[test]
    fn self_referential_closure_does_not_leak_its_call_scope() {
        LIVE_SCOPES.with(|c| c.set(0));
        let env = Env::with_prelude();
        // `helper` re-defines itself into make_counter's own call scope —
        // exactly the Rc-cycle-forming pattern. Called many times in a loop:
        // without the cycle-breaking fix in `apply`, each call leaks its own
        // call scope and never frees it.
        let src = "\
            (def make_counter (fn ()
              (def helper (fn (x) (if (= x 0) 0 (helper (- x 1)))))
              (helper 3)))
            (def i 0)
            (while (< i 500) (make_counter) (def i (+ i 1)))
            ";
        crate::run_in(src, &env).unwrap();
        let live = live_scopes();
        // Only long-lived scopes should remain (the global env + a couple
        // still referenced by `env`/`make_counter`'s own defining scope) —
        // nowhere near the 500+ this would be if every call scope leaked.
        assert!(live < 20, "expected a bounded live-scope count, got {live}");
    }

    #[test]
    fn escaping_closure_keeps_its_captured_scope_alive() {
        LIVE_SCOPES.with(|c| c.set(0));
        let env = Env::with_prelude();
        let src = "\
            (def make_adder (fn (n) (fn (x) (+ x n))))
            (def add5 (make_adder 5))
            ";
        crate::run_in(src, &env).unwrap();
        // `add5`'s captured scope (holding `n = 5`) must still be alive and
        // correct — this is the correctness counterpart to the leak test
        // above: the fix must not clear a scope a live closure still needs.
        assert_eq!(crate::run_in("(add5 10)", &env).unwrap(), Value::Int(15));
    }

    // ---- list (cons cell) semantics ---------------------------------------

    #[test]
    fn cons_rest_first_len_round_trips() {
        // cons prepends; first/rest peel; len counts — all the same values
        // the old Vec-backed lists produced.
        assert_eq!(
            crate::run_str("(first (cons 1 (list 2 3)))").unwrap(),
            Value::Int(1)
        );
        assert_eq!(
            crate::run_str("(rest (cons 1 (list 2 3)))").unwrap(),
            crate::run_str("(list 2 3)").unwrap()
        );
        assert_eq!(
            crate::run_str("(len (cons 1 (list 2 3)))").unwrap(),
            Value::Int(3)
        );
        // cons is immutable: the original list is untouched.
        let src = "\
            (def xs (list 2 3))\
            (def ys (cons 1 xs))\
            (list (len xs) (len ys) (first xs) (first ys))\
            ";
        assert_eq!(
            crate::run_str(src).unwrap(),
            crate::run_str("(list 2 3 2 1)").unwrap()
        );
        // cons of two equal lists compares equal to a directly built list.
        assert_eq!(
            crate::run_str("(= (cons 1 (list 2)) (list 1 2))").unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn nth_and_empty_list_edge_cases() {
        assert_eq!(
            crate::run_str("(nth (list 1 2 3) 0)").unwrap(),
            Value::Int(1)
        );
        assert_eq!(
            crate::run_str("(nth (list 1 2 3) 1)").unwrap(),
            Value::Int(2)
        );
        assert_eq!(
            crate::run_str("(nth (list 1 2 3) 2)").unwrap(),
            Value::Int(3)
        );
        assert_eq!(crate::run_str("(nth (list 1 2 3) 3)").unwrap(), Value::Nil);
        assert_eq!(crate::run_str("(nth (list 1 2 3) -1)").unwrap(), Value::Nil);
        // Empty list: len 0, first/rest are nil/empty.
        assert_eq!(crate::run_str("(len (list))").unwrap(), Value::Int(0));
        assert_eq!(crate::run_str("(first (list))").unwrap(), Value::Nil);
        assert_eq!(
            crate::run_str("(len (rest (list)))").unwrap(),
            Value::Int(0)
        );
        // `rest` of a one-element list is the empty list (not nil).
        assert_eq!(
            crate::run_str("(rest (list 1))").unwrap(),
            crate::run_str("(list)").unwrap()
        );
        // and it prints as `()`.
        assert_eq!(
            crate::run_str("(str (rest (list 1)))").unwrap(),
            Value::str("()")
        );
    }

    #[test]
    fn push_appends_to_the_end_and_is_non_mutating() {
        assert_eq!(
            crate::run_str("(push (list 1) 2 3)").unwrap(),
            crate::run_str("(list 1 2 3)").unwrap()
        );
        assert_eq!(
            crate::run_str("(push (list) 1)").unwrap(),
            crate::run_str("(list 1)").unwrap()
        );
        let src = "\
            (def xs (list 1))\
            (def ys (push xs 2))\
            (list (len xs) (len ys) (first ys) (rest ys))\
            ";
        assert_eq!(
            crate::run_str(src).unwrap(),
            crate::run_str("(list 1 2 1 (list 2))").unwrap()
        );
    }

    #[test]
    fn quote_and_variadic_rest_build_cons_lists() {
        // Quoted list literals and variadic rest bindings both build lists.
        assert_eq!(
            crate::run_str("(nth (quote (a b c)) 1)").unwrap(),
            crate::run_str("(quote b)").unwrap()
        );
        // (The rest param is named `xs` — naming it `rest` would shadow the
        // `rest` builtin inside the body.)
        let src = "(def f (fn (a & xs) (list a (len xs) (first xs) (rest xs)))) (f 1 2 3 4)";
        assert_eq!(
            crate::run_str(src).unwrap(),
            crate::run_str("(list 1 3 2 (list 3 4))").unwrap()
        );
    }

    #[test]
    fn list_display_and_error_messages_unchanged() {
        // Public rendering: `(1 2 3)`, nested, empty.
        assert_eq!(
            crate::run_str("(str (list 1 (list 2 3) 4))").unwrap(),
            Value::str("(1 (2 3) 4)")
        );
        assert_eq!(crate::run_str("(str (list))").unwrap(), Value::str("()"));
        // Error messages keep the same type names and wording.
        let err = crate::run_str("(first 1)").unwrap_err().to_string();
        assert!(err.contains("first expects list, got int"), "got: {err}");
        let err = crate::run_str("(cons 1 2)").unwrap_err().to_string();
        assert!(err.contains("cons expects (cons value list)"), "got: {err}");
        let err = crate::run_str("(rest 1)").unwrap_err().to_string();
        assert!(err.contains("rest expects list, got int"), "got: {err}");
        let err = crate::run_str("(len true)").unwrap_err().to_string();
        assert!(
            err.contains("len expects list, str, or hash, got bool"),
            "got: {err}"
        );
    }

    /// Performance regression guard: building a 20,000-element list by
    /// repeated `cons` must stay well under 500ms. The old `Rc<Vec<Value>>`
    /// representation made each `cons` an O(n) whole-list clone, so this
    /// program was O(n²) (~1.3s at 20k on the PO's machine); with O(1) cons
    /// cells it is O(n) and runs in single-digit milliseconds. The bound is
    /// deliberately generous so slow CI runners don't flake — it only trips
    /// if the representation regresses to quadratic behavior.
    #[test]
    fn building_a_20k_list_via_cons_stays_linear() {
        let src = "\
            (def n 20000)\
            (def i 0)\
            (def acc (list))\
            (while (< i n)\
              (def acc (cons i acc))\
              (def i (+ i 1)))\
            (len acc)\
            ";
        let start = std::time::Instant::now();
        let result = crate::run_str(src).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(result, Value::Int(20000));
        assert!(
            elapsed.as_millis() < 500,
            "20k cons build took {elapsed:?} — list builtins may have regressed to O(n²)"
        );
    }

    // ---- stdlib (Stage 3.1) -----------------------------------------------
    //
    // One test per builtin, plus the error-message tests that pin the wording
    // the C runtime and the three transpiler targets are held to. File-touching
    // builtins use a per-test temp directory; `exit` (which ends the process)
    // is exercised in `crates/ainl-core/tests/stdlib_cli.rs` via a subprocess.

    /// A unique scratch path under the OS temp dir, for one builtin test.
    /// The test removes it on entry (a leftover from an aborted run must not
    /// make the test read stale content) and the caller removes it after.
    fn scratch_path(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ainl-stdlib-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn as_i64(v: &Value) -> i64 {
        match v {
            Value::Int(i) => *i,
            other => panic!("expected int, got {other:?}"),
        }
    }

    #[test]
    fn file_builtins_round_trip_write_append_read() {
        let dir = scratch_path("file-io");
        let path = dir.join("notes.txt");
        let p = path.to_str().unwrap().to_string();

        // write-file creates (or truncates) and returns nil.
        assert_eq!(
            crate::run_str(&format!("(write-file {p:?} \"alpha\\nbeta\\n\")")).unwrap(),
            Value::Nil
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "alpha\nbeta\n");

        // read-file returns the whole file, newline included.
        assert_eq!(
            crate::run_str(&format!("(read-file {p:?})")).unwrap(),
            Value::str("alpha\nbeta\n")
        );

        // append-file adds to the end, leaving the existing content intact.
        crate::run_str(&format!("(append-file {p:?} \"gamma\\n\")")).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "alpha\nbeta\ngamma\n"
        );

        // write-file on an existing path truncates rather than appending.
        crate::run_str(&format!("(write-file {p:?} \"only\")")).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "only");

        // An empty string is a legitimate file (not a no-op that leaves the
        // previous content in place).
        crate::run_str(&format!("(write-file {p:?} \"\")")).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_file_on_a_missing_path_is_a_clear_runtime_error() {
        let dir = scratch_path("file-missing");
        let missing = dir.join("nope.txt").to_str().unwrap().to_string();
        let err = crate::run_str(&format!("(read-file {missing:?})"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("read-file: cannot read"),
            "read-file must say what failed and where, got: {err}"
        );
        // The path itself is in the message, so a typo is visible without a
        // debugger.
        assert!(err.contains("nope.txt"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn string_builtins_split_join_trim_replace_case_contains() {
        // split → list, including the leading-empty-field case every host
        // language agrees on ("a,b".split(",") = ["a", "b"]; ",a" = ["", "a"]).
        assert_eq!(
            crate::run_str(r#"(split "a,b,c" ",")"#).unwrap(),
            crate::run_str(r#"(list "a" "b" "c")"#).unwrap()
        );
        assert_eq!(
            crate::run_str(r#"(split "one two  three" " ")"#).unwrap(),
            crate::run_str(r#"(list "one" "two" "" "three")"#).unwrap()
        );
        // A separator that never occurs yields a single-element list.
        assert_eq!(
            crate::run_str(r#"(split "abc" ",")"#).unwrap(),
            crate::run_str(r#"(list "abc")"#).unwrap()
        );
        // A multi-character separator works.
        assert_eq!(
            crate::run_str(r#"(split "a::b::c" "::")"#).unwrap(),
            crate::run_str(r#"(list "a" "b" "c")"#).unwrap()
        );

        // join is the inverse of split.
        assert_eq!(
            crate::run_str(r#"(join (split "x,y,z" ",") ",")"#).unwrap(),
            Value::str("x,y,z")
        );
        // join of the empty list is the empty string (no separator, no quotes).
        assert_eq!(
            crate::run_str(r#"(join (list) ",")"#).unwrap(),
            Value::str("")
        );
        // join of one element never emits the separator.
        assert_eq!(
            crate::run_str(r#"(join (list "solo") ",")"#).unwrap(),
            Value::str("solo")
        );

        // trim strips ASCII whitespace on both ends, nothing in the middle.
        assert_eq!(
            crate::run_str(r#"(trim "  hi  ")"#).unwrap(),
            Value::str("hi")
        );
        assert_eq!(
            crate::run_str(r#"(trim "\t\n hi \r\n")"#).unwrap(),
            Value::str("hi")
        );
        // An all-whitespace string trims to empty.
        assert_eq!(crate::run_str(r#"(trim "   ")"#).unwrap(), Value::str(""));
        // A string with no leading/trailing space is unchanged.
        assert_eq!(
            crate::run_str(r#"(trim "hi there")"#).unwrap(),
            Value::str("hi there")
        );

        // replace rewrites every non-overlapping occurrence.
        assert_eq!(
            crate::run_str(r#"(replace "a-b-c" "-" "+")"#).unwrap(),
            Value::str("a+b+c")
        );
        assert_eq!(
            crate::run_str(r#"(replace "aaaa" "aa" "b")"#).unwrap(),
            Value::str("bb")
        );
        // No match leaves the string alone.
        assert_eq!(
            crate::run_str(r#"(replace "abc" "z" "y")"#).unwrap(),
            Value::str("abc")
        );
        // The replacement may itself contain the target — it is not rescanned.
        assert_eq!(
            crate::run_str(r#"(replace "a" "a" "aa")"#).unwrap(),
            Value::str("aa")
        );

        // upcase/downcase are ASCII-only, by design (see `is_ascii_ws`).
        assert_eq!(
            crate::run_str(r#"(upcase "hello")"#).unwrap(),
            Value::str("HELLO")
        );
        assert_eq!(
            crate::run_str(r#"(downcase "HeLLo")"#).unwrap(),
            Value::str("hello")
        );
        assert_eq!(
            crate::run_str(r#"(upcase "a-b_c 1")"#).unwrap(),
            Value::str("A-B_C 1")
        );

        // contains is a substring test returning a bool.
        assert_eq!(
            crate::run_str(r#"(contains "haystack" "stack")"#).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            crate::run_str(r#"(contains "haystack" "needle")"#).unwrap(),
            Value::Bool(false)
        );
        // An empty needle is always contained.
        assert_eq!(
            crate::run_str(r#"(contains "x" "")"#).unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn stdlib_string_builtins_reject_the_host_disagreements() {
        // An empty split separator: Python raises, JS splits per character,
        // Ruby raises. AINL refuses, so all four agree.
        for (src, want) in [
            (r#"(split "abc" "")"#, "split expects a non-empty separator"),
            (
                r#"(replace "abc" "" "x")"#,
                "replace expects a non-empty target",
            ),
        ] {
            let err = crate::run_str(src).unwrap_err().to_string();
            assert!(
                err.contains(want),
                "for `{src}` expected {want:?}, got: {err}"
            );
        }
        // A non-string operand reports the builtin's own name and the type.
        for (src, want) in [
            (r#"(trim 1)"#, "trim expects a str, got int"),
            (r#"(upcase (list 1))"#, "upcase expects a str, got list"),
            (r#"(contains "a" 1)"#, "contains expects a str, got int"),
            (r#"(split 1 ",")"#, "split expects a str, got int"),
        ] {
            let err = crate::run_str(src).unwrap_err().to_string();
            assert!(
                err.contains(want),
                "for `{src}` expected {want:?}, got: {err}"
            );
        }
    }

    #[test]
    fn env_get_reads_the_environment_and_is_nil_when_unset() {
        // Set into this process's env so the test sees it.
        // (std::env::set_var is unsafe in edition 2024, hence the lock; the
        // value is process-global either way.)
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: single-threaded within the lock; no other thread reads it.
        unsafe { std::env::set_var("AINL_STDLIB_TEST", "present") };

        assert_eq!(
            crate::run_str(r#"(env-get "AINL_STDLIB_TEST")"#).unwrap(),
            Value::str("present")
        );
        // An unset variable is nil, not an error — that's what makes
        // `(if (env-get "X") ...)` work.
        assert_eq!(
            crate::run_str(r#"(env-get "AINL_STDLIB_DEFINITELY_UNSET")"#).unwrap(),
            Value::Nil
        );
        // An empty value is still a value (a str, not nil).
        unsafe { std::env::set_var("AINL_STDLIB_EMPTY", "") };
        assert_eq!(
            crate::run_str(r#"(env-get "AINL_STDLIB_EMPTY")"#).unwrap(),
            Value::str("")
        );
        unsafe {
            std::env::remove_var("AINL_STDLIB_TEST");
            std::env::remove_var("AINL_STDLIB_EMPTY");
        }
        // A non-string name is rejected under the builtin's own name.
        let err = crate::run_str("(env-get 1)").unwrap_err().to_string();
        assert!(err.contains("env-get expects a str, got int"), "got: {err}");
    }

    #[test]
    fn now_is_plausible_seconds_since_the_epoch_and_is_monotonic_enough() {
        // 2020-01-01T00:00:00Z. If this trips, the machine's clock is wrong
        // (or `now` regressed to something smaller than seconds).
        const YEAR_2020: i64 = 1_577_836_800;
        let t = as_i64(&crate::run_str("(now)").unwrap());
        assert!(t > YEAR_2020, "now() returned {t}, implausibly small");
        // Sanity on the far end: below year 10000 (~2.5e11) and within i64.
        assert!(t < 253_402_300_800, "now() returned {t}, implausibly large");
        // `(now)` takes no arguments and must not silently ignore them.
        let err = crate::run_str("(now 1)").unwrap_err().to_string();
        assert!(err.contains("now expects (now)"), "got: {err}");
    }

    #[test]
    fn sleep_accepts_zero_and_a_short_nap_and_returns_nil() {
        assert_eq!(crate::run_str("(sleep 0)").unwrap(), Value::Nil);
        // 0.01s is long enough to observe, short enough to keep the suite fast.
        let start = std::time::Instant::now();
        assert_eq!(crate::run_str("(sleep 0.01)").unwrap(), Value::Nil);
        assert!(
            start.elapsed().as_millis() >= 5,
            "sleep(0.01) returned in under 5ms — did it actually sleep?"
        );
        // Negative is an error, not a silent no-op (Python raises, JS ignores).
        let err = crate::run_str("(sleep -1)").unwrap_err().to_string();
        assert!(
            err.contains("sleep expects a non-negative number"),
            "got: {err}"
        );
    }

    #[test]
    fn math_builtins_abs_min_max_floor_sqrt() {
        // abs stays integer; abs of i64::MIN promotes to float (no i64 answer).
        assert_eq!(crate::run_str("(abs -7)").unwrap(), Value::Int(7));
        assert_eq!(crate::run_str("(abs 7)").unwrap(), Value::Int(7));
        assert_eq!(crate::run_str("(abs -2.5)").unwrap(), Value::Float(2.5));
        assert_eq!(
            crate::run_str("(abs -9223372036854775808)").unwrap(),
            Value::Float(9223372036854775808.0)
        );

        // min/max are variadic and fold pairwise.
        assert_eq!(crate::run_str("(min 3 1 2)").unwrap(), Value::Int(1));
        assert_eq!(crate::run_str("(max 3 1 2)").unwrap(), Value::Int(3));
        assert_eq!(crate::run_str("(min 5)").unwrap(), Value::Int(5));
        assert_eq!(crate::run_str("(max 1.5 2)").unwrap(), Value::Float(2.0));
        // Mixed int/float comparison is numeric, not per-type.
        assert_eq!(crate::run_str("(min 2 1.5)").unwrap(), Value::Float(1.5));
        // Ties keep the first argument (the < / > comparison is strict), so a
        // min over equal values is the first one.
        assert_eq!(crate::run_str("(min 2 2)").unwrap(), Value::Int(2));
        assert_eq!(crate::run_str("(max 2.5 2.5)").unwrap(), Value::Float(2.5));
        // min/max are numeric-only: a list operand is a type error, not a
        // lexicographic comparison (this is what makes the C/JS/Python/Ruby
        // mappings agree without per-target sorting semantics).
        let err = crate::run_str("(min (list 1) 1)").unwrap_err().to_string();
        assert!(err.contains("min expects a number, got list"), "got: {err}");
        // Negatives and zero.
        assert_eq!(crate::run_str("(min 0 -3 -1)").unwrap(), Value::Int(-3));
        assert_eq!(crate::run_str("(max 0 -3 -1)").unwrap(), Value::Int(0));

        // floor always yields an int; an int argument passes through unchanged
        // (no float round-trip, so a large i64 keeps every bit).
        assert_eq!(crate::run_str("(floor 2.7)").unwrap(), Value::Int(2));
        assert_eq!(crate::run_str("(floor -2.1)").unwrap(), Value::Int(-3));
        assert_eq!(crate::run_str("(floor 4)").unwrap(), Value::Int(4));
        assert_eq!(
            crate::run_str("(floor 9007199254740993)").unwrap(),
            Value::Int(9007199254740993)
        );

        // sqrt is always a float.
        assert_eq!(crate::run_str("(sqrt 16)").unwrap(), Value::Float(4.0));
        assert_eq!(
            crate::run_str("(sqrt 2)").unwrap(),
            Value::Float(std::f64::consts::SQRT_2)
        );
        assert_eq!(crate::run_str("(sqrt 0)").unwrap(), Value::Float(0.0));
        // A negative argument is an error, not a silent NaN.
        let err = crate::run_str("(sqrt -1)").unwrap_err().to_string();
        assert!(
            err.contains("sqrt expects a non-negative number"),
            "got: {err}"
        );
    }

    #[test]
    fn math_builtins_report_their_own_name_in_type_errors() {
        for (src, want) in [
            ("(abs \"x\")", "abs expects a number, got str"),
            ("(sqrt \"x\")", "sqrt expects a number, got str"),
            ("(floor (list 1))", "floor expects a number, got list"),
            ("(min 1 \"x\")", "min expects a number, got str"),
            ("(max \"x\")", "max expects a number, got str"),
        ] {
            let err = crate::run_str(src).unwrap_err().to_string();
            assert!(
                err.contains(want),
                "for `{src}` expected {want:?}, got: {err}"
            );
        }
        // min/max need at least one argument.
        for (src, want) in [
            ("(min)", "min expects at least 1 argument"),
            ("(max)", "max expects at least 1 argument"),
        ] {
            let err = crate::run_str(src).unwrap_err().to_string();
            assert!(
                err.contains(want),
                "for `{src}` expected {want:?}, got: {err}"
            );
        }
    }

    #[test]
    fn the_vm_and_the_tree_walk_agree_on_every_stdlib_builtin() {
        // The tree-walk is the semantic reference the VM is checked against;
        // a stdlib builtin that only one of them wires up is a silent
        // divergence, so both are run over the same source and compared.
        let cases = [
            r#"(split "a,b" ",")"#,
            r#"(join (list "a" "b") "-")"#,
            r#"(trim "  x  ")"#,
            r#"(replace "a-a" "a" "b")"#,
            r#"(upcase "ab")"#,
            r#"(downcase "AB")"#,
            r#"(contains "abc" "b")"#,
            r#"(abs -3)"#,
            "(min 3 1)",
            "(max 3 1)",
            "(floor 1.9)",
            "(sqrt 9)",
            r#"(env-get "AINL_STDLIB_UNSET_XYZ")"#,
        ];
        for src in cases {
            let vm = crate::run_str(src).unwrap_or_else(|e| panic!("VM failed on `{src}`: {e}"));
            let tw = crate::run_in_tree_walk(src)
                .unwrap_or_else(|e| panic!("tree-walk failed on `{src}`: {e}"));
            assert_eq!(vm, tw, "VM and tree-walk disagree on `{src}`");
        }
    }
}
