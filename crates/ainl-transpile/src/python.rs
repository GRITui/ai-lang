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
    // Lower `map` / `filter` / `reduce` to the `let` + `while` loops they are
    // (see ainl_core::collection_forms) before emitting. Every backend does this,
    // so the loop is written once and cannot drift between them — and this
    // emitter never needs a Python-specific `map`/`filter`/`reduce`, which
    // would otherwise silently bind to the *host's* builtins of those names
    // (Python's `map` returns an iterator, Ruby's `map` is a method). Lowering
    // first is what keeps `(map f xs)` meaning AINL's `map` on every backend.
    let lowered = ainl_core::collection_forms::lower(forms);
    let forms = &lowered[..];
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
        // Tier 2 testing first: `_test` renders the actual value, so it is a
        // member of the display cluster below — naming it there is what pulls
        // in `_disp`, `_repr`, `_Hash` and `_Sym`.
        if self.needed.contains("_test") {
            self.needed.insert("_typename");
        }
        // `_sort` reports AINL type names in its errors, and the comparator form
        // delegates the sign to `_cmp_sign`.
        if self.needed.contains("_sort") {
            self.needed.insert("_typename");
            self.needed.insert("_cmp_sign");
            self.needed.insert("_sort_key");
        }
        // Resolve runtime dependencies: the display cluster references `_Sym`,
        // and `_sym` (from quoted symbols) needs the `_Sym` class.
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
        // Tier 1 file I/O: the three path builtins share one canonicalizer, and
        // every new builtin rejects a quoted symbol (a `_Sym` str subclass) the
        // way the interpreter's `as_path_arg` rejects a non-str — without the
        // `_Sym` reference they would test `_Sym` before it is defined.
        if ["_path_join", "_path_base", "_path_dir"]
            .iter()
            .any(|n| self.needed.contains(*n))
        {
            self.needed.insert("_path_canonical");
        }
        for n in [
            "_file_exists",
            "_delete_file",
            "_list_dir",
            "_path_join",
            "_path_base",
            "_path_dir",
        ] {
            if self.needed.contains(n) {
                self.needed.insert("_Sym");
            }
        }
        // Tier 1 JSON. Both entry points pull in the whole cluster, because
        // `_ainl_tname` (needed for json-parse's type error) branches on
        // `_Sym` and `_Hash`, and the RUNTIME table is emitted in declaration
        // order — so `_Sym`/`_Hash` must be emitted before the first helper
        // that mentions them by name.
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
            self.needed.insert("_Hash");
            self.needed.insert("_Sym");
        }
        // `try`. `_ainl_try` needs `_AinlError` (the except clause) and
        // `_caught` (which builds the hash and needs `_Hash`), and the RUNTIME
        // table is emitted in declaration order, so `_Hash` must be pulled in
        // too — otherwise `_caught` would name a class defined after it.
        if self.needed.contains("_ainl_try") || self.needed.contains("_caught") {
            self.needed.insert("_ainl_try");
            self.needed.insert("_caught");
            self.needed.insert("_AinlError");
            self.needed.insert("_Hash");
        }
        // The checked helpers call `_error` and `_ainl_tname` by name, and
        // `_ainl_tname` itself branches on `_Hash`/`_Sym`, so both must be
        // emitted first. RUNTIME is emitted in declaration order, and the
        // helper cluster is declared ABOVE the type-name helper, so ordering
        // here is what keeps the generated module importable.
        //
        // This pass runs BEFORE the `_error` -> `_AinlError` rule below,
        // because it is what can insert `_error` in the first place: a program
        // that only reaches `_error` indirectly (through a `(len x)` type
        // guard, say) has no `error` form and no `try`, yet still generates a
        // `raise _AinlError(...)` from `_error`. Checking `_error` before this
        // loop would miss it, and the program would fail at RUN time with
        // `NameError: name '_AinlError' is not defined` — on the error path,
        // which is exactly the path `try` exists to exercise.
        for n in [
            "_add", "_sub", "_mul", "_div", "_mod", "_alist", "_ahash", "_len", "_first", "_rest",
            "_nth", "_cons", "_push", "_get", "_assoc", "_has", "_keys", "_vals",
        ] {
            if self.needed.contains(n) {
                self.needed.insert("_error");
                self.needed.insert("_ainl_tname");
            }
        }
        // AINL-level `error` must raise the type `catch` looks for. Asking for
        // `_error` alone would emit the raise without the class it raises.
        if self.needed.contains("_error") {
            self.needed.insert("_AinlError");
        }
        // The list builtins all guard through `_alist`; the hash ones through
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
        // Every file builtin shares `_apath`, and it needs `_ainl_tname` to name
        // the type it was handed.
        for n in ["_read_file", "_write_file", "_append_file"] {
            if self.needed.contains(n) {
                self.needed.insert("_apath");
                self.needed.insert("_error");
                self.needed.insert("_ainl_tname");
            }
        }
        // `_ainl_tname` is declared after the guards that use it, so pull the
        // classes it inspects in as well.
        if self.needed.contains("_ainl_tname") {
            self.needed.insert("_Hash");
            self.needed.insert("_Sym");
            self.needed.insert("_disp");
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
                    "try" => return self.stmt_try(&items[1..], ret),
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
        // A `& rest` parameter must hold an AINL LIST, not Python's tuple.
        // `def sum(*xs)` binds a tuple, and every AINL list builtin refuses
        // one — `(len xs)`, `(first xs)` and `(rest xs)` all raise "expects
        // list, got ?", where `?` is `_ainl_tname` failing to name a type that
        // should not exist. Converting here keeps the rest parameter an
        // ordinary AINL value, so the program behaves the same as it does in
        // the interpreter. The slice keeps a 0-arg call binding `[]` rather
        // than `None`, matching `(fn (& xs) ...)` called with no arguments.
        if let Some(rest) = rest_param(params_node)? {
            self.line(&format!("{rest} = list({rest})"));
        }
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

    /// `(try body... (catch (e) handler...))` in statement position.
    ///
    /// Lowers to a native `try`/`except`, via a `_ainl_try(body, handler)`
    /// helper so the same lowering serves expression position too. Each side
    /// becomes a **closure**, which is what makes the body a real scope: Python
    /// resolves a function's names locally, so a `def` in the body is invisible
    /// to the handler — the same sibling-scope rule the other three backends
    /// enforce. Without it a handler could read a half-initialised value from
    /// the body that failed.
    fn stmt_try(&mut self, args: &[Node], ret: bool) -> Result<()> {
        let f = ainl_core::eval::parse_try(args)?;
        self.need("_ainl_try");
        self.need("_AinlError");
        self.need("_caught");
        // The two thunks share ONE pair of names and are re-bound by every
        // `try` in the program. That is safe because each is defined and then
        // immediately called, with no code between the `def` and the `_ainl_try`
        // that reads them — so the names are always the ones just defined, even
        // inside a loop. A counter would work too and buy nothing here.
        self.line("def _ainl_body():");
        self.indent += 1;
        self.emit_side(f.body)?;
        self.indent -= 1;
        self.line(&format!("def _ainl_hand({e}):", e = sanitize(f.param)));
        self.indent += 1;
        self.emit_side(f.handler)?;
        self.indent -= 1;
        let slot = "_ainl_v";
        self.line(&format!("{slot} = _ainl_try(_ainl_body, _ainl_hand)"));
        if ret {
            self.line(&format!("return {slot}"));
        }
        Ok(())
    }

    /// Emit `forms` as a function body whose value is the last form's, `return`ed.
    fn emit_side(&mut self, forms: &[Node]) -> Result<()> {
        if forms.is_empty() {
            self.line("return None");
            return Ok(());
        }
        let last = forms.len() - 1;
        for (i, f) in forms.iter().enumerate() {
            if i == last {
                let e = self.expr(f)?;
                self.line(&format!("return {e}"));
            } else {
                self.stmt(f, false)?;
            }
        }
        Ok(())
    }

    fn stmt_if(&mut self, args: &[Node], ret: bool) -> Result<()> {
        match args {
            [cond, then] => {
                let c = self.cond(cond)?;
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
                let c = self.cond(cond)?;
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
                "+" => return self.arith("+", args),
                "*" => return self.arith("*", args),
                "-" => return self.arith("-", args),
                "/" => return self.arith("/", args),
                "=" => return self.eq_chain(args),
                "<" => return self.chain(args, "<"),
                ">" => return self.chain(args, ">"),
                "<=" => return self.chain(args, "<="),
                ">=" => return self.chain(args, ">="),
                "and" => return self.chain_logic(args, "and"),
                "or" => return self.chain_logic(args, "or"),
                "not" => return shared::unary(self, args, "not "),
                "mod" => {
                    let [a, b] = args else {
                        return Err(Error::runtime("'mod' expects 2 arguments"));
                    };
                    self.need("_ainl_tname");
                    self.need("_mod");
                    let (x, y) = (self.expr(a)?, self.expr(b)?);
                    return Ok(format!("_mod({x}, {y})"));
                }
                "if" => return self.expr_if(args),
                "let" => return self.expr_let(args, span),
                "do" => return self.expr_do(args, span),
                "try" => return self.expr_try(args, span),
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
                // `map` / `filter` / `reduce` are special forms lowered to
                // loops before emit, so there is no arm for them here — and
                // deliberately so: Python has its own `map`/`filter`, and
                // binding AINL's names to the host's would silently change
                // the meaning (the host's `map` is a lazy iterator, not a
                // list). `sort` is a real builtin in every backend.
                "sort" => return self.call_builtin("_sort", args, Some("_sort")),
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

    /// `+ - * /` route through the checked `_add`/`_sub`/`_mul`/`_div` helpers
    /// rather than Python's operators.
    ///
    /// The reason is error parity, not speed: a host operator raises a host
    /// exception whose text a `catch` would then bind, and those texts differ
    /// per target (`ZeroDivisionError: division by zero` here, `division by
    /// zero` in the C runtime, a silent `NaN` in JavaScript). The helper
    /// raises AINL's own message, so the caught value matches on all five
    /// backends. The 0-arg and 1-arg cases keep Python's own identities so the
    /// non-error path is unchanged.
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

    /// AINL's `and` / `or`, which short-circuit on AINL truthiness.
    ///
    /// NOT the host operator. `(and 0 "x")` is `"x"` in AINL — `0` is truthy, so
    /// the chain continues — but `0` in Python, because `0` is falsey there. The
    /// host operator also returns an OPERAND rather than a boolean, so
    /// `(or 0 "")` would answer `0` here and `""` in the interpreter: a different
    /// value, not just a different truthiness reading. Routing both operands
    /// through `_truthy` and returning the original makes the chain a genuine
    /// short-circuit.
    fn chain_logic(&mut self, args: &[Node], op: &str) -> Result<String> {
        let parts = self.expr_all(args)?;
        match parts.len() {
            // The identities: `(and)` is `true` and `(or)` is `false`, not nil.
            0 => Ok(if op == "and" { "True" } else { "False" }.to_string()),
            1 => Ok(parts.into_iter().next().unwrap()),
            _ => {
                self.need("_truthy");
                // Every operand but the last is coerced to a host boolean; the
                // last is returned as-is, which is what makes `(or 0 "")` answer
                // `""` rather than `False`.
                let mut out = format!("_truthy({})", parts[0]);
                for p in &parts[1..parts.len() - 1] {
                    out.push_str(&format!(" {op} _truthy({p})"));
                }
                let last = &parts[parts.len() - 1];
                if op == "and" {
                    Ok(format!("(({out}) and {last})"))
                } else {
                    Ok(format!("(({out}) or {last})"))
                }
            }
        }
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
            None => "None".to_string(),
        };
        Ok(format!("({t} if {c} else {e})"))
    }

    /// AINL's condition, as a host boolean.
    ///
    /// NOT `self.expr(cond)`. Python's `if` uses host truthiness, and AINL
    /// disagrees with it about two values: `0` and `""` are TRUTHY in AINL, and
    /// falsey in Python. Emitting the condition bare would make `(if 0 "t" "f")`
    /// answer `f` here and `t` in the interpreter, the VM, the C runtime and
    /// Ruby — four backends against one. The `filter` builtin's predicate is the
    /// case that matters: a port that inherits host truthiness drops every `0`
    /// and `""` from the result, silently.
    fn cond(&mut self, node: &Node) -> Result<String> {
        // Any `if` needs `_truthy`, in both statement and expression position.
        self.need("_truthy");
        Ok(format!("_truthy({})", self.expr(node)?))
    }

    /// `try` in expression position — `(def status (try … (catch (e) …)))` is
    /// the most natural form of all, so it is not refused.
    ///
    /// A Python expression cannot contain statements, so this needs an IIFE:
    /// the whole `try` becomes a call to a nullary lambda that in turn calls
    /// `_ainl_try` with the two sides. That keeps the sides as real closures
    /// (so the body's `def`s stay local to it) *and* keeps them closing over
    /// the enclosing function's locals, which a hoisted module-level helper
    /// could not do.
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
        self.need("_AinlError");
        self.need("_caught");
        let body = match f.body.first() {
            Some(n) => self.expr(n)?,
            // An empty body is nil, the same as `(do)`.
            None => "None".to_string(),
        };
        let handler = match f.handler.first() {
            Some(n) => self.expr(n)?,
            None => "None".to_string(),
        };
        Ok(format!(
            "(lambda: _ainl_try(lambda: {body}, lambda {e}: {handler}))()",
            e = sanitize(f.param)
        ))
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

/// The name of the `& rest` parameter, if the parameter list has one.
///
/// Reported separately from `python_params` because the caller needs to emit
/// something for it beyond the parameter itself: a Python `*rest` binds a
/// tuple, and an AINL rest parameter is a list.
fn rest_param(params_node: &Node) -> Result<Option<String>> {
    let Node::List(param_nodes, _) = params_node else {
        return Err(Error::runtime("fn params must be a list"));
    };
    let Some(i) = param_nodes
        .iter()
        .position(|n| matches!(n, Node::Sym(p, _) if p == "&"))
    else {
        return Ok(None);
    };
    match param_nodes.get(i + 1) {
        Some(Node::Sym(rest, _)) => Ok(Some(sanitize(rest))),
        _ => Err(Error::runtime("'&' must be followed by a rest parameter")),
    }
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
        "_truthy",
        "def _truthy(x):\n    # AINL: only `nil` and `false` are falsey. `0` and `\"\"` are TRUTHY, which\n    # is where AINL parts company with Python — `if 0` and `if \"\"` are falsey\n    # here. Every `if` in the emitted program goes through this, so the two\n    # languages cannot disagree about a condition.\n    if x is None: return False\n    if x is False: return False\n    return True",
    ),
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
    (
        "_typename",
        "def _typename(x):\n    if x is None: return 'nil'\n    if x is True or x is False: return 'bool'\n    if isinstance(x, _Sym): return 'sym'\n    if isinstance(x, str): return 'str'\n    if isinstance(x, _Hash): return 'hash'\n    if isinstance(x, list): return 'list'\n    if isinstance(x, bool): return 'bool'\n    if isinstance(x, int): return 'int'\n    if isinstance(x, float): return 'float'\n    return '?'",
    ),
    (
        "_test",
        "def _test(name, actual, expected):\n    if not isinstance(name, str) or isinstance(name, _Sym): raise RuntimeError('test expects a str name, got %s' % _typename(name))\n    if not isinstance(expected, str) or isinstance(expected, _Sym): raise RuntimeError('test expects a str expected value, got %s' % _typename(expected))\n    got = _disp(actual)\n    if got == expected: return True\n    raise RuntimeError('test failed: %s: expected %s, got %s' % (name, expected, got))",
    ),
    // ---- Tier 3 collections: `sort` ----
    //
    // Python's `sorted` is stable, so `key=` would be enough for the *order*,
    // but three things are re-implemented by hand rather than delegated:
    //
    // * The default key. Python compares str by CODE POINT; the interpreter
    //   orders by Rust's `str` Ord (byte order) and the C runtime by `strcmp`.
    //   The three disagree on any non-ASCII string, so the key is
    //   `s.encode('utf-8')` — the same decision `_list_dir` already makes.
    // * Mixed-type rejection. Python's `<` between an int and a str raises a
    //   host `TypeError` whose text is not AINL's; the check below raises the
    //   interpreter's own message, so stderr stays byte-equal across backends.
    // * The comparator's return type. A comparator returning a bool would
    //   silently read as 0/1 and look like a working sort, so a non-number is
    //   named instead.
    //
    // `sort_by_key` is decorated to carry the original index, which is what
    // makes the stability guarantee explicit rather than inherited: on a tie
    // the lower index wins, so equal elements keep their input order.
    (
        "_sort_key",
        "def _sort_key(x):\n    if isinstance(x, (bool,)) or x is None: raise TypeError('sort expects a list of numbers or of strings, got a list mixing %s and ?' % _typename(x))\n    if isinstance(x, (int, float)): return (0, x, b'')\n    if isinstance(x, str) and not isinstance(x, _Sym): return (1, 0, x.encode('utf-8'))\n    raise TypeError('sort expects a list of numbers or of strings, got a list mixing %s and ?' % _typename(x))",
    ),
    (
        "_sort",
        "def _sort(*args):\n    if len(args) == 2:\n        cmpf, xs = args\n        if not callable(cmpf): raise TypeError('sort expects a fn, got %s' % _typename(cmpf))\n    elif len(args) == 1:\n        cmpf, xs = None, args[0]\n    else:\n        raise TypeError('sort expects (sort list) or (sort fn list)')\n    if not isinstance(xs, list): raise TypeError('sort expects a list, got %s' % _typename(xs))\n    if cmpf is None:\n        # A pre-pass, but ONLY for the default form (this branch). The key function alone cannot detect a MIXED list: numbering numbers before strings with a leading 0/1 tag would happily order [1, \"a\"] and return it. The interpreter and the C runtime reject the mixed list, so the rejection has to happen before any ordering. The comparator form is exempt -- a comparator is exactly how a program sorts a list of records, and the interpreter and C runtime only type-check in the default form.\n        if xs:\n            first = _sort_key(xs[0])\n            for v in xs:\n                if (_sort_key(v)[0] == 0) != (first[0] == 0):\n                    raise TypeError('sort expects a list of numbers or of strings, got a list mixing %s and %s' % (_typename(xs[0]), _typename(v)))\n        return [v for _, v in sorted(enumerate(xs), key=lambda p: (_sort_key(p[1]), p[0]))]\n    import functools\n    def _cmp(pa, pb):\n        # `< 0` keeps `pa` first, so on a tie the lower original index wins —\n        # the stability rule, made explicit rather than inherited from\n        # sorted()'s own guarantee.\n        c = _cmp_sign(cmpf, pa[1], pb[1])\n        return c if c != 0 else pa[0] - pb[0]\n    return [v for _, v in sorted(enumerate(xs), key=functools.cmp_to_key(_cmp))]",
    ),
    (
        "_cmp_sign",
        "def _cmp_sign(f, a, b):\n    r = f(a, b)\n    if isinstance(r, bool) or not isinstance(r, (int, float)): raise TypeError('sort comparator must return a number, got %s' % _typename(r))\n    return -1 if r < 0 else (1 if r > 0 else 0)",
    ),
    (
        // Type guards for the list and hash builtins.
        //
        // Same reason as the arithmetic helpers: a `catch` binds the message,
        // and a host `TypeError` ("object of type 'int' has no len()") is not
        // AINL's text and does not match the C runtime or the interpreter. The
        // guards re-establish AINL's own wording — which leads with the
        // *builtin's* name (`first expects list, got int`), so the builtin
        // passes its own name in rather than the guard hard-coding one.
        "_alist",
        "def _alist(who, x):\n    if not isinstance(x, list) or isinstance(x, _Hash): _error(who + ' expects list, got ' + _ainl_tname(x))",
    ),
    (
        // The path-argument check shared by every file builtin. It exists as one
        // helper because AINL's rule is uniform across them — a quoted symbol is
        // a `str` subclass in Python, so `isinstance` alone would accept
        // `(read-file 'x.txt)`, which the interpreter rejects.
        "_apath",
        "def _apath(who, p):\n    if not isinstance(p, str) or isinstance(p, _Sym): _error(who + ' expects a str path, got ' + _ainl_tname(p))",
    ),
    (
        "_ahash",
        "def _ahash(who, x):\n    if not isinstance(x, _Hash): _error(who + ' expects a hash, got ' + _ainl_tname(x))",
    ),
    ("_str", "def _str(*xs):\n    return ''.join(_disp(x) for x in xs)"),
    (
        // `len` accepts a list, a str or a hash in AINL, so it cannot be
        // Python's builtin `len` (which would also accept a dict, a set, and
        // raise on an int with a host message).
        "_len",
        "def _len(x):\n    if isinstance(x, _Hash) or isinstance(x, list) or isinstance(x, str): return len(x)\n    _error('len expects list, str, or hash, got ' + _ainl_tname(x))",
    ),
    (
        // The arity text is AINL's, not Python's. Python would say
        // `TypeError: _first() takes 1 positional argument but 2 were given`,
        // and a `catch` binds that string — so the message has to be the one
        // the interpreter and the C runtime already use.
        "_first",
        "def _first(*a):\n    if len(a) != 1: _error('first expects (first list)')\n    _alist('first', a[0])\n    return a[0][0] if len(a[0]) else None",
    ),
    (
        "_rest",
        "def _rest(*a):\n    if len(a) != 1: _error('rest expects (rest list)')\n    _alist('rest', a[0])\n    return list(a[0][1:])",
    ),
    (
        "_nth",
        "def _nth(*a):\n    if len(a) != 2: _error('nth expects (nth list int)')\n    if not isinstance(a[0], list) or isinstance(a[0], _Hash): _error('nth expects (nth list int)')\n    return a[0][a[1]] if 0 <= a[1] < len(a[0]) else None",
    ),
    (
        "_cons",
        "def _cons(*a):\n    if len(a) != 2: _error('cons expects (cons value list)')\n    if not isinstance(a[1], list) or isinstance(a[1], _Hash): _error('cons expects (cons value list)')\n    return [a[0]] + list(a[1])",
    ),
    (
        "_push",
        "def _push(*a):\n    if len(a) < 2: _error('push expects (push list value...)')\n    if not isinstance(a[0], list) or isinstance(a[0], _Hash): _error('push expects (push list value...)')\n    return list(a[0]) + list(a[1:])",
    ),
    (
        "_hash",
        "def _hash(*kvs):\n    if len(kvs) % 2 != 0: _error('hash expects an even number of key/value arguments, got ' + str(len(kvs)))\n    out = _Hash()\n    for i in range(0, len(kvs), 2):\n        k, v = kvs[i], kvs[i + 1]\n        for pair in out:\n            if _eq(pair[0], k):\n                pair[1] = v\n                break\n        else:\n            out.append([k, v])\n    return out",
    ),
    (
        "_get",
        "def _get(h, k):\n    _ahash('get', h)\n    for pair in h:\n        if _eq(pair[0], k): return pair[1]\n    return None",
    ),
    (
        "_assoc",
        "def _assoc(h, k, v):\n    _ahash('assoc', h)\n    out = _Hash(list(p) for p in h)\n    for pair in out:\n        if _eq(pair[0], k):\n            pair[1] = v\n            return out\n    out.append([k, v])\n    return out",
    ),
    (
        "_has",
        "def _has(h, k):\n    _ahash('has', h)\n    return any(_eq(pair[0], k) for pair in h)",
    ),
    (
        "_keys",
        "def _keys(h):\n    _ahash('keys', h)\n    return [pair[0] for pair in h]",
    ),
    (
        "_vals",
        "def _vals(h):\n    _ahash('vals', h)\n    return [pair[1] for pair in h]",
    ),
    (
        // AINL-level errors get their own exception type, not a bare
        // `RuntimeError`. `catch` binds the message it sees, so the type has to
        // carry AINL's own text rather than a host string; and a dedicated type
        // keeps a `catch` from swallowing a genuine Python bug in the generated
        // code (a `TypeError` from a mistake in *this* transpiler is not an
        // AINL-level error and should propagate rather than be caught).
        "_AinlError",
        "class _AinlError(Exception):\n    pass",
    ),
    (
        "_error",
        "def _error(*xs):\n    raise _AinlError(' '.join(_disp(x) for x in xs))",
    ),
    (
        // Checked arithmetic.
        //
        // These exist because a `catch` binds the MESSAGE it sees, and the
        // host's own message is not AINL's: Python says `ZeroDivisionError:
        // division by zero` and `TypeError: unsupported operand type(s)`, the
        // C runtime and the interpreter say `division by zero` and `expected a
        // number, got str`. Before `catch` that difference was invisible — the
        // error just killed the process. The moment it became catchable it was
        // a 4-backend divergence on the error path, so the arithmetic is
        // re-checked here and the AINL message raised instead.
        //
        // The check is `type(x) in (int, float)` rather than `isinstance`,
        // because Python's bool is a subclass of int and `(true + 1)` must
        // report `got bool` — the same message the other backends give.
        //
        // The fold is a plain loop, NOT `sum(xs)`. AINL names a function
        // `sum` with no difficulty (`(def sum (fn (& xs) ...))` is in
        // examples/hello.ainl), and a module-level `def sum` shadows the
        // builtin for the WHOLE module — including inside `_add`. Calling
        // `sum(xs)` there re-entered the AINL function with a tuple, and the
        // program died with "expected a number, got ?" instead of adding its
        // arguments. A loop has no name to collide with.
        "_add",
        "def _add(*xs):\n    for x in xs:\n        if type(x) not in (int, float): _error('expected a number, got ' + _ainl_tname(x))\n    r = 0\n    for x in xs: r += x\n    return r",
    ),
    (
        "_mul",
        "def _mul(*xs):\n    for x in xs:\n        if type(x) not in (int, float): _error('expected a number, got ' + _ainl_tname(x))\n    r = 1\n    for x in xs: r *= x\n    return r",
    ),
    (
        "_sub",
        "def _sub(a, *rest):\n    if type(a) not in (int, float): _error('expected a number, got ' + _ainl_tname(a))\n    if not rest: return -a\n    for x in rest:\n        if type(x) not in (int, float): _error('expected a number, got ' + _ainl_tname(x))\n    r = a\n    for x in rest: r -= x\n    return r",
    ),
    (
        "_div",
        "def _div(a, *rest):\n    if type(a) not in (int, float): _error('expected a number, got ' + _ainl_tname(a))\n    if not rest: return 1 / a if a != 0 else _error('division by zero')\n    for x in rest:\n        if type(x) not in (int, float): _error('expected a number, got ' + _ainl_tname(x))\n        if x == 0: _error('division by zero')\n    r = a\n    for x in rest: r = r / x if r != 0 else _error('division by zero')\n    return r",
    ),
    (
        "_mod",
        "def _mod(a, b):\n    if type(a) not in (int, float): _error('expected a number, got ' + _ainl_tname(a))\n    if type(b) not in (int, float): _error('expected a number, got ' + _ainl_tname(b))\n    if b == 0: _error('mod by zero')\n    return a % b",
    ),
    (
        // The value a `catch` binds: `{"message" <str>, "kind" "runtime"}`.
        // Built here rather than by calling `_hash` so the key ORDER is fixed by
        // construction — every backend prints a map in insertion order, and
        // `message` before `kind` is what makes the caught value byte-identical
        // across all five.
        "_caught",
        "def _caught(e):\n    return _Hash([['message', str(e)], ['kind', 'runtime']])",
    ),
    (
        // The `try` itself. A dedicated helper (rather than an inline
        // `try:`/`except:`) is what lets statement position and expression
        // position share one lowering: both are just a call with the two sides
        // passed as closures.
        "_ainl_try",
        "def _ainl_try(body, handler):\n    try:\n        return body()\n    except _AinlError as e:\n        return handler(_caught(e))",
    ),
    // ---- Tier 1 JSON ----
    // Python is the one host with a real JSON parser, but `json.loads` cannot
    // be used directly: it returns dicts (unordered-by-contract, and the
    // equality/round-trip rules are AINL's own), and `json.dumps` emits
    // scientific notation for some floats plus its own key order. Both are
    // replaced by hand-written code that follows
    // crates/ainl-core/src/json_value.rs — the normative spec — for the four
    // documented decisions: str keys only, insertion-order objects, one
    // canonical float spelling, non-finite floats are an error.
    //
    // The parser is hand-written rather than json.loads for one more reason:
    // a map is a _Hash (a list of pairs) in this target, and `json.loads` would
    // silently change duplicate-key behavior (Python keeps the last value but
    // not AINL's first-position rule) and produce plain dicts that _disp
    // couldn't tell from a list.
    (
        // The AINL type name for a value, for json-parse's type error and
        // json-serialize's "keys must be str, got t" message. The bool checks
        // come before the int check because Python's bool is a subclass of
        // int — `isinstance(True, int)` is True, and "got int" for a bool
        // would be a wrong error message in the one backend that can express
        // the mistake.
        "_ainl_tname",
        "def _ainl_tname(x):\n    if x is None: return 'nil'\n    if x is True or x is False: return 'bool'\n    if isinstance(x, _Sym): return 'sym'\n    if isinstance(x, str): return 'str'\n    if isinstance(x, _Hash): return 'hash'\n    if isinstance(x, list): return 'list'\n    if isinstance(x, float): return 'float'\n    if isinstance(x, int): return 'int'\n    if callable(x): return 'fn'\n    return '?'",
    ),
    (
        "_json_float",
        // The canonical float spelling: plain fixed-point, never scientific,
        // with a mandatory '.0' on a whole value. The digits are the shortest
        // that round-trip. NOT '%.1f' on a whole value (that is `Value`'s
        // Display rule, which prints the exact binary expansion) and NOT
        // repr() (which emits '1e+300'). The shortest form is the rule because
        // it is the one all four backends can compute — JS `toFixed` is
        // undefined above 1e21 and returns exponential form there.
        "def _json_float(x):\n    if x != x or x in (float('inf'), float('-inf')):\n        raise ValueError('json-serialize: cannot serialize %s (not a finite number)' % ('NaN' if x != x else ('inf' if x > 0 else '-inf')))\n    if x == 0.0: return '0.0'\n    # Shortest round-trip digits, then shifted out of scientific notation.\n    r = repr(x)\n    if 'e' not in r and 'E' not in r: return r\n    mant, exp = r.lower().split('e')\n    exp = int(exp)\n    neg = mant.startswith('-')\n    if neg: mant = mant[1:]\n    if '.' in mant: ip, fp = mant.split('.')\n    else: ip, fp = mant, ''\n    digits = ip + fp\n    point = len(ip) + exp\n    if point <= 0: out = '0.' + '0' * (-point) + digits\n    elif point >= len(digits): out = digits + '0' * (point - len(digits)) + '.0'\n    else: out = digits[:point] + '.' + digits[point:]\n    return ('-' + out) if neg else out",
    ),
    (
        "_json_str",
        // One escaping rule, shared with every other backend: '\"', '\\\\',
        // '\\n', '\\r', '\\t', and \\u00xx for every other C0 control. Notably
        // never '\\b' or '\\f' (read on input, never written), so a Python
        // program and an AOT one emit the same bytes. Non-ASCII stays literal
        // UTF-8; json.dumps' default ensure_ascii=True is deliberately not used.
        "def _json_str(s):\n    out = ['\"']\n    for ch in s:\n        o = ord(ch)\n        if ch == '\"': out.append('\\\\\"')\n        elif ch == '\\\\': out.append('\\\\\\\\')\n        elif ch == '\\n': out.append('\\\\n')\n        elif ch == '\\r': out.append('\\\\r')\n        elif ch == '\\t': out.append('\\\\t')\n        elif o < 0x20: out.append('\\\\u%04x' % o)\n        else: out.append(ch)\n    out.append('\"')\n    return ''.join(out)",
    ),
    (
        "_json_ser",
        "def _json_ser(v, depth=0):\n    if depth > 512: raise ValueError('json-serialize: nesting too deep (max 512 levels)')\n    if v is None: return 'null'\n    if v is True: return 'true'\n    if v is False: return 'false'\n    if isinstance(v, float): return _json_float(v)\n    if isinstance(v, bool): return 'true' if v else 'false'\n    if isinstance(v, int): return str(v)\n    if isinstance(v, str) and not isinstance(v, _Sym): return _json_str(v)\n    if isinstance(v, _Hash):\n        parts = []\n        for k, e in v:\n            if isinstance(k, _Sym) or not isinstance(k, str):\n                raise ValueError('json-serialize: object keys must be str, got %s' % _ainl_tname(k))\n            parts.append(_json_str(str.__str__(k)) + ':' + _json_ser(e, depth + 1))\n        return '{' + ','.join(parts) + '}'\n    # _Hash is a list subclass, so the _Hash branch above MUST come first —\n    # a map checked against `isinstance(v, list)` first would serialize as an\n    # array of [k, v] pairs.\n    if isinstance(v, list): return '[' + ','.join(_json_ser(e, depth + 1) for e in v) + ']'\n    if isinstance(v, _Sym): raise ValueError('json-serialize: cannot serialize a sym')\n    if callable(v): raise ValueError('json-serialize: cannot serialize a fn')\n    raise ValueError('json-serialize: cannot serialize a %s' % _ainl_tname(v))",
    ),
    (
        "_json_parse",
        "def _json_parse(s):\n    p = [0]\n    b = s\n    def err(m): raise ValueError('json-parse: %s at position %d' % (m, p[0]))\n    def ws():\n        while p[0] < len(b) and b[p[0]] in ' \\t\\n\\r': p[0] += 1\n    def value(depth):\n        if depth > 512: raise ValueError('json-parse: nesting too deep (max 512 levels)')\n        ws()\n        if p[0] >= len(b): err('unexpected end of input')\n        c = b[p[0]]\n        if c == '{': return obj(depth)\n        if c == '[': return arr(depth)\n        if c == '\"':\n            p[0] += 1\n            return string()\n        if b.startswith('true', p[0]): p[0] += 4; return True\n        if b.startswith('false', p[0]): p[0] += 5; return False\n        if b.startswith('null', p[0]): p[0] += 4; return None\n        if c == '-' or c.isdigit(): return number()\n        err('unexpected character')\n    def obj(depth):\n        p[0] += 1\n        m = _Hash()\n        ws()\n        if p[0] < len(b) and b[p[0]] == '}': p[0] += 1; return m\n        while True:\n            ws()\n            if p[0] >= len(b) or b[p[0]] != '\"': err('expected a string key')\n            p[0] += 1\n            k = string()\n            ws()\n            if p[0] >= len(b) or b[p[0]] != ':': err(\"expected ':' after a key\")\n            p[0] += 1\n            v = value(depth + 1)\n            # Last value wins, first position — as hash/assoc do.\n            for pair in m:\n                if pair[0] == k: pair[1] = v; break\n            else: m.append([k, v])\n            ws()\n            if p[0] < len(b) and b[p[0]] == ',': p[0] += 1; continue\n            if p[0] < len(b) and b[p[0]] == '}': p[0] += 1; return m\n            err(\"expected ',' or '}'\")\n    def arr(depth):\n        p[0] += 1\n        items = []\n        ws()\n        if p[0] < len(b) and b[p[0]] == ']': p[0] += 1; return items\n        while True:\n            items.append(value(depth + 1))\n            ws()\n            if p[0] < len(b) and b[p[0]] == ',': p[0] += 1; continue\n            if p[0] < len(b) and b[p[0]] == ']': p[0] += 1; return items\n            err(\"expected ',' or ']'\")\n    def hex4():\n        if p[0] + 4 > len(b): err('truncated \\\\u escape')\n        v = int(b[p[0]:p[0] + 4], 16)\n        p[0] += 4\n        return v\n    def string():\n        out = []\n        while True:\n            if p[0] >= len(b): err('unterminated string')\n            c = b[p[0]]\n            if c == '\"': p[0] += 1; return ''.join(out)\n            p[0] += 1\n            if c == '\\\\':\n                if p[0] >= len(b): err('unterminated escape')\n                e = b[p[0]]; p[0] += 1\n                if e == '\"': out.append('\"')\n                elif e == '\\\\': out.append('\\\\')\n                elif e == '/': out.append('/')\n                elif e == 'b': out.append('\\b')\n                elif e == 'f': out.append('\\f')\n                elif e == 'n': out.append('\\n')\n                elif e == 'r': out.append('\\r')\n                elif e == 't': out.append('\\t')\n                elif e == 'u':\n                    hi = hex4()\n                    if 0xD800 <= hi <= 0xDBFF:\n                        if not (p[0] + 1 < len(b) and b[p[0]] == '\\\\' and b[p[0] + 1] == 'u'): err('unpaired surrogate')\n                        p[0] += 2\n                        lo = hex4()\n                        if not (0xDC00 <= lo <= 0xDFFF): err('invalid low surrogate')\n                        out.append(chr(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)))\n                    elif 0xDC00 <= hi <= 0xDFFF: err('unpaired surrogate')\n                    else: out.append(chr(hi))\n                else: err('invalid escape')\n            elif ord(c) < 0x20: err('control character in string')\n            else: out.append(c)\n    def number():\n        start = p[0]\n        if b[p[0]] == '-': p[0] += 1\n        if p[0] >= len(b): err('expected a digit')\n        if b[p[0]] == '0':\n            p[0] += 1\n            if p[0] < len(b) and b[p[0]].isdigit(): err('leading zero in number')\n        elif b[p[0]].isdigit():\n            while p[0] < len(b) and b[p[0]].isdigit(): p[0] += 1\n        else: err('expected a digit')\n        is_float = False\n        if p[0] < len(b) and b[p[0]] == '.':\n            is_float = True; p[0] += 1\n            if not (p[0] < len(b) and b[p[0]].isdigit()): err(\"expected a digit after '.'\")\n            while p[0] < len(b) and b[p[0]].isdigit(): p[0] += 1\n        if p[0] < len(b) and b[p[0]] in 'eE':\n            is_float = True; p[0] += 1\n            if p[0] < len(b) and b[p[0]] in '+-': p[0] += 1\n            if not (p[0] < len(b) and b[p[0]].isdigit()): err('expected a digit in the exponent')\n            while p[0] < len(b) and b[p[0]].isdigit(): p[0] += 1\n        t = b[start:p[0]]\n        if not is_float:\n            try:\n                return int(t)\n            except ValueError:\n                pass\n        return float(t)\n    v = value(0)\n    ws()\n    if p[0] != len(b): err('trailing content after the value')\n    return v",
    ),
    ("_json_parse_b", "def _json_parse_b(s):\n    if isinstance(s, _Sym) or not isinstance(s, str): raise TypeError('json-parse expects a str, got %s' % _ainl_tname(s))\n    return _json_parse(s)"),
    ("_json_serialize_b", "def _json_serialize_b(v):\n    return _json_ser(v)"),
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
    // The three file builtins below raise AINL's own message rather than
    // letting the host's exception escape. That matters because `catch` binds
    // the message it sees, and a host message differs per target: Python would
    // give `[Errno 2] No such file or directory: 'x.txt'`, JavaScript a raw
    // `ENOENT` object, Ruby `Errno::ENOENT`. The C runtime and the interpreter
    // both say `read-file: cannot read 'x.txt'`, so that is what all five
    // must say — a `catch` comparing messages is then portable. The
    // `except` clauses deliberately re-raise only the I/O failure: a
    // TypeError from a non-str path is already a defined AINL error and keeps
    // its own (different) text.
    (
        "_read_file",
        "def _read_file(path):\n    _apath('read-file', path)\n    try:\n        with open(path, 'r') as f:\n            return f.read()\n    except IsADirectoryError:\n        _error(\"read-file: cannot read '%s': it is a directory\" % path)\n    except OSError:\n        _error(\"read-file: cannot read '%s'\" % path)",
    ),
    (
        "_write_file",
        "def _write_file(path, content):\n    _apath('write-file', path)\n    try:\n        with open(path, 'w') as f:\n            f.write(content)\n    except IsADirectoryError:\n        _error(\"write-file: cannot write '%s': it is a directory\" % path)\n    except OSError:\n        _error(\"write-file: cannot write '%s'\" % path)",
    ),
    (
        "_append_file",
        "def _append_file(path, content):\n    _apath('append-file', path)\n    try:\n        with open(path, 'a') as f:\n            f.write(content)\n    except IsADirectoryError:\n        _error(\"append-file: cannot append '%s': it is a directory\" % path)\n    except OSError:\n        _error(\"append-file: cannot append '%s'\" % path)",
    ),
    // ---- Tier 1 file I/O ----
    // The path helpers implement AINL's own rules rather than delegating to
    // os.path, because the four hosts disagree on every edge case that matters
    // — os.path.join("", "b") is "b" but File.join("", "b") is "/b", and
    // os.path.dirname("x") is "" where POSIX says ".". See the measured table
    // in ainl-core/src/eval.rs and docs/SYNTAX.md "Path functions".
    //
    // `os` is imported lazily inside each helper that needs it, as elsewhere,
    // so a program that only uses the pure path functions loads nothing.
    (
        "_file_exists",
        "def _file_exists(path):\n    import os\n    if not isinstance(path, str) or isinstance(path, _Sym): raise TypeError('file-exists expects a str path')\n    # lexists, not exists: lexists is the lstat, and a broken symlink is still a\n    # directory entry. exists() follows the link and would report it as absent,\n    # matching neither the interpreter nor the C runtime.\n    return True if os.path.lexists(path.rstrip('/') or '/') else None",
    ),
    (
        "_delete_file",
        "def _delete_file(path):\n    import os\n    if not isinstance(path, str) or isinstance(path, _Sym): raise TypeError('delete-file expects a str path')\n    if not os.path.lexists(path): raise OSError(\"delete-file: cannot delete '%s'\" % path)\n    if os.path.isdir(path): raise OSError(\"delete-file: cannot delete '%s': it is a directory\" % path)\n    os.remove(path)",
    ),
    (
        "_list_dir",
        "def _list_dir(path):\n    import os\n    if not isinstance(path, str) or isinstance(path, _Sym): raise TypeError('list-dir expects a str path')\n    try:\n        names = os.listdir(path)\n    except OSError:\n        raise OSError(\"list-dir: cannot read '%s'\" % path)\n    # os.listdir already omits '.' and '..'; the filter is belt-and-braces.\n    # Sort by *bytes*, not by str: the interpreter sorts by Rust's str Ord (byte\n    # order) and the C runtime by unsigned-byte order, while Python's default\n    # str sort is by code point. The two orders differ for non-ASCII names.\n    return sorted((n for n in names if n not in ('.', '..')), key=lambda s: s.encode('utf-8'))",
    ),
    (
        "_path_canonical",
        "def _path_canonical(p):\n    absolute = p.startswith('/')\n    segs = [s for s in p.split('/') if s and s != '.']\n    if p.endswith('/.'):\n        segs.append('.')\n    out = '/'.join(segs)\n    return '/' + out if absolute else out",
    ),
    (
        "_path_join",
        "def _path_join(*parts):\n    if not parts: raise ValueError('path-join expects at least 1 argument')\n    for i, p in enumerate(parts):\n        if not isinstance(p, str) or isinstance(p, _Sym): raise TypeError('path-join expects str parts, got %s at position %d' % (type(p).__name__, i + 1))\n    return _path_canonical('/'.join(parts))",
    ),
    (
        "_path_base",
        "def _path_base(path):\n    if not isinstance(path, str) or isinstance(path, _Sym): raise TypeError('path-base expects a str path')\n    c = _path_canonical(path)\n    return c.rsplit('/', 1)[-1] if c else ''",
    ),
    (
        "_path_dir",
        "def _path_dir(path):\n    if not isinstance(path, str) or isinstance(path, _Sym): raise TypeError('path-dir expects a str path')\n    c = _path_canonical(path)\n    if '/' not in c: return '.'\n    if c == '/': return '/'\n    # rsplit on a top-level name leaves an empty head ('/x' -> ['', 'x']),\n    # which is the root, not the empty string.\n    head = c.rsplit('/', 1)[0]\n    return head or '/'",
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
