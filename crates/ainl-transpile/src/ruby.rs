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
    // Lower `map` / `filter` / `reduce` before emitting (see
    // ainl_core::collection_forms), so the loop is shared with every other
    // backend. Without this, Ruby's own `map` — a method on Enumerable, so a
    // bare `map(f, xs)` would be a NoMethodError, and an unhandled `(map f xs)`
    // could bind to it in some other position — would be in scope for collision.
    let lowered = ainl_core::collection_forms::lower(forms);
    let forms = &lowered[..];
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
        // Tier 2 testing first: `_test` renders the actual value, so it is a
        // member of the display cluster below — naming it there is what pulls
        // in `_disp` and `_repr` — and it names a bad operand's type.
        if self.needed.contains("_test") {
            self.needed.insert("_ainl_tname");
        }
        // Tier 3 collections: `_sort` names AINL types in its errors and
        // delegates the comparator's sign to `_cmp_sign`.
        if self.needed.contains("_sort") {
            self.needed.insert("_ainl_tname");
            self.needed.insert("_cmp_sign");
            self.needed.insert("_sort_key");
        }
        // `try`. `_ainl_try` needs `AinlError` (the `rescue` clause type) and
        // `_caught` (which builds the hash and needs `AHash`), and the RUNTIME
        // table is emitted in declaration order, so `AHash` must be pulled in
        // too — otherwise `_caught` would name a class defined after it.
        if self.needed.contains("_ainl_try") || self.needed.contains("_caught") {
            self.needed.insert("_ainl_try");
            self.needed.insert("_caught");
            self.needed.insert("AinlError");
            self.needed.insert("AHash");
        }
        // AINL-level `error` must raise the type `catch` looks for. Asking for
        // `_error` alone would emit the raise without the class it raises.
        if self.needed.contains("_error") {
            self.needed.insert("AinlError");
        }
        // The checked helpers call `_error` and `_ainl_tname` by name, and
        // `_ainl_tname` itself branches on AHash, so both must be emitted
        // first. RUNTIME is emitted in declaration order and the helper cluster
        // is declared ABOVE the type-name helper, so this ordering is what
        // keeps the generated file loadable.
        for n in [
            "_add", "_sub", "_mul", "_div", "_mod", "_alist", "_ahash", "_len", "_first", "_rest",
            "_nth", "_cons", "_push", "_get", "_assoc", "_has", "_keys", "_vals",
        ] {
            if self.needed.contains(n) {
                self.needed.insert("_error");
                self.needed.insert("_ainl_tname");
            }
        }
        // The list builtins all guard through `_alist`, the hash ones through
        // `_ahash`. Pulling the right predicate in is what keeps a program that
        // only calls `first` from emitting the hash guard too (and vice versa).
        for n in ["_first", "_rest", "_nth", "_cons", "_push"] {
            if self.needed.contains(n) {
                self.needed.insert("_alist");
            }
        }
        for n in ["_get", "_assoc", "_has", "_keys", "_vals"] {
            if self.needed.contains(n) {
                self.needed.insert("_ahash");
            }
        }
        // Every file builtin raises through `_error`, and names a bad operand's
        // type through `_ainl_tname`.
        for n in ["_read_file", "_write_file", "_append_file"] {
            if self.needed.contains(n) {
                self.needed.insert("_error");
                self.needed.insert("_ainl_tname");
            }
        }
        let disp_used = self.needed.iter().any(|n| {
            matches!(
                *n,
                "_print" | "_str" | "_error" | "_repr" | "_disp" | "_test"
            )
        });
        if disp_used {
            self.needed.insert("_disp");
            self.needed.insert("_repr");
        }
        // `_disp` branches on `is_a?(AHash)`; `_hash`/`_assoc` construct one.
        // No `_eq` needed here (unlike JS/Python) — Ruby's native `==`
        // already distinguishes Symbol/String/Integer/Bool correctly.
        if disp_used || self.needed.iter().any(|n| matches!(*n, "_hash" | "_assoc")) {
            self.needed.insert("AHash");
        }
        // Stage 3.1 stdlib dependency: `_min`/`_max` share one fold helper.
        if self.needed.contains("_min") || self.needed.contains("_max") {
            self.needed.insert("_minmax");
        }
        // Tier 1 file I/O: the three path builtins share one canonicalizer, and
        // `_ainl_tname` (path-join's positional type error) branches on AHash.
        if ["_path_join", "_path_base", "_path_dir"]
            .iter()
            .any(|n| self.needed.contains(*n))
        {
            self.needed.insert("_path_canonical");
        }
        if self.needed.contains("_path_join") {
            self.needed.insert("_ainl_tname");
            self.needed.insert("AHash");
        }
        // Tier 1 JSON. The whole cluster is pulled in by either entry point,
        // because `_ainl_tname` (json-parse's type error and the "keys must be
        // str, got t" message) and `_json_ser` both branch on AHash, and
        // `_json_ser` also needs _ainl_tname. The RUNTIME table is emitted in
        // declaration order, so _ainl_tname and AHash come first.
        if self
            .needed
            .iter()
            .any(|n| matches!(*n, "_json_parse_b" | "_json_serialize_b"))
        {
            self.needed.insert("_ainl_tname");
            self.needed.insert("_json_parse");
            self.needed.insert("_json_ser");
            self.needed.insert("_json_str");
            self.needed.insert("_json_float");
            self.needed.insert("AHash");
        }
        // Tier 2 testing: `_test` renders the actual value (`_disp`) and names a
        // bad operand's type (`_ainl_tname`, which branches on AHash). The
        // RUNTIME table is emitted in declaration order, so AHash,
        // _disp and _ainl_tname all precede _test.
        if self.needed.contains("_test") {
            self.needed.insert("_ainl_tname");
            self.needed.insert("_disp");
            self.needed.insert("AHash");
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
                    "try" => return self.stmt_try(&items[1..]),
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
        let c = self.cond(cond)?;
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
                let c = self.cond(cond)?;
                self.line(&format!("if {c}"));
                self.indent += 1;
                self.stmt(then)?;
                self.indent -= 1;
                self.line("end");
                Ok(())
            }
            [cond, then, els] => {
                let c = self.cond(cond)?;
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
                "+" => return self.arith("+", args),
                "*" => return self.arith("*", args),
                "-" => return self.arith("-", args),
                "/" => return self.arith("/", args),
                "=" => return shared::cmp(self, args, "=="),
                "<" => return shared::cmp(self, args, "<"),
                ">" => return shared::cmp(self, args, ">"),
                "<=" => return shared::cmp(self, args, "<="),
                ">=" => return shared::cmp(self, args, ">="),
                "and" => return shared::logic(self, args, "&&"),
                "or" => return shared::logic(self, args, "||"),
                "not" => return shared::unary(self, args, "!"),
                "mod" => return self.infix_mod(args),
                "if" => return self.expr_if(args),
                "try" => return self.expr_try(args, span),
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
                "hash" => return self.call_builtin("_hash", args, Some("_hash")),
                "get" => return self.call_builtin("_get", args, Some("_get")),
                "assoc" => return self.call_builtin("_assoc", args, Some("_assoc")),
                "has" => return self.call_builtin("_has", args, Some("_has")),
                "keys" => return self.call_builtin("_keys", args, Some("_keys")),
                "vals" => return self.call_builtin("_vals", args, Some("_vals")),
                "error" => return self.call_builtin("_error", args, Some("_error")),
                // ---- Tier 2 testing ----
                // A failure raises with the same message the interpreter and
                // the AOT C runtime produce, so stderr stays byte-equal across
                // all four backends. `_test` compares the *rendered* value,
                // which is the same thing the assertion spells.
                "test" => return self.call_builtin("_test", args, Some("_test")),
                // ---- Tier 3 collections ----
                // `map` / `filter` / `reduce` are special forms lowered to loops
                // before emit. Deliberately no arm: `map` is an Enumerable
                // method in Ruby, and an emitted bare `map(...)` would be a
                // NoMethodError rather than anything meaningful.
                "sort" => return self.call_builtin("_sort", args, Some("_sort")),
                // ---- Stage 3.1 stdlib ----
                // Each maps to the host's own idiom (File.read, ENV[],
                // Time.now.to_i, sleep, Math.sqrt) so the emitted Ruby reads like
                // Ruby. Where Ruby's own behavior would diverge from the
                // interpreter's, the helper restores the interpreter's rule —
                // notably rejecting an empty split separator, keeping trim/case
                // ASCII-only, and not letting `Comparable` sort a String against
                // a Numeric (which Ruby permits for some pairs and AINL does not).
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
                // ---- Tier 1 JSON ----
                "json-parse" => {
                    return self.call_builtin("_json_parse_b", args, Some("_json_parse_b"))
                }
                "json-serialize" => {
                    return self.call_builtin("_json_serialize_b", args, Some("_json_serialize_b"))
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

    /// `(try body... (catch (e) handler...))` in statement position.
    ///
    /// Lowers to a native `begin`/`rescue`, via an `_ainl_try(body, handler)`
    /// helper so the same lowering serves expression position too. Each side
    /// becomes a **lambda**, which is what makes the body a real scope: Ruby
    /// resolves a block's parameters and locals locally, so a `def` in the body
    /// is invisible to the handler — the same sibling-scope rule the other
    /// four backends enforce. Without it a handler could read a
    /// half-initialised value from the body that failed.
    fn stmt_try(&mut self, args: &[Node]) -> Result<()> {
        let f = ainl_core::eval::parse_try(args)?;
        self.need("_ainl_try");
        self.need("AinlError");
        self.need("_caught");
        // The two lambdas share ONE pair of names and are re-bound by every
        // `try` in the program. That is safe because each is defined and then
        // immediately passed to `_ainl_try`, with no code between the `lambda`
        // and the call that reads them — so the names are always the ones just
        // defined, even inside a loop. A counter would work too and buys
        // nothing here.
        //
        // The sides are emitted as stabby lambdas (`-> () { }`) rather than
        // `lambda { }` or `lambda do end`. A brace block is only syntactic sugar
        // for `do...end`, and `do...end` BINDS TO THE NEAREST KEYWORD — so a
        // `lambda { }` appearing inside this method's own `do` block is a parse
        // error ("tried to create Proc object without a block"), and a
        // `lambda do ... end` would capture the wrong `do`. The stabby form is
        // delimited by its own braces, so it cannot be captured by an enclosing
        // block. Both sides always carry an explicit parenthesised parameter
        // list: a bare `-> body` reads as the unary minus operator followed by
        // its operand, which does not parse. Verified on the host at top level
        // and nested in a `do` block.
        self.line("_ainl_v = _ainl_try(");
        self.indent += 1;
        self.line("->() {");
        self.indent += 1;
        self.emit_side(f.body)?;
        self.indent -= 1;
        self.line("},");
        self.line(&format!("->({}) {{", sanitize(f.param)));
        self.indent += 1;
        self.emit_side(f.handler)?;
        self.indent -= 1;
        self.line("}");
        self.indent -= 1;
        self.line(")");
        Ok(())
    }

    /// Emit `forms` as a lambda body whose value is the last form's — Ruby
    /// returns a block's last expression, so no explicit return is threaded.
    fn emit_side(&mut self, forms: &[Node]) -> Result<()> {
        if forms.is_empty() {
            self.line("nil");
            return Ok(());
        }
        for f in forms {
            self.stmt(f)?;
        }
        Ok(())
    }

    /// `try` in expression position — `(def status (try … (catch (e) …)))` is
    /// the most natural form of all, so it is not refused.
    ///
    /// A Ruby expression cannot contain statements, so this needs an IIFE: the
    /// whole `try` becomes a call to a nullary lambda that in turn calls
    /// `_ainl_try` with the two sides. That keeps the sides as real lambdas
    /// (so the body's `def`s stay local to it) *and* keeps them closing over
    /// the enclosing method's locals, which a hoisted top-level helper could
    /// not do.
    ///
    /// The limit is the same one `let` already has: a side that is more than
    /// one form is a sequence of statements and cannot be an expression. Saying
    /// so beats emitting something that silently drops a form.
    fn expr_try(&mut self, args: &[Node], span: ainl_core::Span) -> Result<String> {
        let f = ainl_core::eval::parse_try(args)?;
        if f.body.len() > 1 {
            return Err(self.no_expr("multi-statement try body", span));
        }
        if f.handler.len() > 1 {
            return Err(self.no_expr("multi-statement try handler", span));
        }
        self.need("_ainl_try");
        self.need("AinlError");
        self.need("_caught");
        let body = match f.body.first() {
            Some(n) => self.expr(n)?,
            // An empty body is nil, the same as `(do)`.
            None => "nil".to_string(),
        };
        let handler = match f.handler.first() {
            Some(n) => self.expr(n)?,
            None => "nil".to_string(),
        };
        // The IIFE wrapper is a stabby lambda too, for the reason `stmt_try`
        // gives. It is spelled with parens and a block, and never as a bare
        // `-> name` with no argument list: without one, `->` reads as the unary
        // minus operator and the next token is taken as its operand.
        Ok(format!(
            "(->() {{\n  _ainl_try(->() {{ {body} }}, ->({e}) {{ {handler} }})\n}}).call",
            e = sanitize(f.param)
        ))
    }

    /// AINL `+ - * /` and `mod` route through the checked runtime helpers rather
    /// than Ruby's own operators, for the reason `_add` documents: a `catch`
    /// binds the message it sees, and Ruby's behaviour is not AINL's
    /// (`1 + "s"` is `"1s"`, `1.0 / 0.0` is `Infinity`).
    fn arith(&mut self, op: &str, args: &[Node]) -> Result<String> {
        let helper = match op {
            "+" => {
                if args.is_empty() {
                    return Ok("0".to_string());
                }
                "_add"
            }
            "*" => {
                if args.is_empty() {
                    return Ok("1".to_string());
                }
                "_mul"
            }
            "-" => {
                if args.is_empty() {
                    return Err(Error::runtime("- expects at least 1 argument"));
                }
                "_sub"
            }
            _ => {
                if args.is_empty() {
                    return Err(Error::runtime("/ expects at least 1 argument"));
                }
                "_div"
            }
        };
        self.need("_ainl_tname");
        self.need(helper);
        let parts = self.expr_all(args)?;
        Ok(format!("{helper}({})", parts.join(", ")))
    }

    /// `mod` needs its own helper: the zero-denominator check must precede the
    /// `%`, and the message differs from `division by zero`.
    fn infix_mod(&mut self, args: &[Node]) -> Result<String> {
        let [a, b] = args else {
            return Err(Error::runtime("'mod' expects 2 arguments"));
        };
        self.need("_ainl_tname");
        self.need("_mod");
        let (x, y) = (self.expr(a)?, self.expr(b)?);
        Ok(format!("_mod({x}, {y})"))
    }

    /// AINL's condition, as a host boolean.
    ///
    /// NOT `self.expr(cond)`. Ruby's `if` uses host truthiness, and although
    /// Ruby happens to agree with AINL about `0` and `""` (both truthy in both),
    /// agreement by luck is not agreement: Ruby's only falsey values are `nil`
    /// and `false`, which is AINL's rule by coincidence, not by contract. Naming
    /// it makes the rule explicit, keeps Ruby in step if the hosts are ever
    /// swapped, and silences Ruby's `string literal in condition` warning.
    fn cond(&mut self, node: &Node) -> Result<String> {
        self.need("_truthy");
        Ok(format!("_truthy({})", self.expr(node)?))
    }

    fn expr_if(&mut self, args: &[Node]) -> Result<String> {
        let (cond, then, els) = match args {
            [c, t] => (c, t, None),
            [c, t, e] => (c, t, Some(e)),
            _ => return Err(Error::runtime("if expects (if cond then [else])")),
        };
        let c = self.cond(cond)?;
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
        "_truthy",
        "def _truthy(x)\n  # AINL: only `nil` and `false` are falsey. `0` and `\"\"` are TRUTHY. Ruby\n  # agrees today, but by coincidence rather than by contract, so every `if`\n  # and every `and`/`or` chain goes through this rather than the host's rules.\n  return false if x.nil?\n  return false if x == false\n  true\nend",
    ),
    // A map is an array of [k, v] pairs; this subclass exists only so
    // `_disp` can tell a hash apart from a plain list at print time (a print
    // call can't otherwise know a variable's AINL-level type). Ruby's
    // `Array#map`/`#select` return a plain Array (not the subclass) and
    // `#dup` preserves it, so — unlike JS — no extra care is needed there.
    ("AHash", "class AHash < Array\nend"),
    (
        "_disp",
        "def _disp(x)\n  return \"true\" if x == true\n  return \"false\" if x == false\n  return \"nil\" if x.nil?\n  return x.to_s if x.is_a?(Symbol)\n  return \"{\" + x.map { |p| _repr(p[0]) + \" \" + _repr(p[1]) }.join(\" \") + \"}\" if x.is_a?(AHash)\n  return \"(\" + x.map { |e| _repr(e) }.join(\" \") + \")\" if x.is_a?(Array)\n  x.to_s\nend",
    ),
    (
        "_repr",
        "def _repr(x)\n  x.is_a?(String) ? \"\\\"\" + x + \"\\\"\" : _disp(x)\nend",
    ),
    ("_print", "def _print(*xs)\n  puts xs.map { |x| _disp(x) }.join(\" \")\nend"),
    (
        "_test",
        "def _test(name, actual, expected)\n  raise RuntimeError, 'test expects a str name, got ' + _ainl_tname(name) unless name.is_a?(String)\n  raise RuntimeError, 'test expects a str expected value, got ' + _ainl_tname(expected) unless expected.is_a?(String)\n  got = _disp(actual)\n  return true if got == expected\n  raise RuntimeError, \"test failed: #{name}: expected #{expected}, got #{got}\"\nend",
    ),
    // ---- Tier 3 collections: `sort` ----
    //
    // Ruby's `Array#sort` IS stable and `sort_by` is NOT, so neither can be
    // used for the comparator form: `sort_by { |v| ... }` would break the
    // stability guarantee the card requires, on this host alone. The comparator
    // form therefore uses `sort` with a two-element key of [sign, original
    // index], and the default form uses `sort_by` over an explicit
    // [class-tag, value, index] triple — the index is what makes the order
    // independent of Ruby's `sort_by` instability.
    (
        "_sort_key",
        "def _sort_key(x)\n  return [0, x, ''] if x.is_a?(Numeric)\n  return [1, 0, x] if x.is_a?(String)\n  raise RuntimeError, 'sort expects a list of numbers or of strings, got a list mixing ' + _ainl_tname(x) + ' and ?'\nend",
    ),
    (
        "_cmp_sign",
        "def _cmp_sign(f, a, b)\n  r = f.call(a, b)\n  raise RuntimeError, 'sort comparator must return a number, got ' + _ainl_tname(r) unless r.is_a?(Numeric)\n  return -1 if r < 0\n  return 1 if r > 0\n  0\nend",
    ),
    (
        "_sort",
        "def _sort(*args)\n  if args.length == 2\n    cmpf = args[0]\n    xs = args[1]\n    raise RuntimeError, 'sort expects a fn, got ' + _ainl_tname(cmpf) unless cmpf.respond_to?(:call)\n  elsif args.length == 1\n    cmpf = nil\n    xs = args[0]\n  else\n    raise RuntimeError, 'sort expects (sort list) or (sort fn list)'\n  end\n  raise RuntimeError, 'sort expects a list, got ' + _ainl_tname(xs) unless xs.is_a?(Array)\n  if cmpf.nil?\n    # A pre-pass, but ONLY for the default form. A key alone cannot see a MIXED\n    # list: tagging numbers before strings with a leading 0/1 would happily\n    # order [1, 'a']. The interpreter and the C runtime reject that, so the check\n    # runs before any ordering. The comparator form is exempt — a comparator is\n    # exactly how a program sorts a list of records, and the interpreter and the\n    # C runtime only type-check in the default form.\n    unless xs.empty?\n      first = _sort_key(xs[0])[0]\n      xs.each do |v|\n        if _sort_key(v)[0] != first\n          raise RuntimeError, 'sort expects a list of numbers or of strings, got a list mixing ' + _ainl_tname(xs[0]) + ' and ' + _ainl_tname(v)\n        end\n      end\n    end\n    # `_sort_key` puts a number's value in slot 1 and a string's bytes in slot 2,\n    # with the other slot neutral ('' / 0). Since the pre-pass proved the list is\n    # all one type, comparing [tag, slot1, slot2, index] orders numbers by value\n    # and strings bytewise. The trailing index is the stability rule: equal\n    # elements keep their input order.\n    return xs.each_with_index.sort_by { |v, i| k = _sort_key(v); [k[0], k[1], k[2], i] }.map { |v, _i| v }\n  end\n  # `sort` (not `sort_by`): on a tie the lower original index wins, which is the\n  # stability rule, stated rather than inherited from the host.\n  xs.each_with_index.sort { |(av, ai), (bv, bi)| c = _cmp_sign(cmpf, av, bv); c.zero? ? (ai <=> bi) : c }.map { |v, _i| v }\nend",
    ),
    ("_str", "def _str(*xs)\n  xs.map { |x| _disp(x) }.join(\"\")\nend"),
    // Type guards shared by the list and hash builtins.
    //
    // These are the reason a `catch` sees the same message on Ruby as on the
    // other four backends. Without them a wrong-typed operand raises a HOST
    // error — `NoMethodError: undefined method 'length' for 5:Integer` — which
    // is neither the interpreter's wording nor an `_AinlError`, so it would
    // escape the rescue clause and kill the program. Each guard leads with the
    // *builtin's* name, so the builtin passes its own name in rather than the
    // guard hard-coding one.
    (
        "_alist",
        "def _alist(who, x)\n  _error(who + ' expects list, got ' + _ainl_tname(x)) unless x.is_a?(Array) && !x.is_a?(AHash)\nend",
    ),
    (
        "_ahash",
        "def _ahash(who, x)\n  _error(who + ' expects a hash, got ' + _ainl_tname(x)) unless x.is_a?(AHash)\nend",
    ),
    (
        // `len` accepts a list, a str or a hash in AINL, so it cannot be
        // Ruby's own `length` — which would also accept a Hash/Range and would
        // raise a host NoMethodError on an Integer.
        "_len",
        "def _len(x)\n  return x.length if x.is_a?(AHash) || x.is_a?(Array) || x.is_a?(String)\n  _error('len expects list, str, or hash, got ' + _ainl_tname(x))\nend",
    ),
    ("_first", "def _first(x)\n  _alist('first', x)\n  x[0]\nend"),
    ("_rest", "def _rest(x)\n  _alist('rest', x)\n  x.drop(1)\nend"),
    (
        "_nth",
        "def _nth(x, i)\n  _alist('nth', x)\n  (0 <= i && i < x.length) ? x[i] : nil\nend",
    ),
    ("_cons", "def _cons(h, t)\n  _alist('cons', t)\n  [h] + t\nend"),
    ("_push", "def _push(t, *xs)\n  _alist('push', t)\n  t + xs\nend"),
    (
        "_hash",
        "def _hash(*kvs)\n  out = AHash.new\n  i = 0\n  while i < kvs.length\n    k, v = kvs[i], kvs[i + 1]\n    pair = out.find { |p| p[0] == k }\n    if pair\n      pair[1] = v\n    else\n      out << [k, v]\n    end\n    i += 2\n  end\n  out\nend",
    ),
    (
        "_get",
        "def _get(h, k)\n  _ahash('get', h)\n  pair = h.find { |p| p[0] == k }\n  pair ? pair[1] : nil\nend",
    ),
    (
        "_assoc",
        "def _assoc(h, k, v)\n  _ahash('assoc', h)\n  out = AHash[*h.map { |p| p.dup }]\n  pair = out.find { |p| p[0] == k }\n  if pair\n    pair[1] = v\n  else\n    out << [k, v]\n  end\n  out\nend",
    ),
    ("_has", "def _has(h, k)\n  _ahash('has', h)\n  h.any? { |p| p[0] == k }\nend"),
    ("_keys", "def _keys(h)\n  _ahash('keys', h)\n  h.map { |p| p[0] }\nend"),
    ("_vals", "def _vals(h)\n  _ahash('vals', h)\n  h.map { |p| p[1] }\nend"),
    // ---- Tier 3 try/catch ----
    // The error type a `catch` looks for. Every AINL-level failure raises
    // THIS, not Ruby's `RuntimeError`, so `_ainl_try` can tell an AINL error
    // (catchable) from a bug in the generated code (not catchable) and
    // re-raise the latter. Declared BEFORE `_error` because the RUNTIME table
    // is emitted in declaration order and `_error` names this class.
    ("AinlError", "class AinlError < StandardError\nend"),
    (
        // Raises `AinlError`, the type `_ainl_try` rescues. A bare `raise`
        // with a String would produce a `RuntimeError`, which the rescue clause
        // would NOT catch — so an AINL error raised through this helper would
        // pass straight through every `catch` in the program.
        "_error",
        "def _error(*xs)\n  raise(AinlError, xs.map { |x| _disp(x) }.join(\" \"))\nend",
    ),
    // Checked arithmetic.
    //
    // These exist because a `catch` binds the MESSAGE it sees, and Ruby's own
    // behaviour is not AINL's. The two traps, both verified on the host:
    //   * `/` does not raise on a zero denominator — `1.0 / 0.0` is
    //     `Infinity` and `1 / 0` is `ZeroDivisionError` only for Integers.
    //     AINL rejects both, so the zero test comes BEFORE the division.
    //   * `+` does not reject a non-numeric operand: `1 + "s"` is `"1s"` and
    //     `true + 1` is a TypeError with a different message. AINL rejects
    //     both with `expected a number, got <t>`.
    // `mod` is the same story, and the zero test must precede `%` too.
    (
        "_add",
        "def _add(a, *rest)\n  _error('expected a number, got ' + _ainl_tname(a)) unless a.is_a?(Numeric)\n  rest.each { |x| _error('expected a number, got ' + _ainl_tname(x)) unless x.is_a?(Numeric) }\n  a + rest.inject(0) { |acc, x| acc + x }\nend",
    ),
    (
        "_sub",
        "def _sub(a, *rest)\n  _error('expected a number, got ' + _ainl_tname(a)) unless a.is_a?(Numeric)\n  rest.each { |x| _error('expected a number, got ' + _ainl_tname(x)) unless x.is_a?(Numeric) }\n  return -a if rest.empty?\n  a - rest.inject(0) { |acc, x| acc + x }\nend",
    ),
    (
        "_mul",
        "def _mul(a, *rest)\n  _error('expected a number, got ' + _ainl_tname(a)) unless a.is_a?(Numeric)\n  rest.each { |x| _error('expected a number, got ' + _ainl_tname(x)) unless x.is_a?(Numeric) }\n  rest.inject(a) { |acc, x| acc * x }\nend",
    ),
    (
        "_div",
        "def _div(a, *rest)\n  _error('expected a number, got ' + _ainl_tname(a)) unless a.is_a?(Numeric)\n  _error('division by zero') if a == 0\n  return (1.0 / a).to_f if rest.empty?\n  rest.each do |x|\n    _error('expected a number, got ' + _ainl_tname(x)) unless x.is_a?(Numeric)\n    _error('division by zero') if x == 0\n  end\n  (a.to_f / rest[0].to_f).to_f\nend",
    ),
    (
        "_mod",
        "def _mod(a, b)\n  _error('expected a number, got ' + _ainl_tname(a)) unless a.is_a?(Numeric)\n  _error('expected a number, got ' + _ainl_tname(b)) unless b.is_a?(Numeric)\n  _error('mod by zero') if b == 0\n  a % b\nend",
    ),
    (
        // The value a `catch` binds: `{"message" <str>, "kind" "runtime"}`.
        // Built here rather than by calling `_hash` so the key ORDER is fixed by
        // construction — every backend prints a map in insertion order, and
        // `message` before `kind` is what makes the caught value byte-identical
        // across all five. `e.to_s` on a `AinlError` is exactly the message
        // `_error` built, because `_error` passes it to `raise` as the whole
        // string.
        "_caught",
        "def _caught(e)\n  AHash.new([['message', e.to_s], ['kind', 'runtime']])\nend",
    ),
    (
        // The `try` itself. A dedicated helper (rather than an inline
        // `begin`/`rescue`) is what lets statement position and expression
        // position share one lowering: both are just a call with the two sides
        // passed as lambdas. The `rescue AinlError` (not a bare `rescue`) is
        // what keeps a Ruby-level bug in the generated code from being
        // silently swallowed as if the AINL program had handled it.
        "_ainl_try",
        "def _ainl_try(body, handler)\n  begin\n    body.call\n  rescue AinlError => e\n    handler.call(_caught(e))\n  end\nend",
    ),
    // ---- Stage 3.1 stdlib ----
    // Each helper pins the *interpreter's* rule where Ruby's own behavior would
    // differ, so all four backends agree:
    //   * split/replace reject an empty target instead of raising ArgumentError
    //     / silently returning the input.
    //   * trim and case are ASCII-only (Ruby's are Unicode-aware, and
    //     String#strip also removes U+00A0 and friends).
    //   * min/max fold pairwise over Numeric only: Ruby's Comparable would let
    //     a String participate in `<` against an Integer, AINL will not.
    //   * floor/sqrt keep AINL's rules — floor returns an Integer and passes an
    //     Integer through untouched, sqrt rejects a negative rather than
    //     raising Math::DomainError with a different message.
    // `require` is lazy inside each helper so a program that only uses `trim`
    // loads nothing.
    // The three file builtins convert a host failure into AINL's own message.
    // Without the conversion a `catch` binds a HOST object: Ruby's would be
    // `Errno::ENOENT`, Python's a `FileNotFoundError`, JS's a raw `ENOENT`
    // `Error` — three different types and three different message bodies for
    // the same missing file, which is exactly the 4-backend divergence this
    // card has to rule out. `SystemCallError` is the common ancestor of the
    // errno-backed ones (`Errno::ENOENT`, `Errno::EISDIR`, `Errno::EACCES`);
    // `SystemStackError`/`NoMemoryError` are not file failures and must not be
    // relabelled as one.
    (
        "_read_file",
        "def _read_file(path)\n  raise TypeError, 'read-file expects a str path' unless path.is_a?(String)\n  begin\n    File.read(path)\n  rescue Errno::EISDIR\n    _error(\"read-file: cannot read '#{path}': it is a directory\")\n  rescue SystemCallError\n    _error(\"read-file: cannot read '#{path}'\")\n  end\nend",
    ),
    (
        "_write_file",
        "def _write_file(path, content)\n  raise TypeError, 'write-file expects a str path' unless path.is_a?(String)\n  raise TypeError, 'write-file expects str content' unless content.is_a?(String)\n  begin\n    File.write(path, content)\n  rescue Errno::EISDIR\n    _error(\"write-file: cannot write '#{path}': it is a directory\")\n  rescue SystemCallError\n    _error(\"write-file: cannot write '#{path}'\")\n  end\nend",
    ),
    (
        "_append_file",
        "def _append_file(path, content)\n  raise TypeError, 'append-file expects a str path' unless path.is_a?(String)\n  raise TypeError, 'append-file expects str content' unless content.is_a?(String)\n  begin\n    File.open(path, 'a') { |f| f.write(content) }\n  rescue Errno::EISDIR\n    _error(\"append-file: cannot append '#{path}': it is a directory\")\n  rescue SystemCallError\n    _error(\"append-file: cannot append '#{path}'\")\n  end\nend",
    ),
    // ---- Tier 1 file I/O ----
    // The path helpers implement AINL's own rules rather than delegating to
    // File/File.dirname, because the hosts disagree on every edge case that
    // matters — File.join("", "b") is "/b" where os.path.join gives "b" and
    // File.join("a//b", "d") preserves a duplicate separator Node collapses.
    // See the measured table in ainl-core/src/eval.rs and docs/SYNTAX.md
    // "Path functions".
    (
        "_file_exists",
        "def _file_exists(path)\n  raise TypeError, 'file-exists expects a str path' unless path.is_a?(String)\n  # File.lstat, not File.exist?: exist? follows a symlink, so a broken one\n  # reads as absent where the interpreter and the C runtime (both lstat) see it.\n  p = path.length > 1 && path.end_with?('/') ? path[0..-1] : path\n  begin\n    File.lstat(p)\n    true\n  rescue SystemCallError\n    nil\n  end\nend",
    ),
    (
        "_delete_file",
        "def _delete_file(path)\n  raise TypeError, 'delete-file expects a str path' unless path.is_a?(String)\n  begin\n    st = File.lstat(path)\n  rescue SystemCallError\n    raise \"delete-file: cannot delete '#{path}'\"\n  end\n  raise \"delete-file: cannot delete '#{path}': it is a directory\" if st.directory?\n  File.unlink(path)\n  nil\nend",
    ),
    (
        "_list_dir",
        "def _list_dir(path)\n  raise TypeError, 'list-dir expects a str path' unless path.is_a?(String)\n  begin\n    names = Dir.children(path)\n  rescue SystemCallError\n    raise \"list-dir: cannot read '#{path}'\"\n  end\n  # Dir.children already omits '.' and '..'. Sort by UTF-8 *bytes*: the\n  # interpreter sorts by Rust's str Ord and the C runtime by unsigned-byte\n  # order, while Array#sort compares String#<=> — which is defined for\n  # character content, and need not be byte order for non-ASCII names.\n  names.reject { |n| n == '.' || n == '..' }.sort_by { |n| n.b }\nend",
    ),
    (
        "_path_canonical",
        "def _path_canonical(p)\n  absolute = p.start_with?('/')\n  segs = p.split('/').reject { |s| s.empty? || s == '.' }\n  segs.push('.') if p.end_with?('/.')\n  out = segs.join('/')\n  absolute ? '/' + out : out\nend",
    ),
    (
        "_path_join",
        "def _path_join(*parts)\n  raise ArgumentError, 'path-join expects at least 1 argument' if parts.empty?\n  parts.each_with_index do |p, i|\n    raise TypeError, \"path-join expects str parts, got #{_ainl_tname(p)} at position #{i + 1}\" unless p.is_a?(String)\n  end\n  _path_canonical(parts.join('/'))\nend",
    ),
    (
        "_path_base",
        "def _path_base(path)\n  raise TypeError, 'path-base expects a str path' unless path.is_a?(String)\n  c = _path_canonical(path)\n  return '' if c.empty?\n  # rindex/slice, not split('/').last: Ruby's String#split drops trailing empty\n  # fields, so '/'.split('/') is [] and .last is nil, where the interpreter's\n  # rsplit gives \"\" and every other backend agrees.\n  i = c.rindex('/')\n  i.nil? ? c : c[(i + 1)..-1]\nend",
    ),
    (
        "_path_dir",
        "def _path_dir(path)\n  raise TypeError, 'path-dir expects a str path' unless path.is_a?(String)\n  c = _path_canonical(path)\n  i = c.rindex('/')\n  return '.' if i.nil?\n  return '/' if i.zero?\n  c[0...i]\nend",
    ),
    // The AINL type name for a value, for `path-join`'s positional type error.
    // Explicit because the interpreter's wording ("got int at position 2") is
    // part of the 4-backend contract, and Ruby's own class names differ. A
    // quoted symbol is a real Ruby Symbol here (not a String subclass, as in
    // the Python/JS targets), and a map is an AHash.
    (
        "_ainl_tname",
        "def _ainl_tname(x)\n  case x\n  when nil then 'nil'\n  when true, false then 'bool'\n  when Integer then 'int'\n  when Float then 'float'\n  when Symbol then 'sym'\n  when String then 'str'\n  when AHash then 'hash'\n  when Array then 'list'\n  when Proc then 'fn'\n  else '?'\n  end\nend",
    ),
    // ---- Tier 1 JSON ----
    // Ruby's JSON library is deliberately not used, for the same reasons as
    // the other two targets: JSON.generate emits its own float spelling
    // ("1.0e+300", and no ".0" on a whole number) and JSON.parse returns a
    // Hash that would not be an AHash, would not implement AINL's
    // first-position duplicate-key rule, and would accept NaN/Infinity.
    // crates/ainl-core/src/json_value.rs is normative.
    (
        "_json_float",
        // Shortest round-tripping digits in plain fixed-point notation, with a
        // mandatory '.0' on a whole value. Ruby's Float#to_s is NOT usable:
        // it emits '1.0e+300'. sprintf('%.17g') gives all 17 digits, so the
        // shortening loop below is what finds the shortest form that still
        // reads back as the same Float.
        "def _json_float(x)\n  if x.nan? || x.infinite?\n    n = x.nan? ? 'NaN' : (x > 0 ? 'inf' : '-inf')\n    raise ArgumentError, \"json-serialize: cannot serialize #{n} (not a finite number)\"\n  end\n  return '0.0' if x == 0.0\n  neg = x < 0\n  ax = neg ? -x : x\n  # Shortest round-tripping significant digits. The format string is built\n  # with the % sign first and the precision interpolated after it, because\n  # Ruby's Kernel#format rejects a precision that precedes the flag.\n  s = nil\n  (0..16).each do |p|\n    s = format('%.' + p.to_s + 'e', ax)\n    break if Float(s) == ax\n  end\n  mant, exp = s.split('e')\n  exp = exp.to_i\n  digits = mant.delete('.')\n  point = mant.index('.') || mant.length\n  point += exp\n  out = if point <= 0\n    '0.' + ('0' * -point) + digits\n  elsif point >= digits.length\n    digits + ('0' * (point - digits.length)) + '.0'\n  else\n    digits[0...point] + '.' + digits[point..-1]\n  end\n  neg ? '-' + out : out\nend",
    ),
    (
        "_json_str",
        // One escaping rule, identical in all four backends: '\"', '\\\\', '\\n',
        // '\\r', '\\t', and \\u00xx for every other C0 control. Never '\\b'/'\\f'.
        // Non-ASCII is emitted literally (JSON.generate would \\u-escape it,
        // and every non-ASCII program would then disagree with the others).
        "def _json_str(s)\n  out = '\"'\n  s.each_char do |ch|\n    o = ch.ord\n    if ch == '\"' then out << '\\\\\"'\n    elsif ch == '\\\\' then out << '\\\\\\\\'\n    elsif ch == \"\\n\" then out << '\\\\n'\n    elsif ch == \"\\r\" then out << '\\\\r'\n    elsif ch == \"\\t\" then out << '\\\\t'\n    elsif o < 0x20 then out << format('\\\\u%04x', o)\n    else out << ch\n    end\n  end\n  out + '\"'\nend",
    ),
    (
        "_json_ser",
        "def _json_ser(v, depth = 0)\n  raise ArgumentError, 'json-serialize: nesting too deep (max 512 levels)' if depth > 512\n  case v\n  when nil then 'null'\n  when true then 'true'\n  when false then 'false'\n  when Float then _json_float(v)\n  when Integer then v.to_s\n  when String then _json_str(v)\n  when AHash\n    parts = []\n    v.each do |p|\n      k = p[0]\n      raise ArgumentError, \"json-serialize: object keys must be str, got #{_ainl_tname(k)}\" unless k.is_a?(String)\n      parts << _json_str(k) + ':' + _json_ser(p[1], depth + 1)\n    end\n    '{' + parts.join(',') + '}'\n  when Array then '[' + v.map { |e| _json_ser(e, depth + 1) }.join(',') + ']'\n  when Symbol then raise ArgumentError, 'json-serialize: cannot serialize a sym'\n  when Proc then raise ArgumentError, 'json-serialize: cannot serialize a fn'\n  else raise ArgumentError, \"json-serialize: cannot serialize a #{_ainl_tname(v)}\"\n  end\nend",
    ),
    (
        "_json_parse",
        "def _json_parse(s)\n  i = 0\n  n = s.bytesize\n  b = s\n  err = lambda do |m|\n    raise ArgumentError, \"json-parse: #{m} at position #{i}\"\n  end\n  ws = lambda do\n    while i < n && [' ', \"\\t\", \"\\n\", \"\\r\"].include?(b[i]) do i += 1 end\n  end\n  number = nil\n  string = nil\n  value = nil\n  arr = nil\n  obj = nil\n  number = lambda do\n    start = i\n    i += 1 if b[i] == '-'\n    err.call('expected a digit') if i >= n\n    if b[i] == '0'\n      i += 1\n      err.call('leading zero in number') if i < n && b[i] >= '0' && b[i] <= '9'\n    elsif b[i] >= '1' && b[i] <= '9'\n      i += 1 while i < n && b[i] >= '0' && b[i] <= '9'\n    else\n      err.call('expected a digit')\n    end\n    is_float = false\n    if i < n && b[i] == '.'\n      is_float = true\n      i += 1\n      err.call(\"expected a digit after '.'\") unless i < n && b[i] >= '0' && b[i] <= '9'\n      i += 1 while i < n && b[i] >= '0' && b[i] <= '9'\n    end\n    if i < n && (b[i] == 'e' || b[i] == 'E')\n      is_float = true\n      i += 1\n      i += 1 if i < n && (b[i] == '+' || b[i] == '-')\n      err.call('expected a digit in the exponent') unless i < n && b[i] >= '0' && b[i] <= '9'\n      i += 1 while i < n && b[i] >= '0' && b[i] <= '9'\n    end\n    t = b[start...i]\n    is_float ? Float(t) : Integer(t, 10)\n  end\n  hex4 = lambda do\n    err.call('truncated \\\\u escape') if i + 4 > n\n    v = b[i, 4].to_i(16)\n    err.call('invalid \\\\u escape') if v.to_s(16).length < 4 && b[i, 4] !~ /\\A[0-9a-fA-F]{4}\\z/\n    i += 4\n    v\n  end\n  string = lambda do\n    out = ''\n    loop do\n      err.call('unterminated string') if i >= n\n      c = b[i]\n      if c == '\"'\n        i += 1\n        return out\n      end\n      i += 1\n      if c == '\\\\'\n        err.call('unterminated escape') if i >= n\n        e = b[i]\n        i += 1\n        if e == '\"' then out << '\"'\n        elsif e == '\\\\' then out << '\\\\'\n        elsif e == '/' then out << '/'\n        elsif e == 'b' then out << \"\\b\"\n        elsif e == 'f' then out << \"\\f\"\n        elsif e == 'n' then out << \"\\n\"\n        elsif e == 'r' then out << \"\\r\"\n        elsif e == 't' then out << \"\\t\"\n        elsif e == 'u'\n          hi = hex4.call\n          if hi >= 0xd800 && hi <= 0xdbff\n            unless i + 1 < n && b[i] == '\\\\' && b[i + 1] == 'u'\n              err.call('unpaired surrogate')\n            end\n            i += 2\n            lo = hex4.call\n            err.call('invalid low surrogate') if lo < 0xdc00 || lo > 0xdfff\n            out << [0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)].pack('U')\n          elsif hi >= 0xdc00 && hi <= 0xdfff\n            err.call('unpaired surrogate')\n          else\n            out << [hi].pack('U')\n          end\n        else\n          err.call('invalid escape')\n        end\n      elsif c.ord < 0x20\n        err.call('control character in string')\n      else\n        out << c\n      end\n    end\n  end\n  arr = lambda do |depth|\n    i += 1\n    items = []\n    ws.call\n    if i < n && b[i] == ']'\n      i += 1\n      return items\n    end\n    loop do\n      items << value.call(depth + 1)\n      ws.call\n      if i < n && b[i] == ','\n        i += 1\n        next\n      end\n      if i < n && b[i] == ']'\n        i += 1\n        return items\n      end\n      err.call(\"expected ',' or ']'\")\n    end\n  end\n  obj = lambda do |depth|\n    i += 1\n    m = AHash.new\n    ws.call\n    if i < n && b[i] == '}'\n      i += 1\n      return m\n    end\n    loop do\n      ws.call\n      err.call('expected a string key') unless i < n && b[i] == '\"'\n      i += 1\n      k = string.call\n      ws.call\n      err.call(\"expected ':' after a key\") unless i < n && b[i] == ':'\n      i += 1\n      v = value.call(depth + 1)\n      # Last value wins, first position — as hash/assoc do. AHash is an Array\n      # of [k, v] pairs, so `m[k] = v` is wrong (that is Array#[]= on an\n      # integer index); the pair is located and updated like _hash does.\n      pair = m.find { |p| p[0] == k }\n      if pair\n        pair[1] = v\n      else\n        m << [k, v]\n      end\n      ws.call\n      if i < n && b[i] == ','\n        i += 1\n        next\n      end\n      if i < n && b[i] == '}'\n        i += 1\n        return m\n      end\n      err.call(\"expected ',' or '}'\")\n    end\n  end\n  value = lambda do |depth|\n    raise ArgumentError, 'json-parse: nesting too deep (max 512 levels)' if depth > 512\n    ws.call\n    err.call('unexpected end of input') if i >= n\n    c = b[i]\n    return obj.call(depth) if c == '{'\n    return arr.call(depth) if c == '['\n    if c == '\"'\n      i += 1\n      return string.call\n    end\n    return true if b[i, 4] == 'true' && (i += 4)\n    return false if b[i, 5] == 'false' && (i += 5)\n    return nil if b[i, 4] == 'null' && (i += 4)\n    return number.call if c == '-' || (c >= '0' && c <= '9')\n    err.call('unexpected character')\n  end\n  v = value.call(0)\n  ws.call\n  err.call('trailing content after the value') if i != n\n  v\nend",
    ),
    (
        "_json_parse_b",
        "def _json_parse_b(s)\n  raise TypeError, \"json-parse expects a str, got #{_ainl_tname(s)}\" unless s.is_a?(String)\n  _json_parse(s)\nend",
    ),
    ("_json_serialize_b", "def _json_serialize_b(v)\n  _json_ser(v, 0)\nend"),
    (
        "_split",
        "def _split(s, sep)\n  raise TypeError, 'split expects a str' unless s.is_a?(String) && sep.is_a?(String)\n  raise ArgumentError, 'split expects a non-empty separator' if sep.empty?\n  # The -1 limit keeps trailing empty fields, which AINL's split does\n  # (\"a,b,\" -> [\"a\" \"b\" \"\"]); Ruby's default limit drops them, which would\n  # disagree with the interpreter and with the Python/JS targets.\n  s.split(sep, -1)\nend",
    ),
    (
        "_join",
        "def _join(xs, sep)\n  raise TypeError, 'join expects a list' unless xs.is_a?(Array)\n  raise TypeError, 'join expects a str separator' unless sep.is_a?(String)\n  xs.each { |x| raise TypeError, 'join expects a list of str' unless x.is_a?(String) }\n  xs.join(sep)\nend",
    ),
    (
        "_trim",
        "def _trim(s)\n  raise TypeError, 'trim expects a str' unless s.is_a?(String)\n  s.gsub(/\\A[ \\t\\n\\r\\x0b\\x0c]+|[ \\t\\n\\r\\x0b\\x0c]+\\z/, '')\nend",
    ),
    (
        "_replace",
        "def _replace(s, old, neu)\n  raise TypeError, 'replace expects a str' unless s.is_a?(String) && old.is_a?(String) && neu.is_a?(String)\n  raise ArgumentError, 'replace expects a non-empty target' if old.empty?\n  s.gsub(old, neu)\nend",
    ),
    (
        "_upcase",
        "def _upcase(s)\n  raise TypeError, 'upcase expects a str' unless s.is_a?(String)\n  s.gsub(/[a-z]/) { |c| (c.ord - 32).chr }\nend",
    ),
    (
        "_downcase",
        "def _downcase(s)\n  raise TypeError, 'downcase expects a str' unless s.is_a?(String)\n  s.gsub(/[A-Z]/) { |c| (c.ord + 32).chr }\nend",
    ),
    (
        "_contains",
        "def _contains(hay, needle)\n  raise TypeError, 'contains expects a str' unless hay.is_a?(String) && needle.is_a?(String)\n  hay.include?(needle)\nend",
    ),
    (
        "_env_get",
        "def _env_get(name)\n  raise TypeError, 'env-get expects a str' unless name.is_a?(String)\n  ENV[name]\nend",
    ),
    (
        "_exit",
        "def _exit(code)\n  raise TypeError, 'exit expects an int' unless code.is_a?(Integer)\n  $stdout.flush\n  exit(code)\nend",
    ),
    (
        "_now",
        "def _now\n  Time.now.to_i\nend",
    ),
    (
        "_sleep",
        "def _sleep(secs)\n  raise TypeError, 'sleep expects a number' unless secs.is_a?(Numeric)\n  raise ArgumentError, 'sleep expects a non-negative number' if secs.respond_to?(:nan?) && secs.nan? || secs < 0\n  sleep(secs) if secs > 0\n  nil\nend",
    ),
    (
        "_abs",
        "def _abs(n)\n  raise TypeError, 'abs expects a number' unless n.is_a?(Numeric)\n  n < 0 ? -n : n\nend",
    ),
    (
        "_minmax",
        "def _minmax(xs, want_max)\n  who = want_max ? 'max' : 'min'\n  raise TypeError, \"#{who} expects at least 1 argument\" if xs.empty?\n  best = xs[0]\n  xs.each do |x|\n    raise TypeError, \"#{who} expects a number\" unless x.is_a?(Numeric)\n    best = x if (want_max ? x > best : x < best)\n  end\n  best\nend",
    ),
    ("_min", "def _min(*xs)\n  _minmax(xs, false)\nend"),
    ("_max", "def _max(*xs)\n  _minmax(xs, true)\nend"),
    (
        "_floor",
        "def _floor(n)\n  raise TypeError, 'floor expects a number' unless n.is_a?(Numeric)\n  n.is_a?(Integer) ? n : n.floor\nend",
    ),
    (
        "_sqrt",
        "def _sqrt(n)\n  raise TypeError, 'sqrt expects a number' unless n.is_a?(Numeric)\n  raise ArgumentError, 'sqrt expects a non-negative number' if n < 0\n  Math.sqrt(n)\nend",
    ),
];
