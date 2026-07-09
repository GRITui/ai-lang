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
        if disp_used || self.needed.contains("_sym") {
            self.needed.insert("_Sym");
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
        for (name, val) in let_bindings(binds_node)? {
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

    fn expr_list(&mut self, items: &[Node], span: ainl_core::Span) -> Result<String> {
        let Some(head) = items.first() else {
            return Ok("None".to_string()); // empty list evaluates to nil
        };
        let args = &items[1..];
        if let Node::Sym(op, _) = head {
            match op.as_str() {
                "+" => return self.infix(args, "+", Some("0")),
                "*" => return self.infix(args, "*", Some("1")),
                "-" => return self.infix_sub(args),
                "/" => return self.infix_div(args),
                "=" => return self.chain(args, "=="),
                "<" => return self.chain(args, "<"),
                ">" => return self.chain(args, ">"),
                "<=" => return self.chain(args, "<="),
                ">=" => return self.chain(args, ">="),
                "and" => return self.chain_logic(args, "and"),
                "or" => return self.chain_logic(args, "or"),
                "not" => return self.unary(args, "not "),
                "mod" => return self.binary(args, "%"),
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
                "error" => return self.call_builtin("_error", args, Some("_error")),
                _ => {}
            }
        }
        // General call: evaluate the callee then apply.
        let callee = self.expr(head)?;
        let parts = self.expr_all(args)?;
        Ok(format!("{callee}({})", parts.join(", ")))
    }

    fn expr_all(&mut self, nodes: &[Node]) -> Result<Vec<String>> {
        nodes.iter().map(|n| self.expr(n)).collect()
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

    fn infix(&mut self, args: &[Node], op: &str, identity: Option<&str>) -> Result<String> {
        let parts = self.expr_all(args)?;
        match parts.len() {
            0 => Ok(identity.unwrap_or("None").to_string()),
            1 => Ok(parts.into_iter().next().unwrap()),
            _ => Ok(format!("({})", parts.join(&format!(" {op} ")))),
        }
    }

    fn infix_sub(&mut self, args: &[Node]) -> Result<String> {
        let parts = self.expr_all(args)?;
        match parts.len() {
            0 => Err(Error::runtime("- expects at least 1 argument")),
            1 => Ok(format!("(-{})", parts[0])),
            _ => Ok(format!("({})", parts.join(" - "))),
        }
    }

    fn infix_div(&mut self, args: &[Node]) -> Result<String> {
        let parts = self.expr_all(args)?;
        match parts.len() {
            0 => Err(Error::runtime("/ expects at least 1 argument")),
            1 => Ok(format!("(1 / {})", parts[0])),
            _ => Ok(format!("({})", parts.join(" / "))),
        }
    }

    fn binary(&mut self, args: &[Node], op: &str) -> Result<String> {
        let [a, b] = args else {
            return Err(Error::runtime(format!("'{op}' expects 2 arguments")));
        };
        Ok(format!("({} {op} {})", self.expr(a)?, self.expr(b)?))
    }

    fn unary(&mut self, args: &[Node], op: &str) -> Result<String> {
        let [a] = args else {
            return Err(Error::runtime("expects 1 argument"));
        };
        Ok(format!("({op}{})", self.expr(a)?))
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
        let binds = let_bindings(binds_node)?;
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

// ---- shared helpers --------------------------------------------------------

fn python_params(params_node: &Node) -> Result<String> {
    let Node::List(param_nodes, _) = params_node else {
        return Err(Error::runtime("fn params must be a list"));
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < param_nodes.len() {
        let Node::Sym(p, _) = &param_nodes[i] else {
            return Err(Error::runtime("fn params must be symbols"));
        };
        if p == "&" {
            let Some(Node::Sym(rest, _)) = param_nodes.get(i + 1) else {
                return Err(Error::runtime("'&' must be followed by a rest parameter"));
            };
            out.push(format!("*{}", sanitize(rest)));
            break;
        }
        out.push(sanitize(p));
        i += 1;
    }
    Ok(out.join(", "))
}

fn let_bindings(binds_node: &Node) -> Result<Vec<(&str, &Node)>> {
    let Node::List(binds, _) = binds_node else {
        return Err(Error::runtime("let bindings must be a list"));
    };
    let mut out = Vec::new();
    for b in binds {
        let Node::List(pair, _) = b else {
            return Err(Error::runtime("each let binding must be (name value)"));
        };
        let [Node::Sym(name, _), val] = &pair[..] else {
            return Err(Error::runtime("each let binding must be (name value)"));
        };
        out.push((name.as_str(), val));
    }
    Ok(out)
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
    (
        "_disp",
        "def _disp(x):\n    if isinstance(x, _Sym): return str.__str__(x)\n    if x is True: return 'true'\n    if x is False: return 'false'\n    if x is None: return 'nil'\n    if isinstance(x, list): return '(' + ' '.join(_repr(e) for e in x) + ')'\n    if isinstance(x, float): return ('%.1f' % x) if x.is_integer() else repr(x)\n    return str(x)",
    ),
    (
        "_repr",
        "def _repr(x):\n    if isinstance(x, _Sym): return str.__str__(x)\n    return '\"' + x + '\"' if isinstance(x, str) else _disp(x)",
    ),
    ("_print", "def _print(*xs):\n    print(' '.join(_disp(x) for x in xs))"),
    ("_str", "def _str(*xs):\n    return ''.join(_disp(x) for x in xs)"),
    ("_first", "def _first(x):\n    return x[0] if len(x) else None"),
    ("_rest", "def _rest(x):\n    return list(x[1:])"),
    ("_nth", "def _nth(x, i):\n    return x[i] if 0 <= i < len(x) else None"),
    ("_cons", "def _cons(h, t):\n    return [h] + list(t)"),
    ("_push", "def _push(t, *xs):\n    return list(t) + list(xs)"),
    ("_error", "def _error(*xs):\n    raise RuntimeError(' '.join(_disp(x) for x in xs))"),
];
