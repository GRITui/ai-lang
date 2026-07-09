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
        if disp_used || self.needed.contains("_sym") {
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
        for (name, val) in let_bindings(binds_node)? {
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

    fn expr_list(&mut self, items: &[Node], span: ainl_core::Span) -> Result<String> {
        let Some(head) = items.first() else {
            return Ok("null".to_string());
        };
        let args = &items[1..];
        if let Node::Sym(op, _) = head {
            match op.as_str() {
                "+" => return self.infix(args, "+", "0"),
                "*" => return self.infix(args, "*", "1"),
                "-" => return self.infix_sub(args),
                "/" => return self.infix_div(args),
                "=" => return self.cmp(args, "==="),
                "<" => return self.cmp(args, "<"),
                ">" => return self.cmp(args, ">"),
                "<=" => return self.cmp(args, "<="),
                ">=" => return self.cmp(args, ">="),
                "and" => return self.logic(args, "&&", "true"),
                "or" => return self.logic(args, "||", "false"),
                "not" => return self.unary(args, "!"),
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
                "error" => return self.call_builtin("_error", args, Some("_error")),
                _ => {}
            }
        }
        let callee = self.expr(head)?;
        let parts = self.expr_all(args)?;
        Ok(format!("{callee}({})", parts.join(", ")))
    }

    fn expr_all(&mut self, nodes: &[Node]) -> Result<Vec<String>> {
        nodes.iter().map(|n| self.expr(n)).collect()
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

    fn infix(&mut self, args: &[Node], op: &str, identity: &str) -> Result<String> {
        let parts = self.expr_all(args)?;
        match parts.len() {
            0 => Ok(identity.to_string()),
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

    /// JS has no chained comparison, so expand `(< a b c)` to `(a < b && b < c)`.
    fn cmp(&mut self, args: &[Node], op: &str) -> Result<String> {
        if args.len() < 2 {
            return Ok("true".to_string());
        }
        let parts = self.expr_all(args)?;
        let clauses: Vec<String> = parts
            .windows(2)
            .map(|w| format!("{} {op} {}", w[0], w[1]))
            .collect();
        Ok(format!("({})", clauses.join(" && ")))
    }

    fn logic(&mut self, args: &[Node], op: &str, identity: &str) -> Result<String> {
        let parts = self.expr_all(args)?;
        match parts.len() {
            0 => Ok(identity.to_string()),
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
        for (n, v) in let_bindings(binds_node)? {
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
            out.push(format!("...{}", sanitize(rest)));
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
    (
        "_disp",
        "function _disp(x) {\n  if (x === true) return \"true\";\n  if (x === false) return \"false\";\n  if (x === null || x === undefined) return \"nil\";\n  if (x instanceof _Sym) return x.name;\n  if (Array.isArray(x)) return \"(\" + x.map(_repr).join(\" \") + \")\";\n  return String(x);\n}",
    ),
    (
        "_repr",
        "function _repr(x) {\n  return typeof x === \"string\" ? '\"' + x + '\"' : _disp(x);\n}",
    ),
    ("_print", "function _print(...xs) { console.log(xs.map(_disp).join(\" \")); }"),
    ("_str", "function _str(...xs) { return xs.map(_disp).join(\"\"); }"),
    ("_len", "function _len(x) { return x.length; }"),
    ("_first", "function _first(x) { return x.length ? x[0] : null; }"),
    ("_rest", "function _rest(x) { return x.slice(1); }"),
    ("_nth", "function _nth(x, i) { return (0 <= i && i < x.length) ? x[i] : null; }"),
    ("_cons", "function _cons(h, t) { return [h].concat(t); }"),
    ("_push", "function _push(t, ...xs) { return t.concat(xs); }"),
    ("_error", "function _error(...xs) { throw new Error(xs.map(_disp).join(\" \")); }"),
];
