//! AINL AST → Ruby source projection.
//!
//! Ruby is expression-oriented — a method/block returns its last expression —
//! which matches AINL closely, so no explicit `return` machinery is needed: the
//! last form of a body is naturally its value.
//!
//! Key modelling choice: **every AINL function becomes a Ruby lambda** and every
//! non-builtin call uses `.call`. This avoids Ruby's method-vs-lambda calling
//! split, so higher-order functions (`map`), closures, and recursion all work
//! uniformly. Ruby keeps Integer/Float distinct like AINL; `/` uses `.to_f` to
//! preserve AINL's float division. `if` is a Ruby expression; chained
//! comparison is expanded (Ruby, like JS, has none).

use crate::shared::{self, ExprEmit};
use ainl_core::parser::Node;
use ainl_core::serialize::LineIndex;
use ainl_core::{Error, Result};
use std::collections::BTreeSet;

pub fn transpile_ruby_src(src: &str) -> Result<String> {
    let forms = ainl_core::parse(src)?;
    transpile_ruby(&forms, src)
}

pub fn transpile_ruby(forms: &[Node], src: &str) -> Result<String> {
    let idx = LineIndex::new(src);
    let mut rb = Rb {
        body: String::new(),
        indent: 0,
        needed: BTreeSet::new(),
    };
    for form in forms {
        rb.top_form(form, &idx)?;
    }
    Ok(rb.finish())
}

struct Rb {
    body: String,
    indent: usize,
    needed: BTreeSet<&'static str>,
}

impl Rb {
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
        if self
            .needed
            .iter()
            .any(|n| matches!(*n, "_print" | "_str" | "_error" | "_repr" | "_disp"))
        {
            self.needed.insert("_disp");
            self.needed.insert("_repr");
        }
        let mut out = String::new();
        out.push_str("# Transpiled from AINL by `ainl transpile --to ruby`.\n");
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

    // -- top level & statements ---------------------------------------------

    fn top_form(&mut self, form: &Node, idx: &LineIndex) -> Result<()> {
        if let Node::List(items, _) = form {
            if let (Some(Node::Sym(op, _)), Some(Node::Sym(name, _))) =
                (items.first(), items.get(1))
            {
                if op == "def" {
                    let (line, _) = idx.locate(form.span().start);
                    self.line(&format!("# ainl:{line}  {name}"));
                }
            }
        }
        self.stmt(form)
    }

    /// Emit a form as a statement. Ruby returns the last expression of a block,
    /// so no explicit return is threaded.
    fn stmt(&mut self, form: &Node) -> Result<()> {
        if let Node::List(items, _) = form {
            if let Some(Node::Sym(op, _)) = items.first() {
                match op.as_str() {
                    "def" => return self.stmt_def(&items[1..]),
                    "while" => return self.stmt_while(&items[1..]),
                    "let" => return self.stmt_let(&items[1..]),
                    "do" => return self.stmt_body(&items[1..]),
                    "if" => return self.stmt_if(&items[1..]),
                    _ => {}
                }
            }
        }
        let e = self.expr(form)?;
        self.line(&e);
        Ok(())
    }

    fn stmt_body(&mut self, forms: &[Node]) -> Result<()> {
        if forms.is_empty() {
            self.line("nil");
            return Ok(());
        }
        for f in forms {
            self.stmt(f)?;
        }
        Ok(())
    }

    fn stmt_def(&mut self, args: &[Node]) -> Result<()> {
        let [name_node, val] = args else {
            return Err(Error::runtime("def expects (def name value)"));
        };
        let Node::Sym(raw, _) = name_node else {
            return Err(Error::runtime("def name must be a symbol"));
        };
        let name = sanitize(raw);
        if let Node::List(fitems, _) = val {
            if matches!(fitems.first(), Some(Node::Sym(s, _)) if s == "fn") {
                return self.def_lambda(&name, &fitems[1..]);
            }
        }
        let e = self.expr(val)?;
        self.line(&format!("{name} = {e}"));
        Ok(())
    }

    fn def_lambda(&mut self, name: &str, fn_args: &[Node]) -> Result<()> {
        let Some((params_node, body)) = fn_args.split_first() else {
            return Err(Error::runtime("fn expects (fn (params...) body...)"));
        };
        let params = ruby_params(params_node)?;
        if params.is_empty() {
            self.line(&format!("{name} = lambda do"));
        } else {
            self.line(&format!("{name} = lambda do |{}|", params.join(", ")));
        }
        self.indent += 1;
        if body.is_empty() {
            self.line("nil");
        } else {
            self.stmt_body(body)?;
        }
        self.indent -= 1;
        self.line("end");
        Ok(())
    }

    fn stmt_while(&mut self, args: &[Node]) -> Result<()> {
        let Some((cond, body)) = args.split_first() else {
            return Err(Error::runtime("while expects (while cond body...)"));
        };
        let c = self.expr(cond)?;
        self.line(&format!("while {c}"));
        self.indent += 1;
        for f in body {
            self.stmt(f)?;
        }
        self.indent -= 1;
        self.line("end");
        Ok(())
    }

    fn stmt_let(&mut self, args: &[Node]) -> Result<()> {
        let Some((binds_node, body)) = args.split_first() else {
            return Err(Error::runtime("let expects (let ((n v)...) body...)"));
        };
        for (name, val) in shared::let_bindings(binds_node)? {
            let e = self.expr(val)?;
            self.line(&format!("{} = {e}", sanitize(name)));
        }
        self.stmt_body(body)
    }

    fn stmt_if(&mut self, args: &[Node]) -> Result<()> {
        match args {
            [cond, then] => {
                let c = self.expr(cond)?;
                self.line(&format!("if {c}"));
                self.indent += 1;
                self.stmt(then)?;
                self.indent -= 1;
                self.line("end");
                Ok(())
            }
            [cond, then, els] => {
                let c = self.expr(cond)?;
                self.line(&format!("if {c}"));
                self.indent += 1;
                self.stmt(then)?;
                self.indent -= 1;
                self.line("else");
                self.indent += 1;
                self.stmt(els)?;
                self.indent -= 1;
                self.line("end");
                Ok(())
            }
            _ => Err(Error::runtime("if expects (if cond then [else])")),
        }
    }

    // -- expressions ---------------------------------------------------------

    fn expr_list(&mut self, items: &[Node], span: ainl_core::Span) -> Result<String> {
        let Some(head) = items.first() else {
            return Ok("nil".to_string());
        };
        let args = &items[1..];
        if let Node::Sym(op, _) = head {
            match op.as_str() {
                "+" => return shared::infix(self, args, "+", "0"),
                "*" => return shared::infix(self, args, "*", "1"),
                "-" => return shared::infix_sub(self, args),
                "/" => return self.infix_div(args),
                "=" => return shared::cmp(self, args, "=="),
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
                "quote" => return Ok(self.quote(&items[1..][0])),
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
        // General call: every AINL function is a Ruby lambda, so use `.call`.
        let callee = self.expr(head)?;
        let parts = self.expr_all(args)?;
        Ok(format!("{callee}.call({})", parts.join(", ")))
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

    /// AINL `/` is float division; Ruby `/` on integers truncates, so coerce the
    /// first operand to Float to preserve AINL semantics.
    fn infix_div(&mut self, args: &[Node]) -> Result<String> {
        let parts = self.expr_all(args)?;
        match parts.len() {
            0 => Err(Error::runtime("/ expects at least 1 argument")),
            1 => Ok(format!("(1.0 / {})", parts[0])),
            _ => {
                let mut it = parts.into_iter();
                let first = it.next().unwrap();
                let rest: Vec<String> = it.collect();
                Ok(format!("(({}).to_f / {})", first, rest.join(" / ")))
            }
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
            None => "nil".to_string(),
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
        let params = if names.is_empty() {
            String::new()
        } else {
            format!("|{}|", names.join(", "))
        };
        Ok(format!(
            "lambda {{ {params} {b} }}.call({})",
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
                "fn with a multi-statement body cannot be an inline Ruby lambda (bytes {}..{}); bind it with def",
                span.start, span.end
            )));
        }
        let params = ruby_params(params_node)?;
        let b = self.expr(&body[0])?;
        if params.is_empty() {
            Ok(format!("-> {{ {b} }}"))
        } else {
            Ok(format!("->({}) {{ {b} }}", params.join(", ")))
        }
    }

    /// A quoted symbol becomes a Ruby Symbol literal (`:"name"`), which prints
    /// bare like an AINL symbol.
    fn quote(&mut self, node: &Node) -> String {
        match node {
            Node::Int(i, _) => i.to_string(),
            Node::Float(x, _) => ruby_float(*x),
            Node::Str(s, _) => ruby_str(s),
            Node::Sym(s, _) => format!(":{}", ruby_str(s)),
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

impl ExprEmit for Rb {
    fn expr(&mut self, node: &Node) -> Result<String> {
        match node {
            Node::Int(i, _) => Ok(i.to_string()),
            Node::Float(x, _) => Ok(ruby_float(*x)),
            Node::Str(s, _) => Ok(ruby_str(s)),
            Node::Sym(name, _) => Ok(match name.as_str() {
                "true" => "true".to_string(),
                "false" => "false".to_string(),
                "nil" => "nil".to_string(),
                other => sanitize(other),
            }),
            Node::List(items, _) => self.expr_list(items, node.span()),
        }
    }
}

fn ruby_params(params_node: &Node) -> Result<Vec<String>> {
    shared::parse_params(params_node, "*", sanitize)
}

/// Turn an AINL symbol into a valid Ruby local-variable name (lowercase start).
fn sanitize(name: &str) -> String {
    let mut s = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_alphanumeric() || c == '_' {
            s.push(c);
        } else {
            s.push('_');
        }
    }
    let first = s.chars().next();
    if s.is_empty() || matches!(first, Some(c) if c.is_ascii_digit() || c.is_uppercase()) {
        s.insert(0, '_');
    }
    if is_ruby_keyword(&s) {
        s.push('_');
    }
    s
}

fn is_ruby_keyword(s: &str) -> bool {
    matches!(
        s,
        "BEGIN"
            | "END"
            | "alias"
            | "and"
            | "begin"
            | "break"
            | "case"
            | "class"
            | "def"
            | "defined?"
            | "do"
            | "else"
            | "elsif"
            | "end"
            | "ensure"
            | "false"
            | "for"
            | "if"
            | "in"
            | "module"
            | "next"
            | "nil"
            | "not"
            | "or"
            | "redo"
            | "rescue"
            | "retry"
            | "return"
            | "self"
            | "super"
            | "then"
            | "true"
            | "undef"
            | "unless"
            | "until"
            | "when"
            | "while"
            | "yield"
            | "lambda"
            | "proc"
    )
}

fn ruby_float(x: f64) -> String {
    if !x.is_finite() {
        return "Float::NAN".to_string();
    }
    if x.fract() == 0.0 {
        format!("{x:.1}")
    } else {
        format!("{x}")
    }
}

fn ruby_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            '\r' => o.push_str("\\r"),
            '#' => o.push_str("\\#"), // avoid accidental #{} interpolation
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

const RUNTIME: &[(&str, &str)] = &[
    (
        "_disp",
        "def _disp(x)\n  return \"true\" if x == true\n  return \"false\" if x == false\n  return \"nil\" if x.nil?\n  return x.to_s if x.is_a?(Symbol)\n  return \"(\" + x.map { |e| _repr(e) }.join(\" \") + \")\" if x.is_a?(Array)\n  x.to_s\nend",
    ),
    (
        "_repr",
        "def _repr(x)\n  x.is_a?(String) ? \"\\\"\" + x + \"\\\"\" : _disp(x)\nend",
    ),
    ("_print", "def _print(*xs)\n  puts xs.map { |x| _disp(x) }.join(\" \")\nend"),
    ("_str", "def _str(*xs)\n  xs.map { |x| _disp(x) }.join(\"\")\nend"),
    ("_len", "def _len(x)\n  x.length\nend"),
    ("_first", "def _first(x)\n  x[0]\nend"),
    ("_rest", "def _rest(x)\n  x.drop(1)\nend"),
    ("_nth", "def _nth(x, i)\n  (0 <= i && i < x.length) ? x[i] : nil\nend"),
    ("_cons", "def _cons(h, t)\n  [h] + t\nend"),
    ("_push", "def _push(t, *xs)\n  t + xs\nend"),
    ("_error", "def _error(*xs)\n  raise(xs.map { |x| _disp(x) }.join(\" \"))\nend"),
];
