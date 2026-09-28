//! AINL AST → JavaScript source projection.
//!
//! Mirrors the Python backend's two-context model (expression vs statement) but
//! renders JavaScript: brace-delimited blocks, arrow functions, `var` for
//! `def` (so re-`def` reassigns like AINL), and ternaries for `if` expressions.
//!
//! Two JS-specific gotchas handled here:
//! * JS has no chained comparison — `(< 1 2 3)` expands to `(1 < 2 && 2 < 3)`.
//! * JS has a single `number` type, so AINL's int/float distinction is lost;
//!   division that yields a whole number prints without a trailing `.0`. The
//!   sample programs avoid that case and verify byte-identical.

use crate::shared::{self, ExprEmit};
use ainl_core::parser::Node;
use ainl_core::serialize::LineIndex;
use ainl_core::{Error, Result};
use std::collections::BTreeSet;

pub fn transpile_js_src(src: &str) -> Result<String> {
    let forms = ainl_core::parse(src)?;
    transpile_js(&forms, src)
}

pub fn transpile_js(forms: &[Node], src: &str) -> Result<String> {
    let idx = LineIndex::new(src);
    let mut js = Js {
        body: String::new(),
        indent: 0,
        needed: BTreeSet::new(),
    };
    for form in forms {
        js.top_form(form, &idx)?;
    }
    Ok(js.finish())
}

struct Js {
    body: String,
    indent: usize,
    needed: BTreeSet<&'static str>,
}

impl Js {
    fn line(&mut self, s: &str) {
        for _ in 0..self.indent {
            self.body.push_str("  ");
        }
        self.body.push_str(s);
        self.body.push('\n');
    }

    fn need(&mut self, name: &'static str) {
        self.needed.insert(name);
    }

    fn finish(mut self) -> String {
        let disp_used = self
            .needed
            .iter()
            .any(|n| matches!(*n, "_print" | "_str" | "_error" | "_repr" | "_disp"));
        if disp_used {
            self.needed.insert("_disp");
            self.needed.insert("_repr");
        }
        // Key comparison for hash lookups needs structural equality.
        if self
            .needed
            .iter()
            .any(|n| matches!(*n, "_hash" | "_get" | "_assoc" | "_has"))
        {
            self.needed.insert("_eq");
        }
        // `_disp` branches on `instanceof _Hash`; `_hash`/`_assoc` construct one.
        if disp_used || self.needed.iter().any(|n| matches!(*n, "_hash" | "_assoc")) {
            self.needed.insert("_Hash");
        }
        if disp_used || self.needed.contains("_sym") || self.needed.contains("_eq") {
            self.needed.insert("_Sym");
        }
        // Stage 3.1 stdlib dependencies: `_min`/`_max` share one fold helper and
        // `_sleep`/`_abs`/`_floor`/`_sqrt` share the number check.
        if self.needed.contains("_min") || self.needed.contains("_max") {
            self.needed.insert("_minmax");
        }
        for dep in ["_sleep", "_abs", "_floor", "_sqrt"] {
            if self.needed.contains(dep) {
                self.needed.insert("_isnum");
            }
        }
        // Tier 1 file I/O: the three path builtins share one canonicalizer, and
        // `_ainl_tname` (used by `path-join`'s positional type error) branches
        // on _Hash and _Sym, so it must pull both in.
        if ["_path_join", "_path_base", "_path_dir"]
            .iter()
            .any(|n| self.needed.contains(*n))
        {
            self.needed.insert("_path_canonical");
        }
        if self.needed.contains("_path_join") {
            self.needed.insert("_ainl_tname");
            self.needed.insert("_Hash");
            self.needed.insert("_Sym");
        }
        let mut out = String::new();
        out.push_str("// Transpiled from AINL by `ainl transpile --to js`.\n");
        out.push_str("// Generated code: edit the .ainl source, not this file.\n\n");
        let mut wrote_runtime = false;
        for (name, code) in RUNTIME {
            if self.needed.contains(name) {
                out.push_str(code);
                out.push('\n');
                wrote_runtime = true;
            }
        }
        if wrote_runtime {
            out.push('\n');
        }
        out.push_str(&self.body);
        out
    }

    // -- top level & statements ---------------------------------------------

    fn top_form(&mut self, form: &Node, idx: &LineIndex) -> Result<()> {
        if let Node::List(items, _) = form {
            if let (Some(Node::Sym(op, _)), Some(Node::Sym(name, _))) =
                (items.first(), items.get(1))
            {
                if op == "def" {
                    let (line, _) = idx.locate(form.span().start);
                    self.line(&format!("// ainl:{line}  {name}"));
                }
            }
        }
        self.stmt(form, false)
    }

    fn stmt(&mut self, form: &Node, ret: bool) -> Result<()> {
        if let Node::List(items, _) = form {
            if let Some(Node::Sym(op, _)) = items.first() {
                match op.as_str() {
                    "def" => return self.stmt_def(&items[1..], ret),
                    "while" => return self.stmt_while(&items[1..], ret),
                    "let" => return self.stmt_let(&items[1..], ret),
                    "do" => return self.stmt_body(&items[1..], ret),
                    "if" => return self.stmt_if(&items[1..], ret),
                    _ => {}
                }
            }
        }
        let e = self.expr(form)?;
        if ret {
            self.line(&format!("return {e};"));
        } else {
            self.line(&format!("{e};"));
        }
        Ok(())
    }

    fn stmt_body(&mut self, forms: &[Node], ret: bool) -> Result<()> {
        if forms.is_empty() {
            if ret {
                self.line("return null;");
            }
            return Ok(());
        }
        let (last, init) = forms.split_last().unwrap();
        for f in init {
            self.stmt(f, false)?;
        }
        self.stmt(last, ret)
    }

    fn stmt_def(&mut self, args: &[Node], ret: bool) -> Result<()> {
        let [name_node, val] = args else {
            return Err(Error::runtime("def expects (def name value)"));
        };
        let Node::Sym(raw, _) = name_node else {
            return Err(Error::runtime("def name must be a symbol"));
        };
        let name = sanitize(raw);
        if let Node::List(fitems, _) = val {
            if matches!(fitems.first(), Some(Node::Sym(s, _)) if s == "fn") {
                self.def_function(&name, &fitems[1..])?;
                if ret {
                    self.line(&format!("return {name};"));
                }
                return Ok(());
            }
        }
        let e = self.expr(val)?;
        self.line(&format!("var {name} = {e};"));
        if ret {
            self.line(&format!("return {name};"));
        }
        Ok(())
    }

    fn def_function(&mut self, name: &str, fn_args: &[Node]) -> Result<()> {
        let Some((params_node, body)) = fn_args.split_first() else {
            return Err(Error::runtime("fn expects (fn (params...) body...)"));
        };
        let params = js_params(params_node)?;
        self.line(&format!("function {name}({params}) {{"));
        self.indent += 1;
        if body.is_empty() {
            self.line("return null;");
        } else {
            self.stmt_body(body, true)?;
        }
        self.indent -= 1;
        self.line("}");
        Ok(())
    }

    fn stmt_while(&mut self, args: &[Node], ret: bool) -> Result<()> {
        let Some((cond, body)) = args.split_first() else {
            return Err(Error::runtime("while expects (while cond body...)"));
        };
        let c = self.expr(cond)?;
        self.line(&format!("while ({c}) {{"));
        self.indent += 1;
        for f in body {
            self.stmt(f, false)?;
        }
        self.indent -= 1;
        self.line("}");
        if ret {
            self.line("return null;");
        }
        Ok(())
    }

    fn stmt_let(&mut self, args: &[Node], ret: bool) -> Result<()> {
        let Some((binds_node, body)) = args.split_first() else {
            return Err(Error::runtime("let expects (let ((n v)...) body...)"));
        };
        for (name, val) in shared::let_bindings(binds_node)? {
            let e = self.expr(val)?;
            self.line(&format!("var {} = {e};", sanitize(name)));
        }
        self.stmt_body(body, ret)
    }

    fn stmt_if(&mut self, args: &[Node], ret: bool) -> Result<()> {
        match args {
            [cond, then] => {
                let c = self.expr(cond)?;
                self.line(&format!("if ({c}) {{"));
                self.indent += 1;
                self.stmt(then, ret)?;
                self.indent -= 1;
                if ret {
                    self.line("} else {");
                    self.indent += 1;
                    self.line("return null;");
                    self.indent -= 1;
                }
                self.line("}");
                Ok(())
            }
            [cond, then, els] => {
                let c = self.expr(cond)?;
                self.line(&format!("if ({c}) {{"));
                self.indent += 1;
                self.stmt(then, ret)?;
                self.indent -= 1;
                self.line("} else {");
                self.indent += 1;
                self.stmt(els, ret)?;
                self.indent -= 1;
                self.line("}");
                Ok(())
            }
            _ => Err(Error::runtime("if expects (if cond then [else])")),
        }
    }

    // -- expressions ---------------------------------------------------------

    fn expr_list(&mut self, items: &[Node], span: ainl_core::Span) -> Result<String> {
        let Some(head) = items.first() else {
            return Ok("null".to_string());
        };
        let args = &items[1..];
        if let Node::Sym(op, _) = head {
            match op.as_str() {
                "+" => return shared::infix(self, args, "+", "0"),
                "*" => return shared::infix(self, args, "*", "1"),
                "-" => return shared::infix_sub(self, args),
                "/" => return self.infix_div(args),
                "=" => return self.eq_chain(args),
                "<" => return shared::cmp(self, args, "<"),
                ">" => return shared::cmp(self, args, ">"),
                "<=" => return shared::cmp(self, args, "<="),
                ">=" => return shared::cmp(self, args, ">="),
                "and" => return shared::infix(self, args, "&&", "true"),
                "or" => return shared::infix(self, args, "||", "false"),
                "not" => return shared::unary(self, args, "!"),
                "mod" => return shared::binary(self, args, "%"),
                "if" => return self.expr_if(args),
                "let" => return self.expr_let(args, span),
                "do" => return self.expr_do(args, span),
                "quote" => return self.expr_quote(args),
                "fn" => return self.expr_fn(args, span),
                "list" => {
                    let parts = self.expr_all(args)?;
                    return Ok(format!("[{}]", parts.join(", ")));
                }
                "while" => return Err(self.no_expr("while", span)),
                "print" => {
                    self.need("_print");
                    return Ok(format!("_print({})", self.expr_all(args)?.join(", ")));
                }
                "str" => {
                    self.need("_str");
                    return Ok(format!("_str({})", self.expr_all(args)?.join(", ")));
                }
                "len" => return self.call_builtin("_len", args, Some("_len")),
                "first" => return self.call_builtin("_first", args, Some("_first")),
                "rest" => return self.call_builtin("_rest", args, Some("_rest")),
                "nth" => return self.call_builtin("_nth", args, Some("_nth")),
                "cons" => return self.call_builtin("_cons", args, Some("_cons")),
                "push" => return self.call_builtin("_push", args, Some("_push")),
                "hash" => return self.call_builtin("_hash", args, Some("_hash")),
                "get" => return self.call_builtin("_get", args, Some("_get")),
                "assoc" => return self.call_builtin("_assoc", args, Some("_assoc")),
                "has" => return self.call_builtin("_has", args, Some("_has")),
                "keys" => return self.call_builtin("_keys", args, Some("_keys")),
                "vals" => return self.call_builtin("_vals", args, Some("_vals")),
                "error" => return self.call_builtin("_error", args, Some("_error")),
                // ---- Stage 3.1 stdlib ----
                // Each maps to the host's own idiom (fs.readFileSync,
                // process.env, Date.now()/1000, Math.sqrt) so the emitted JS
                // reads like JS. Where JS's own behavior would diverge from the
                // interpreter's, the helper restores the interpreter's rule —
                // most importantly, the synchronous fs calls and the integer
                // checks, since Node's `"1" < 2` and `[] + 1` coercions would
                // otherwise produce a value where AINL errors.
                "read-file" => return self.call_builtin("_read_file", args, Some("_read_file")),
                "write-file" => return self.call_builtin("_write_file", args, Some("_write_file")),
                "append-file" => {
                    return self.call_builtin("_append_file", args, Some("_append_file"))
                }
                // ---- Tier 1 file I/O ----
                "file-exists" => {
                    return self.call_builtin("_file_exists", args, Some("_file_exists"))
                }
                "delete-file" => {
                    return self.call_builtin("_delete_file", args, Some("_delete_file"))
                }
                "list-dir" => return self.call_builtin("_list_dir", args, Some("_list_dir")),
                "path-join" => return self.call_builtin("_path_join", args, Some("_path_join")),
                "path-base" => return self.call_builtin("_path_base", args, Some("_path_base")),
                "path-dir" => return self.call_builtin("_path_dir", args, Some("_path_dir")),
                "split" => return self.call_builtin("_split", args, Some("_split")),
                "join" => return self.call_builtin("_join", args, Some("_join")),
                "trim" => return self.call_builtin("_trim", args, Some("_trim")),
                "replace" => return self.call_builtin("_replace", args, Some("_replace")),
                "upcase" => return self.call_builtin("_upcase", args, Some("_upcase")),
                "downcase" => return self.call_builtin("_downcase", args, Some("_downcase")),
                "contains" => return self.call_builtin("_contains", args, Some("_contains")),
                "env-get" => return self.call_builtin("_env_get", args, Some("_env_get")),
                "exit" => return self.call_builtin("_exit", args, Some("_exit")),
                "now" => return self.call_builtin("_now", args, Some("_now")),
                "sleep" => return self.call_builtin("_sleep", args, Some("_sleep")),
                "abs" => return self.call_builtin("_abs", args, Some("_abs")),
                "min" => return self.call_builtin("_min", args, Some("_min")),
                "max" => return self.call_builtin("_max", args, Some("_max")),
                "floor" => return self.call_builtin("_floor", args, Some("_floor")),
                "sqrt" => return self.call_builtin("_sqrt", args, Some("_sqrt")),
                _ => {}
            }
        }
        let callee = self.expr(head)?;
        let parts = self.expr_all(args)?;
        Ok(format!("{callee}({})", parts.join(", ")))
    }

    fn call_builtin(
        &mut self,
        name: &str,
        args: &[Node],
        need: Option<&'static str>,
    ) -> Result<String> {
        if let Some(n) = need {
            self.need(n);
        }
        Ok(format!("{name}({})", self.expr_all(args)?.join(", ")))
    }

    fn infix_div(&mut self, args: &[Node]) -> Result<String> {
        let parts = self.expr_all(args)?;
        match parts.len() {
            0 => Err(Error::runtime("/ expects at least 1 argument")),
            1 => Ok(format!("(1 / {})", parts[0])),
            _ => Ok(format!("({})", parts.join(" / "))),
        }
    }

    /// `=` needs structural equality (AINL lists compare element-wise, and a
    /// quoted symbol compares by name), unlike `===` which is JS reference
    /// identity for arrays and `_Sym` instances. Expand chained `(= a b c)`
    /// to `(_eq(a, b) && _eq(b, c))`, matching `shared::cmp`'s chaining shape.
    fn eq_chain(&mut self, args: &[Node]) -> Result<String> {
        if args.len() < 2 {
            return Ok("true".to_string());
        }
        self.need("_eq");
        let parts = self.expr_all(args)?;
        let clauses: Vec<String> = parts
            .windows(2)
            .map(|w| format!("_eq({}, {})", w[0], w[1]))
            .collect();
        Ok(format!("({})", clauses.join(" && ")))
    }

    fn expr_if(&mut self, args: &[Node]) -> Result<String> {
        let (cond, then, els) = match args {
            [c, t] => (c, t, None),
            [c, t, e] => (c, t, Some(e)),
            _ => return Err(Error::runtime("if expects (if cond then [else])")),
        };
        let c = self.expr(cond)?;
        let t = self.expr(then)?;
        let e = match els {
            Some(e) => self.expr(e)?,
            None => "null".to_string(),
        };
        Ok(format!("({c} ? {t} : {e})"))
    }

    fn expr_let(&mut self, args: &[Node], span: ainl_core::Span) -> Result<String> {
        let Some((binds_node, body)) = args.split_first() else {
            return Err(Error::runtime("let expects (let ((n v)...) body...)"));
        };
        if body.len() != 1 {
            return Err(self.no_expr("multi-statement let", span));
        }
        let mut names = Vec::new();
        let mut vals = Vec::new();
        for (n, v) in shared::let_bindings(binds_node)? {
            names.push(sanitize(n));
            vals.push(self.expr(v)?);
        }
        let b = self.expr(&body[0])?;
        Ok(format!(
            "(({}) => {b})({})",
            names.join(", "),
            vals.join(", ")
        ))
    }

    fn expr_do(&mut self, args: &[Node], span: ainl_core::Span) -> Result<String> {
        match args {
            [one] => self.expr(one),
            _ => Err(self.no_expr("multi-statement do", span)),
        }
    }

    fn expr_fn(&mut self, args: &[Node], span: ainl_core::Span) -> Result<String> {
        let Some((params_node, body)) = args.split_first() else {
            return Err(Error::runtime("fn expects (fn (params...) body...)"));
        };
        if body.len() != 1 {
            return Err(Error::runtime(format!(
                "fn with a multi-statement body cannot be a JS arrow expression (bytes {}..{}); bind it with def",
                span.start, span.end
            )));
        }
        let params = js_params(params_node)?;
        let b = self.expr(&body[0])?;
        Ok(format!("(({params}) => {b})"))
    }
}

impl ExprEmit for Js {
    fn expr(&mut self, node: &Node) -> Result<String> {
        match node {
            Node::Int(i, _) => Ok(i.to_string()),
            Node::Float(x, _) => Ok(js_float(*x)),
            Node::Str(s, _) => Ok(js_str(s)),
            Node::Sym(name, _) => Ok(match name.as_str() {
                "true" => "true".to_string(),
                "false" => "false".to_string(),
                "nil" => "null".to_string(),
                other => sanitize(other),
            }),
            Node::List(items, _) => self.expr_list(items, node.span()),
        }
    }
}

impl Js {
    fn expr_quote(&mut self, args: &[Node]) -> Result<String> {
        let [node] = args else {
            return Err(Error::runtime("quote expects one form"));
        };
        Ok(self.quote(node))
    }

    fn quote(&mut self, node: &Node) -> String {
        match node {
            Node::Int(i, _) => i.to_string(),
            Node::Float(x, _) => js_float(*x),
            Node::Str(s, _) => js_str(s),
            Node::Sym(s, _) => {
                self.need("_sym");
                format!("_sym({})", js_str(s))
            }
            Node::List(items, _) => {
                let parts: Vec<String> = items.iter().map(|n| self.quote(n)).collect();
                format!("[{}]", parts.join(", "))
            }
        }
    }

    fn no_expr(&self, what: &str, span: ainl_core::Span) -> Error {
        Error::runtime(format!(
            "cannot transpile `{what}` in expression position (bytes {}..{})",
            span.start, span.end
        ))
    }
}

fn js_params(params_node: &Node) -> Result<String> {
    Ok(shared::parse_params(params_node, "...", sanitize)?.join(", "))
}

fn sanitize(name: &str) -> String {
    let mut s = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_alphanumeric() || c == '_' || c == '$' {
            s.push(c);
        } else {
            s.push('_');
        }
    }
    if s.is_empty() || s.chars().next().unwrap().is_ascii_digit() {
        s.insert(0, '_');
    }
    if is_js_reserved(&s) {
        s.push('_');
    }
    s
}

fn is_js_reserved(s: &str) -> bool {
    matches!(
        s,
        "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "continue"
            | "debugger"
            | "default"
            | "delete"
            | "do"
            | "else"
            | "export"
            | "extends"
            | "finally"
            | "for"
            | "function"
            | "if"
            | "import"
            | "in"
            | "instanceof"
            | "new"
            | "return"
            | "super"
            | "switch"
            | "this"
            | "throw"
            | "try"
            | "typeof"
            | "var"
            | "void"
            | "while"
            | "with"
            | "yield"
            | "let"
            | "static"
            | "enum"
            | "await"
            | "null"
            | "true"
            | "false"
    )
}

fn js_float(x: f64) -> String {
    if !x.is_finite() {
        return "NaN".to_string();
    }
    if x.fract() == 0.0 {
        format!("{x:.1}")
    } else {
        format!("{x}")
    }
}

fn js_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            '\r' => o.push_str("\\r"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

const RUNTIME: &[(&str, &str)] = &[
    ("_Sym", "class _Sym { constructor(name) { this.name = name; } }"),
    ("_sym", "function _sym(s) { return new _Sym(s); }"),
    // A map is an array of [k, v] pairs; this subclass exists only so
    // `_disp` can tell a hash apart from a plain list at print time (a
    // print call can't otherwise know a variable's AINL-level type).
    ("_Hash", "class _Hash extends Array {}"),
    (
        "_disp",
        "function _disp(x) {\n  if (x === true) return \"true\";\n  if (x === false) return \"false\";\n  if (x === null || x === undefined) return \"nil\";\n  if (x instanceof _Sym) return x.name;\n  if (x instanceof _Hash) return \"{\" + x.map(p => _repr(p[0]) + \" \" + _repr(p[1])).join(\" \") + \"}\";\n  if (Array.isArray(x)) return \"(\" + x.map(_repr).join(\" \") + \")\";\n  return String(x);\n}",
    ),
    (
        "_repr",
        "function _repr(x) {\n  return typeof x === \"string\" ? '\"' + x + '\"' : _disp(x);\n}",
    ),
    (
        "_eq",
        "function _eq(a, b) {\n  if (Array.isArray(a) && Array.isArray(b)) {\n    if (a.length !== b.length) return false;\n    for (let i = 0; i < a.length; i++) { if (!_eq(a[i], b[i])) return false; }\n    return true;\n  }\n  if (a instanceof _Sym && b instanceof _Sym) return a.name === b.name;\n  return a === b;\n}",
    ),
    ("_print", "function _print(...xs) { console.log(xs.map(_disp).join(\" \")); }"),
    ("_str", "function _str(...xs) { return xs.map(_disp).join(\"\"); }"),
    ("_len", "function _len(x) { return x.length; }"),
    ("_first", "function _first(x) { return x.length ? x[0] : null; }"),
    ("_rest", "function _rest(x) { return x.slice(1); }"),
    ("_nth", "function _nth(x, i) { return (0 <= i && i < x.length) ? x[i] : null; }"),
    ("_cons", "function _cons(h, t) { return [h].concat(t); }"),
    ("_push", "function _push(t, ...xs) { return t.concat(xs); }"),
    (
        "_hash",
        "function _hash(...kvs) {\n  const out = new _Hash();\n  for (let i = 0; i < kvs.length; i += 2) {\n    const k = kvs[i], v = kvs[i + 1];\n    const pair = out.find(p => _eq(p[0], k));\n    if (pair) { pair[1] = v; } else { out.push([k, v]); }\n  }\n  return out;\n}",
    ),
    (
        "_get",
        "function _get(h, k) {\n  const pair = h.find(p => _eq(p[0], k));\n  return pair ? pair[1] : null;\n}",
    ),
    (
        "_assoc",
        "function _assoc(h, k, v) {\n  const out = _Hash.from(h, p => p.slice());\n  const pair = out.find(p => _eq(p[0], k));\n  if (pair) { pair[1] = v; } else { out.push([k, v]); }\n  return out;\n}",
    ),
    (
        "_has",
        "function _has(h, k) { return h.some(p => _eq(p[0], k)); }",
    ),
    // `Array.from` (not `h.map`, which inherits _Hash via Symbol.species) —
    // keys/vals return plain lists, not hashes.
    ("_keys", "function _keys(h) { return Array.from(h, p => p[0]); }"),
    ("_vals", "function _vals(h) { return Array.from(h, p => p[1]); }"),
    ("_error", "function _error(...xs) { throw new Error(xs.map(_disp).join(\" \")); }"),
    // ---- Stage 3.1 stdlib ----
    // JS's own behavior diverges from the interpreter's in ways that matter
    // here, so each helper pins the interpreter's rule:
    //   * Number checks: JS coerces ("1" + 2 === "12", [] + 1 === "1", and
    //     "10" < 9 is true), so _isnum rejects non-numbers instead of letting a
    //     coercion produce a value where AINL errors.
    //   * split/replace with an empty target: JS returns per-character
    //     results / the input unchanged, where AINL rejects.
    //   * trim/case are ASCII-only (JS's are Unicode-aware).
    //   * floor returns an int, and sqrt rejects negatives instead of NaN.
    //   * join requires strings, so `join([1,2])` is an error, not "1,2".
    // The `require` calls are lazy inside the helpers so a program that only
    // uses, say, `trim` never loads fs.
    ("_isnum", "function _isnum(x) { return typeof x === \"number\"; }"),
    (
        "_read_file",
        "function _read_file(path) {\n  if (typeof path !== \"string\") throw new TypeError(\"read-file expects a str path\");\n  return require(\"fs\").readFileSync(path, \"utf8\");\n}",
    ),
    (
        "_write_file",
        "function _write_file(path, content) {\n  if (typeof path !== \"string\") throw new TypeError(\"write-file expects a str path\");\n  if (typeof content !== \"string\") throw new TypeError(\"write-file expects str content\");\n  require(\"fs\").writeFileSync(path, content);\n}",
    ),
    (
        "_append_file",
        "function _append_file(path, content) {\n  if (typeof path !== \"string\") throw new TypeError(\"append-file expects a str path\");\n  if (typeof content !== \"string\") throw new TypeError(\"append-file expects str content\");\n  require(\"fs\").appendFileSync(path, content);\n}",
    ),
    // ---- Tier 1 file I/O ----
    // The path helpers implement AINL's own rules rather than delegating to
    // node's `path`, because the hosts disagree on every edge case that
    // matters — path.join("a", "", "b") is "a/b" but path.join("a//b","d")
    // collapses a duplicate separator the interpreter preserves, and
    // path.dirname("x") is "." for a different reason than POSIX gives it.
    // See the measured table in ainl-core/src/eval.rs and docs/SYNTAX.md
    // "Path functions".
    (
        "_file_exists",
        "function _file_exists(path) {\n  if (typeof path !== \"string\") throw new TypeError(\"file-exists expects a str path\");\n  // lstatSync, not existsSync: existsSync follows the link, so a broken\n  // symlink would read as absent where the interpreter and the C runtime\n  // (both lstat) report it as present.\n  const p = path.length > 1 && path.endsWith(\"/\") ? path.slice(0, -1) : path;\n  try {\n    require(\"fs\").lstatSync(p);\n    return true;\n  } catch (e) {\n    return null;\n  }\n}",
    ),
    (
        "_delete_file",
        "function _delete_file(path) {\n  if (typeof path !== \"string\") throw new TypeError(\"delete-file expects a str path\");\n  const fs = require(\"fs\");\n  let st;\n  try {\n    st = fs.lstatSync(path);\n  } catch (e) {\n    throw new Error(`delete-file: cannot delete '${path}'`);\n  }\n  if (st.isDirectory()) throw new Error(`delete-file: cannot delete '${path}': it is a directory`);\n  fs.unlinkSync(path);\n}",
    ),
    (
        "_list_dir",
        "function _list_dir(path) {\n  if (typeof path !== \"string\") throw new TypeError(\"list-dir expects a str path\");\n  let names;\n  try {\n    names = require(\"fs\").readdirSync(path);\n  } catch (e) {\n    throw new Error(`list-dir: cannot read '${path}'`);\n  }\n  // Sort by UTF-8 *bytes*, not by JS string order: Array#sort compares UTF-16\n  // code units, which orders supplementary-plane characters (emoji, CJK\n  // extension B) before U+E000..U+FFFF, while the interpreter sorts by Rust's\n  // str Ord (byte order) and the C runtime by unsigned-byte order. Buffer\n  // comparison is byte order, so the two agree.\n  return names\n    .filter((n) => n !== \".\" && n !== \"..\")\n    .sort((a, b) => Buffer.compare(Buffer.from(a, \"utf8\"), Buffer.from(b, \"utf8\")));\n}",
    ),
    (
        "_path_canonical",
        "function _path_canonical(p) {\n  const absolute = p.startsWith(\"/\");\n  const segs = p.split(\"/\").filter((s) => s !== \"\" && s !== \".\");\n  if (p.endsWith(\"/.\")) segs.push(\".\");\n  const out = segs.join(\"/\");\n  return absolute ? \"/\" + out : out;\n}",
    ),
    (
        "_path_join",
        "function _path_join(...parts) {\n  if (!parts.length) throw new Error(\"path-join expects at least 1 argument\");\n  for (let i = 0; i < parts.length; i++) {\n    if (typeof parts[i] !== \"string\") throw new TypeError(`path-join expects str parts, got ${_ainl_tname(parts[i])} at position ${i + 1}`);\n  }\n  return _path_canonical(parts.join(\"/\"));\n}",
    ),
    (
        "_path_base",
        "function _path_base(path) {\n  if (typeof path !== \"string\") throw new TypeError(\"path-base expects a str path\");\n  const c = _path_canonical(path);\n  if (!c) return \"\";\n  const i = c.lastIndexOf(\"/\");\n  const name = i === -1 ? c : c.slice(i + 1);\n  return name;\n}",
    ),
    (
        "_path_dir",
        "function _path_dir(path) {\n  if (typeof path !== \"string\") throw new TypeError(\"path-dir expects a str path\");\n  const c = _path_canonical(path);\n  const i = c.lastIndexOf(\"/\");\n  if (i === -1) return \".\";\n  if (i === 0) return \"/\";\n  return c.slice(0, i);\n}",
    ),
    // The AINL type name for a value, for the position-reporting type errors
    // `path-join` raises. Kept tiny and explicit: the interpreter's wording
    // ("got int at position 2") is part of the 4-backend contract, and JS's own
    // typeof would say "number" for both an int and a float.
    (
        "_ainl_tname",
        "function _ainl_tname(x) {\n  if (x === null || x === undefined) return \"nil\";\n  if (typeof x === \"boolean\") return \"bool\";\n  if (typeof x === \"number\") return Number.isInteger(x) ? \"int\" : \"float\";\n  if (typeof x === \"string\") return \"str\";\n  if (Array.isArray(x)) return \"list\";\n  if (x instanceof _Hash) return \"hash\";\n  if (x instanceof _Sym) return \"sym\";\n  if (typeof x === \"function\") return \"fn\";\n  return \"?\";\n}",
    ),
    (
        "_split",
        "function _split(s, sep) {\n  if (typeof s !== \"string\") throw new TypeError(\"split expects a str\");\n  if (typeof sep !== \"string\") throw new TypeError(\"split expects a str\");\n  if (sep === \"\") throw new Error(\"split expects a non-empty separator\");\n  return s.split(sep);\n}",
    ),
    (
        "_join",
        "function _join(xs, sep) {\n  if (!Array.isArray(xs)) throw new TypeError(\"join expects a list\");\n  if (typeof sep !== \"string\") throw new TypeError(\"join expects a str separator\");\n  for (const x of xs) if (typeof x !== \"string\") throw new TypeError(\"join expects a list of str\");\n  return xs.join(sep);\n}",
    ),
    (
        "_trim",
        "function _trim(s) {\n  if (typeof s !== \"string\") throw new TypeError(\"trim expects a str\");\n  return s.replace(/^[ \\t\\n\\r\\x0b\\x0c]+|[ \\t\\n\\r\\x0b\\x0c]+$/g, \"\");\n}",
    ),
    (
        "_replace",
        "function _replace(s, old, neu) {\n  if (typeof s !== \"string\") throw new TypeError(\"replace expects a str\");\n  if (typeof old !== \"string\") throw new TypeError(\"replace expects a str\");\n  if (typeof neu !== \"string\") throw new TypeError(\"replace expects a str\");\n  if (old === \"\") throw new Error(\"replace expects a non-empty target\");\n  return s.split(old).join(neu);\n}",
    ),
    (
        "_upcase",
        "function _upcase(s) {\n  if (typeof s !== \"string\") throw new TypeError(\"upcase expects a str\");\n  return s.replace(/[a-z]/g, c => String.fromCharCode(c.charCodeAt(0) - 32));\n}",
    ),
    (
        "_downcase",
        "function _downcase(s) {\n  if (typeof s !== \"string\") throw new TypeError(\"downcase expects a str\");\n  return s.replace(/[A-Z]/g, c => String.fromCharCode(c.charCodeAt(0) + 32));\n}",
    ),
    (
        "_contains",
        "function _contains(hay, needle) {\n  if (typeof hay !== \"string\") throw new TypeError(\"contains expects a str\");\n  if (typeof needle !== \"string\") throw new TypeError(\"contains expects a str\");\n  return hay.indexOf(needle) !== -1;\n}",
    ),
    (
        "_env_get",
        "function _env_get(name) {\n  if (typeof name !== \"string\") throw new TypeError(\"env-get expects a str\");\n  const v = process.env[name];\n  return v === undefined ? null : v;\n}",
    ),
    (
        "_exit",
        "function _exit(code) {\n  if (typeof code !== \"number\") throw new TypeError(\"exit expects an int\");\n  process.exit(code);\n}",
    ),
    (
        "_now",
        "function _now() {\n  return Math.floor(Date.now() / 1000);\n}",
    ),
    (
        "_sleep",
        "function _sleep(secs) {\n  if (!_isnum(secs)) throw new TypeError(\"sleep expects a number\");\n  if (Number.isNaN(secs) || secs < 0) throw new Error(\"sleep expects a non-negative number\");\n  const shared = new Int32Array(new SharedArrayBuffer(4));\n  Atomics.wait(shared, 0, 0, secs * 1000);\n}",
    ),
    (
        "_abs",
        "function _abs(n) {\n  if (!_isnum(n)) throw new TypeError(\"abs expects a number\");\n  return Math.abs(n);\n}",
    ),
    (
        "_minmax",
        "function _minmax(xs, wantMax) {\n  if (!xs.length) throw new TypeError(wantMax ? \"max expects at least 1 argument\" : \"min expects at least 1 argument\");\n  let best = xs[0];\n  for (const x of xs) {\n    if (!_isnum(x)) throw new TypeError(wantMax ? \"max expects a number\" : \"min expects a number\");\n    if (wantMax ? x > best : x < best) best = x;\n  }\n  return best;\n}",
    ),
    ("_min", "function _min(...xs) { return _minmax(xs, false); }"),
    ("_max", "function _max(...xs) { return _minmax(xs, true); }"),
    (
        "_floor",
        "function _floor(n) {\n  if (!_isnum(n)) throw new TypeError(\"floor expects a number\");\n  return Math.floor(n);\n}",
    ),
    (
        "_sqrt",
        "function _sqrt(n) {\n  if (!_isnum(n)) throw new TypeError(\"sqrt expects a number\");\n  if (n < 0) throw new Error(\"sqrt expects a non-negative number\");\n  return Math.sqrt(n);\n}",
    ),
];
