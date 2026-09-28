//! AINL AST → Python source projection.
//!
//! AINL is expression-oriented; Python mixes expressions and statements. The
//! transpiler emits Python in two contexts:
//!
//! * **expression context** ([`Py::expr`]) — for forms that have a natural
//!   Python expression (`if`→conditional expression, chained comparisons,
//!   `fn`→`lambda`, etc.). Forms with no expression form (e.g. `while`) error
//!   here with the offending source span.
//! * **statement context** ([`Py::stmt`]) — for bodies and the module top level,
//!   where `def`/`while`/`let`/`do`/`if` lower to real Python statements and a
//!   function's tail form is `return`ed.
//!
//! Only the runtime helpers actually used are emitted, keeping the output
//! readable. A small source-map comment precedes each top-level definition.

use crate::shared::{self, ExprEmit};
use ainl_core::parser::Node;
use ainl_core::serialize::LineIndex;
use ainl_core::{Error, Result};
use std::collections::BTreeSet;

/// Parse and transpile a source string to Python.
pub fn transpile_python_src(src: &str) -> Result<String> {
    let forms = ainl_core::parse(src)?;
    transpile_python(&forms, src)
}

/// Transpile already-parsed forms to Python. `src` is used only for source-map
/// line comments.
pub fn transpile_python(forms: &[Node], src: &str) -> Result<String> {
    let idx = LineIndex::new(src);
    let mut py = Py {
        body: String::new(),
        indent: 0,
        needed: BTreeSet::new(),
    };
    for form in forms {
        py.top_form(form, &idx)?;
    }
    Ok(py.finish())
}

struct Py {
    body: String,
    indent: usize,
    needed: BTreeSet<&'static str>,
}

impl Py {
    // -- output helpers ------------------------------------------------------

    fn line(&mut self, s: &str) {
        for _ in 0..self.indent {
            self.body.push_str("    ");
        }
        self.body.push_str(s);
        self.body.push('\n');
    }

    fn need(&mut self, name: &'static str) {
        self.needed.insert(name);
    }

    fn finish(mut self) -> String {
        // Resolve runtime dependencies: the display cluster references `_Sym`,
        // and `_sym` (from quoted symbols) needs the `_Sym` class.
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
        // `_disp` branches on `isinstance(x, _Hash)`; `_hash`/`_assoc` construct one.
        if disp_used || self.needed.iter().any(|n| matches!(*n, "_hash" | "_assoc")) {
            self.needed.insert("_Hash");
        }
        if disp_used || self.needed.contains("_sym") || self.needed.contains("_eq") {
            self.needed.insert("_Sym");
        }
        // Stage 3.1 stdlib dependencies: `_join` distinguishes a quoted symbol
        // from an equal-content string, and `_min`/`_max` share one fold helper.
        if self.needed.contains("_join") {
            self.needed.insert("_Sym");
        }
        if self.needed.contains("_min") || self.needed.contains("_max") {
            self.needed.insert("_minmax");
        }
        let mut out = String::new();
        out.push_str("# Transpiled from AINL by `ainl transpile --to python`.\n");
        out.push_str("# Generated code: edit the .ainl source, not this file.\n\n");
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

    // -- top level -----------------------------------------------------------

    fn top_form(&mut self, form: &Node, idx: &LineIndex) -> Result<()> {
        // A source-map comment tying the projected code back to the AINL line.
        if let Node::List(items, _) = form {
            if let Some(Node::Sym(op, _)) = items.first() {
                if op == "def" {
                    let (line, _) = idx.locate(form.span().start);
                    if let Some(Node::Sym(name, _)) = items.get(1) {
                        self.line(&format!("# ainl:{line}  {name}"));
                    }
                }
            }
        }
        self.stmt(form, false)
    }

    // -- statement context ---------------------------------------------------

    /// Emit `form` as one or more statements. When `ret` is true the form is a
    /// function tail and its value is `return`ed.
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
        // Any other form is an expression in statement position.
        let e = self.expr(form)?;
        if ret {
            self.line(&format!("return {e}"));
        } else {
            self.line(&e);
        }
        Ok(())
    }

    /// Emit a body (sequence of forms): all but the last as plain statements,
    /// the last honoring `ret`.
    fn stmt_body(&mut self, forms: &[Node], ret: bool) -> Result<()> {
        if forms.is_empty() {
            if ret {
                self.line("return None");
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
        // `(def f (fn (..) ..))` becomes a real Python `def`.
        if let Node::List(fitems, _) = val {
            if matches!(fitems.first(), Some(Node::Sym(s, _)) if s == "fn") {
                self.def_function(&name, &fitems[1..])?;
                if ret {
                    self.line(&format!("return {name}"));
                }
                return Ok(());
            }
        }
        let e = self.expr(val)?;
        self.line(&format!("{name} = {e}"));
        if ret {
            self.line(&format!("return {name}"));
        }
        Ok(())
    }

    fn def_function(&mut self, name: &str, fn_args: &[Node]) -> Result<()> {
        let Some((params_node, body)) = fn_args.split_first() else {
            return Err(Error::runtime("fn expects (fn (params...) body...)"));
        };
        let params = python_params(params_node)?;
        self.line(&format!("def {name}({params}):"));
        self.indent += 1;
        if body.is_empty() {
            self.line("return None");
        } else {
            self.stmt_body(body, true)?;
        }
        self.indent -= 1;
        Ok(())
    }

    fn stmt_while(&mut self, args: &[Node], ret: bool) -> Result<()> {
        let Some((cond, body)) = args.split_first() else {
            return Err(Error::runtime("while expects (while cond body...)"));
        };
        let c = self.expr(cond)?;
        self.line(&format!("while {c}:"));
        self.indent += 1;
        if body.is_empty() {
            self.line("pass");
        } else {
            for f in body {
                self.stmt(f, false)?;
            }
        }
        self.indent -= 1;
        if ret {
            self.line("return None");
        }
        Ok(())
    }

    fn stmt_let(&mut self, args: &[Node], ret: bool) -> Result<()> {
        let Some((binds_node, body)) = args.split_first() else {
            return Err(Error::runtime("let expects (let ((n v)...) body...)"));
        };
        for (name, val) in shared::let_bindings(binds_node)? {
            let e = self.expr(val)?;
            self.line(&format!("{} = {e}", sanitize(name)));
        }
        self.stmt_body(body, ret)
    }

    fn stmt_if(&mut self, args: &[Node], ret: bool) -> Result<()> {
        match args {
            [cond, then] => {
                let c = self.expr(cond)?;
                self.line(&format!("if {c}:"));
                self.indent += 1;
                self.stmt(then, ret)?;
                self.indent -= 1;
                if ret {
                    // AINL `if` with no else yields nil.
                    self.line("else:");
                    self.indent += 1;
                    self.line("return None");
                    self.indent -= 1;
                }
                Ok(())
            }
            [cond, then, els] => {
                let c = self.expr(cond)?;
                self.line(&format!("if {c}:"));
                self.indent += 1;
                self.stmt(then, ret)?;
                self.indent -= 1;
                self.line("else:");
                self.indent += 1;
                self.stmt(els, ret)?;
                self.indent -= 1;
                Ok(())
            }
            _ => Err(Error::runtime("if expects (if cond then [else])")),
        }
    }

    // -- expression context --------------------------------------------------

    fn expr_list(&mut self, items: &[Node], span: ainl_core::Span) -> Result<String> {
        let Some(head) = items.first() else {
            return Ok("None".to_string()); // empty list evaluates to nil
        };
        let args = &items[1..];
        if let Node::Sym(op, _) = head {
            match op.as_str() {
                "+" => return shared::infix(self, args, "+", "0"),
                "*" => return shared::infix(self, args, "*", "1"),
                "-" => return shared::infix_sub(self, args),
                "/" => return self.infix_div(args),
                "=" => return self.eq_chain(args),
                "<" => return self.chain(args, "<"),
                ">" => return self.chain(args, ">"),
                "<=" => return self.chain(args, "<="),
                ">=" => return self.chain(args, ">="),
                "and" => return self.chain_logic(args, "and"),
                "or" => return self.chain_logic(args, "or"),
                "not" => return shared::unary(self, args, "not "),
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
                "while" => {
                    return Err(self.no_expr("while", span));
                }
                "print" => {
                    self.need("_print");
                    return Ok(format!("_print({})", self.expr_all(args)?.join(", ")));
                }
                "str" => {
                    self.need("_str");
                    return Ok(format!("_str({})", self.expr_all(args)?.join(", ")));
                }
                "len" => return self.call_builtin("len", args, None),
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
                // The multi-arg / statement-shaped ones get bespoke arms; the
                // rest reuse call_builtin. Each maps to the host's own idiom
                // (open().read(), time.time(), os.getenv, math.sqrt) so the
                // generated code reads like Python, not like an AINL interpreter
                // in Python syntax. Where the host's own behavior would differ
                // from the interpreter's, the helper re-establishes the
                // interpreter's rule — see the RUNTIME table.
                "read-file" => return self.call_builtin("_read_file", args, Some("_read_file")),
                "write-file" => return self.call_builtin("_write_file", args, Some("_write_file")),
                "append-file" => {
                    return self.call_builtin("_append_file", args, Some("_append_file"))
                }
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
        // General call: evaluate the callee then apply.
        let callee = self.expr(head)?;
        let parts = self.expr_all(args)?;
        Ok(format!("{callee}({})", parts.join(", ")))
    }

    fn call_builtin(
        &mut self,
        py_name: &str,
        args: &[Node],
        need: Option<&'static str>,
    ) -> Result<String> {
        if let Some(n) = need {
            self.need(n);
        }
        Ok(format!("{py_name}({})", self.expr_all(args)?.join(", ")))
    }

    fn infix_div(&mut self, args: &[Node]) -> Result<String> {
        let parts = self.expr_all(args)?;
        match parts.len() {
            0 => Err(Error::runtime("/ expects at least 1 argument")),
            1 => Ok(format!("(1 / {})", parts[0])),
            _ => Ok(format!("({})", parts.join(" / "))),
        }
    }

    /// `=` needs the `_eq` runtime helper rather than native `==`: `_Sym` is
    /// implemented as a `str` subclass (so quoted-symbol values print bare),
    /// which makes bare `==` say `_Sym("a") == "a"` — wrongly conflating a
    /// quoted symbol with an equal-content string. `_eq` also recurses into
    /// lists so a symbol nested inside one gets the same treatment.
    fn eq_chain(&mut self, args: &[Node]) -> Result<String> {
        if args.len() < 2 {
            return Ok("True".to_string());
        }
        self.need("_eq");
        let parts = self.expr_all(args)?;
        let clauses: Vec<String> = parts
            .windows(2)
            .map(|w| format!("_eq({}, {})", w[0], w[1]))
            .collect();
        Ok(format!("({})", clauses.join(" and ")))
    }

    /// Chained comparison — Python supports `a < b < c` natively, matching AINL.
    fn chain(&mut self, args: &[Node], op: &str) -> Result<String> {
        if args.len() < 2 {
            return Ok("True".to_string());
        }
        let parts = self.expr_all(args)?;
        Ok(format!("({})", parts.join(&format!(" {op} "))))
    }

    fn chain_logic(&mut self, args: &[Node], op: &str) -> Result<String> {
        let parts = self.expr_all(args)?;
        match parts.len() {
            0 => Ok(if op == "and" { "True" } else { "False" }.to_string()),
            1 => Ok(parts.into_iter().next().unwrap()),
            _ => Ok(format!("({})", parts.join(&format!(" {op} ")))),
        }
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
            None => "None".to_string(),
        };
        Ok(format!("({t} if {c} else {e})"))
    }

    /// `let` in expression position becomes an immediately-invoked lambda, but
    /// only when the body is a single expression.
    fn expr_let(&mut self, args: &[Node], span: ainl_core::Span) -> Result<String> {
        let Some((binds_node, body)) = args.split_first() else {
            return Err(Error::runtime("let expects (let ((n v)...) body...)"));
        };
        if body.len() != 1 {
            return Err(self.no_expr("multi-statement let", span));
        }
        let binds = shared::let_bindings(binds_node)?;
        let mut names = Vec::new();
        let mut vals = Vec::new();
        for (n, v) in binds {
            names.push(sanitize(n));
            vals.push(self.expr(v)?);
        }
        let b = self.expr(&body[0])?;
        Ok(format!(
            "(lambda {}: {b})({})",
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
                "fn with a multi-statement body cannot be a Python lambda (bytes {}..{}); bind it with def",
                span.start, span.end
            )));
        }
        let params = python_params(params_node)?;
        let b = self.expr(&body[0])?;
        Ok(format!("(lambda {params}: {b})"))
    }

    fn expr_quote(&mut self, args: &[Node]) -> Result<String> {
        let [node] = args else {
            return Err(Error::runtime("quote expects one form"));
        };
        Ok(self.quote(node))
    }

    /// Project quoted data. A quoted symbol becomes a `_Sym` (a `str` subclass)
    /// so it renders bare like an AINL symbol rather than as a quoted string.
    fn quote(&mut self, node: &Node) -> String {
        match node {
            Node::Int(i, _) => i.to_string(),
            Node::Float(x, _) => python_float(*x),
            Node::Str(s, _) => python_str(s),
            Node::Sym(s, _) => {
                self.need("_sym");
                format!("_sym({})", python_str(s))
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

impl ExprEmit for Py {
    fn expr(&mut self, node: &Node) -> Result<String> {
        match node {
            Node::Int(i, _) => Ok(i.to_string()),
            Node::Float(x, _) => Ok(python_float(*x)),
            Node::Str(s, _) => Ok(python_str(s)),
            Node::Sym(name, _) => Ok(match name.as_str() {
                "true" => "True".to_string(),
                "false" => "False".to_string(),
                "nil" => "None".to_string(),
                other => sanitize(other),
            }),
            Node::List(items, _) => self.expr_list(items, node.span()),
        }
    }
}

// ---- shared helpers --------------------------------------------------------

fn python_params(params_node: &Node) -> Result<String> {
    Ok(shared::parse_params(params_node, "*", sanitize)?.join(", "))
}

/// Turn an AINL symbol into a valid Python identifier.
fn sanitize(name: &str) -> String {
    let mut s = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_alphanumeric() || c == '_' {
            s.push(c);
        } else {
            s.push('_');
        }
    }
    if s.is_empty() || s.chars().next().unwrap().is_ascii_digit() {
        s.insert(0, '_');
    }
    if is_python_keyword(&s) {
        s.push('_');
    }
    s
}

fn is_python_keyword(s: &str) -> bool {
    matches!(
        s,
        "False"
            | "None"
            | "True"
            | "and"
            | "as"
            | "assert"
            | "async"
            | "await"
            | "break"
            | "class"
            | "continue"
            | "def"
            | "del"
            | "elif"
            | "else"
            | "except"
            | "finally"
            | "for"
            | "from"
            | "global"
            | "if"
            | "import"
            | "in"
            | "is"
            | "lambda"
            | "nonlocal"
            | "not"
            | "or"
            | "pass"
            | "raise"
            | "return"
            | "try"
            | "while"
            | "with"
            | "yield"
    )
}

fn python_float(x: f64) -> String {
    if !x.is_finite() {
        return "float('nan')".to_string();
    }
    if x.fract() == 0.0 {
        format!("{x:.1}")
    } else {
        format!("{x}")
    }
}

fn python_str(s: &str) -> String {
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

/// Runtime shim functions, emitted only when used. Order here is the emission
/// order (dependencies first).
const RUNTIME: &[(&str, &str)] = &[
    ("_Sym", "class _Sym(str):\n    pass"),
    ("_sym", "def _sym(s):\n    return _Sym(s)"),
    // A map is a list of [k, v] pairs; this subclass exists only so `_disp`
    // can tell a hash apart from a plain list at print time (a print call
    // can't otherwise know a variable's AINL-level type).
    ("_Hash", "class _Hash(list):\n    pass"),
    (
        "_disp",
        "def _disp(x):\n    if isinstance(x, _Sym): return str.__str__(x)\n    if x is True: return 'true'\n    if x is False: return 'false'\n    if x is None: return 'nil'\n    if isinstance(x, _Hash): return '{' + ' '.join(_repr(p[0]) + ' ' + _repr(p[1]) for p in x) + '}'\n    if isinstance(x, list): return '(' + ' '.join(_repr(e) for e in x) + ')'\n    if isinstance(x, float): return ('%.1f' % x) if x.is_integer() else repr(x)\n    return str(x)",
    ),
    (
        "_repr",
        "def _repr(x):\n    if isinstance(x, _Sym): return str.__str__(x)\n    return '\"' + x + '\"' if isinstance(x, str) else _disp(x)",
    ),
    (
        "_eq",
        "def _eq(a, b):\n    if isinstance(a, list) and isinstance(b, list):\n        return len(a) == len(b) and all(_eq(x, y) for x, y in zip(a, b))\n    if isinstance(a, _Sym) != isinstance(b, _Sym):\n        return False\n    if isinstance(a, bool) != isinstance(b, bool):\n        return False\n    return a == b",
    ),
    ("_print", "def _print(*xs):\n    print(' '.join(_disp(x) for x in xs))"),
    ("_str", "def _str(*xs):\n    return ''.join(_disp(x) for x in xs)"),
    ("_first", "def _first(x):\n    return x[0] if len(x) else None"),
    ("_rest", "def _rest(x):\n    return list(x[1:])"),
    ("_nth", "def _nth(x, i):\n    return x[i] if 0 <= i < len(x) else None"),
    ("_cons", "def _cons(h, t):\n    return [h] + list(t)"),
    ("_push", "def _push(t, *xs):\n    return list(t) + list(xs)"),
    (
        "_hash",
        "def _hash(*kvs):\n    out = _Hash()\n    for i in range(0, len(kvs), 2):\n        k, v = kvs[i], kvs[i + 1]\n        for pair in out:\n            if _eq(pair[0], k):\n                pair[1] = v\n                break\n        else:\n            out.append([k, v])\n    return out",
    ),
    (
        "_get",
        "def _get(h, k):\n    for pair in h:\n        if _eq(pair[0], k): return pair[1]\n    return None",
    ),
    (
        "_assoc",
        "def _assoc(h, k, v):\n    out = _Hash(list(p) for p in h)\n    for pair in out:\n        if _eq(pair[0], k):\n            pair[1] = v\n            return out\n    out.append([k, v])\n    return out",
    ),
    ("_has", "def _has(h, k):\n    return any(_eq(pair[0], k) for pair in h)"),
    ("_keys", "def _keys(h):\n    return [pair[0] for pair in h]"),
    ("_vals", "def _vals(h):\n    return [pair[1] for pair in h]"),
    ("_error", "def _error(*xs):\n    raise RuntimeError(' '.join(_disp(x) for x in xs))"),
    // ---- Stage 3.1 stdlib ----
    // Each helper re-establishes the *interpreter's* rule where Python's own
    // behavior would differ, so all four backends agree:
    //   * split/replace reject an empty target instead of Python's
    //     ValueError / per-position insertion.
    //   * trim strips the ASCII set only (str.strip() also removes U+00A0 and
    //     friends, which the C runtime and the other two targets do not).
    //   * upcase/downcase fold ASCII only (str.upper() is Unicode-aware).
    //   * sqrt/sleep reject negatives instead of returning NaN / raising
    //     ValueError from time.sleep.
    //   * floor returns an int, and passes an int straight through so a large
    //     i64 is not round-tripped through a float.
    //   * join requires a list of str, so a non-string element is a defined
    //     error rather than Python's silent str() coercion.
    (
        "_read_file",
        "def _read_file(path):\n    with open(path, 'r') as f:\n        return f.read()",
    ),
    (
        "_write_file",
        "def _write_file(path, content):\n    with open(path, 'w') as f:\n        f.write(content)",
    ),
    (
        "_append_file",
        "def _append_file(path, content):\n    with open(path, 'a') as f:\n        f.write(content)",
    ),
    (
        "_split",
        "def _split(s, sep):\n    if not sep: raise ValueError('split expects a non-empty separator')\n    return s.split(sep)",
    ),
    (
        "_join",
        "def _join(xs, sep):\n    for x in xs:\n        if not isinstance(x, str) or isinstance(x, _Sym): raise TypeError('join expects a list of str')\n    return sep.join(xs)",
    ),
    (
        "_trim",
        "def _trim(s):\n    return s.strip(' \\t\\n\\r\\x0b\\x0c')",
    ),
    (
        "_replace",
        "def _replace(s, old, new):\n    if not old: raise ValueError('replace expects a non-empty target')\n    return s.replace(old, new)",
    ),
    (
        "_upcase",
        "def _upcase(s):\n    return ''.join(chr(ord(c) - 32) if 'a' <= c <= 'z' else c for c in s)",
    ),
    (
        "_downcase",
        "def _downcase(s):\n    return ''.join(chr(ord(c) + 32) if 'A' <= c <= 'Z' else c for c in s)",
    ),
    (
        "_contains",
        "def _contains(hay, needle):\n    return needle in hay",
    ),
    ("_env_get", "def _env_get(name):\n    import os\n    return os.environ.get(name)"),
    (
        "_exit",
        "def _exit(code):\n    import sys\n    sys.stdout.flush()\n    raise SystemExit(code)",
    ),
    (
        "_now",
        "def _now():\n    import time\n    return int(time.time())",
    ),
    (
        "_sleep",
        "def _sleep(secs):\n    import time\n    if secs != secs or secs < 0: raise ValueError('sleep expects a non-negative number')\n    if secs > 0: time.sleep(secs)",
    ),
    (
        "_abs",
        "def _abs(n):\n    if not isinstance(n, (int, float)) or isinstance(n, bool): raise TypeError('abs expects a number')\n    return n if n >= 0 else -n",
    ),
    (
        "_min",
        "def _min(*xs):\n    if not xs: raise TypeError('min expects at least 1 argument')\n    return _minmax(xs, False)",
    ),
    (
        "_max",
        "def _max(*xs):\n    if not xs: raise TypeError('max expects at least 1 argument')\n    return _minmax(xs, True)",
    ),
    (
        "_minmax",
        "def _minmax(xs, want_max):\n    best = xs[0]\n    for x in xs[1:]:\n        if not isinstance(x, (int, float)) or isinstance(x, bool): raise TypeError('min/max expects a number')\n        if (x > best) if want_max else (x < best): best = x\n    return best",
    ),
    (
        "_floor",
        "def _floor(n):\n    import math\n    if isinstance(n, bool) or not isinstance(n, (int, float)): raise TypeError('floor expects a number')\n    return n if isinstance(n, int) else math.floor(n)",
    ),
    (
        "_sqrt",
        "def _sqrt(n):\n    import math\n    if isinstance(n, bool) or not isinstance(n, (int, float)): raise TypeError('sqrt expects a number')\n    if n < 0: raise ValueError('sqrt expects a non-negative number')\n    return math.sqrt(n)",
    ),
];
