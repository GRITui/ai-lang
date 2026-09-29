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
    let lowered = ainl_core::collection_forms::lower(forms)?;
    let forms = &lowered[..];
    let idx = LineIndex::new(src);
    let mut rb = Rb {
        body: String::new(),
        indent: 0,
        needed: BTreeSet::new(),
        temps: 0,
        used: shared::used_symbols(forms, sanitize),
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
    /// Counter behind `logic_temp`, so two `and`/`or` chains in one expression
    /// cannot name their operand the same thing.
    temps: usize,
    /// Every name the program already uses, so a generated temp cannot shadow
    /// one. See `shared::used_symbols`.
    used: BTreeSet<String>,
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

    /// One pass of the dependency rules. `finish` calls this repeatedly, so
    /// the order of the rules below is a readability matter and not a
    /// correctness one — see the fixed-point loop in `finish`.
    fn resolve_deps(&mut self) {
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
        // Tier 3 byte-oriented string primitives. `_ainl_b` rejects a non-String
        // under the *calling* builtin's name and reports it through
        // `_ainl_tname`, so every helper here pulls in both.
        if [
            "_substring",
            "_char",
            "_code",
            "_starts_with",
            "_ends_with",
            "_index_of",
        ]
        .iter()
        .any(|n| self.needed.contains(*n))
        {
            self.needed.insert("_ainl_b");
            self.needed.insert("_ainl_idx");
            self.needed.insert("_ainl_off");
            self.needed.insert("_ainl_tname");
            // `_index_of` delegates the actual search to the byte-sequence
            // scanner, which `Array#index` cannot express for a multi-byte
            // needle. Both live in the same RUNTIME table, emitted in
            // declaration order, so it must be requested by name.
            self.needed.insert("_ainl_seq_find");
            // Every helper reports failure through `_error`, so a `catch` can
            // intercept it and the message is AINL's own rather than a host
            // exception carrying a backtrace.
            self.needed.insert("_error");
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
        //
        // `_hash` belongs here for the same reason as the rest: it reports an
        // odd key/value count through `_error`, so naming it without `_error`
        // emitted a helper that died with `NameError: name '_error' is not
        // defined` — a host error where AINL's own message belongs.
        for n in [
            "_add", "_sub", "_mul", "_div", "_mod", "_alist", "_ahash", "_len", "_first", "_rest",
            "_nth", "_cons", "_push", "_hash", "_get", "_assoc", "_has", "_keys", "_vals",
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
        // The six stdlib math builtins report a non-numeric operand through
        // `_anumber`, which names the type with `_ainl_tname` and raises through
        // `_error`. `_error` pulls `AinlError` further down.
        for dep in ["_sleep", "_abs", "_floor", "_sqrt", "_minmax", "_anumber"] {
            if self.needed.contains(dep) {
                self.needed.insert("_anumber");
            }
        }
        if self.needed.contains("_anumber") {
            self.needed.insert("_error");
            self.needed.insert("_ainl_tname");
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
        // Tier 3 file system.
        //
        // `_error` is the shared-message mechanism: every failure raises
        // AINL's own `_AinlError` so an AINL `catch` intercepts it and stderr
        // stays byte-comparable. A bare `raise Errno::EEXIST` is neither — it
        // escapes `catch` and prints a Ruby backtrace.
        //
        // `_ainl_tname` renders AINL's type name for a wrong-typed path, so
        // `got int` matches the interpreter rather than `got Integer`; it
        // branches on AHash, so that comes with it.
        if ["_mkdir", "_rename", "_copy", "_is_dir", "_file_size"]
            .iter()
            .any(|n| self.needed.contains(*n))
        {
            self.needed.insert("_error");
            self.needed.insert("_fs_probe");
            // `_mkdir`'s recursive branch calls the hand-rolled `_fs_mkdir_p`
            // by name, so it must be emitted with it.
            self.needed.insert("_fs_mkdir_p");
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
    }

    /// Resolve the runtime dependency set, then emit.
    ///
    /// The rules above are a graph, not a list, and it is a DAG with real depth
    /// (`_sort` -> `_error` -> `AinlError` -> `_disp` -> `AHash` is four hops).
    /// Evaluating them in a fixed order is therefore wrong by construction: a
    /// rule that runs before the rule which pulls in its own input sees a
    /// `needed` set that is not yet complete and inserts nothing.
    ///
    /// That is not hypothetical. `_error` raises `AinlError`, and the rule
    /// that pulls the class in checked `needed` for `_error` *before* the
    /// loops that insert `_error` for the arithmetic, file and fs builtins — so
    /// `(print (+ 1.0 "s"))`, which has no `try` and no `error` form, emitted
    /// `def _error` naming a class it never defined, and died with
    /// `uninitialized constant AinlError (NameError)` instead of AINL's
    /// message. A fixed point removes the ordering requirement entirely: a new
    /// rule cannot reintroduce this class of bug by being written above a rule
    /// it depends on.
    ///
    /// The loop terminates because `resolve_deps` only ever inserts into a
    /// finite set (`RUNTIME`), so a pass that adds nothing is the last one.
    /// The bound is a belt-and-braces backstop, not the real termination
    /// argument: it is the number of RUNTIME entries.
    fn finish(mut self) -> String {
        let bound = RUNTIME.len() + 1;
        for _ in 0..bound {
            let before = self.needed.len();
            self.resolve_deps();
            if self.needed.len() == before {
                break;
            }
        }
        debug_assert_eq!(
            self.needed.len(),
            {
                self.resolve_deps();
                self.needed.len()
            },
            "dependency resolution did not reach a fixed point"
        );
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
                // ---- Tier 3 file system ----
                "mkdir" => return self.call_builtin("_mkdir", args, Some("_mkdir")),
                "rename" => return self.call_builtin("_rename", args, Some("_rename")),
                "copy" => return self.call_builtin("_copy", args, Some("_copy")),
                "is-dir" => return self.call_builtin("_is_dir", args, Some("_is_dir")),
                "file-size" => return self.call_builtin("_file_size", args, Some("_file_size")),
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
                // ---- Tier 3: byte-oriented string primitives ----
                "substring" => return self.call_builtin("_substring", args, Some("_substring")),
                "char" => return self.call_builtin("_char", args, Some("_char")),
                // `(code s)` and `(code s i)` differ only in arity, and the
                // helper's second parameter defaults to nil — so one arm covers
                // both.
                "code" => return self.call_builtin("_code", args, Some("_code")),
                "starts-with" => {
                    return self.call_builtin("_starts_with", args, Some("_starts_with"))
                }
                "ends-with" => return self.call_builtin("_ends_with", args, Some("_ends_with")),
                "index-of" => return self.call_builtin("_index_of", args, Some("_index_of")),
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
    fn need_truthy(&mut self) {
        self.need("_truthy");
    }

    /// A Ruby lambda, called with `.call` — the same shape `expr_let` already
    /// emits, and what makes the `and`/`or` chain a nest of thunks.
    fn bind_once(&mut self, n: &str, val: &str, body: &str) -> String {
        format!("lambda {{ |{n}| {body} }}.call({val})")
    }

    /// Ruby spells the conditional condition-first, as JS does.
    fn cond(&mut self, cond: &str, then: &str, els: &str) -> String {
        format!("({cond} ? {then} : {els})")
    }

    fn false_lit(&self) -> &'static str {
        "false"
    }

    fn true_lit(&self) -> &'static str {
        "true"
    }

    fn logic_temp(&mut self) -> String {
        // Ruby reserves `$` for global variables (`$stdout`, `$!`), so a `$` in
        // a local name is a SyntaxError — and even where it parses it would be
        // global state, not the per-binding local this needs. The name is
        // therefore plain, which makes it COLLIDABLE with a program that bound
        // `_ainl_t0` itself, so it is stepped past on a hit.
        loop {
            let n = self.temps;
            self.temps += 1;
            let name = format!("_ainl_t{n}");
            if !self.used.contains(&name) {
                return name;
            }
        }
    }

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
        // The numeric-operand guard shared by the stdlib math builtins —
        // `abs`, `floor`, `sqrt`, `sleep`, `min`, `max`.
        //
        // Same reason as `_alist`/`_ahash`: a host `TypeError` is not AINL's
        // `AinlError`, so it escapes an AINL `catch` and prints a Ruby backtrace
        // with the file and line, and its wording is not the interpreter's
        // either (`min expects a number` rather than
        // `min expects a number, got str`).
        //
        // `Numeric` excludes `true`/`false` in Ruby, so the bool case the other
        // backends guard against needs no separate check here.
        //
        // The builtin passes its own name in because AINL's wording leads with
        // it: `min` and `max` share this helper and must not report each other's
        // name.
        "_anumber",
        "def _anumber(who, x)\n  _error(who + ' expects a number, got ' + _ainl_tname(x)) unless x.is_a?(Numeric)\nend",
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
        // The odd-arity guard is AINL's, not Ruby's. Without it, `(hash 1)`
        // reached `kvs[1]` as nil and quietly built `{1 => nil}`, exiting 0
        // where the interpreter exits 1 with `hash expects an even number of
        // key/value arguments, got 1` — a program that fails on four backends
        // silently succeeding on the fifth, the worst shape of divergence
        // because nothing looks wrong. Python and JS both carry the guard.
        "def _hash(*kvs)\n  _error('hash expects an even number of key/value arguments, got ' + kvs.length.to_s) if kvs.length % 2 != 0\n  out = AHash.new\n  i = 0\n  while i < kvs.length\n    k, v = kvs[i], kvs[i + 1]\n    pair = out.find { |p| p[0] == k }\n    if pair\n      pair[1] = v\n    else\n      out << [k, v]\n    end\n    i += 2\n  end\n  out\nend",
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
    // ---- Tier 3 file system ----
    // Same explicit-rules rule as the six above. The two that would bite a
    // naive port: File.rename CLOBBERS an existing destination silently (where
    // POSIX rename(2) refuses), and Ruby's mkdir takes NO second argument at
    // all — the recursive form is FileUtils.mkdir_p, a different method in a
    // module that is not loaded — so the two modes are genuinely different
    // calls in this host.
    (
        "_fs_probe",
        // The path a filesystem *query* builtin probes, with a trailing
        // separator stripped. The hosts split on whether "f/" is a legal way
        // to name a non-directory (Errno::ENOTDIR here, ENOTDIR in C, a throw
        // in Node, a NotADirectoryError in Python), so it is trimmed. A lone
        // "/" is the root and must survive.
        "def _fs_probe(p)\n  (p.length > 1 && p.end_with?('/')) ? p[0..-2] : p\nend",
    ),
    (
        "_mkdir",
        // The option is a positional string, not a keyword: AINL has no
        // keyword-argument syntax, so ":recursive" arrives as an ordinary
        // string. See docs/SYNTAX.md §3h.
        //
        // Arity is checked here rather than left to a Ruby arity error,
        // because a host ArgumentError/TypeError is not an _AinlError: it would
        // escape an AINL `catch` and print a Ruby backtrace to stderr. (Found
        // by the parity gate, not by reading the code.)
        //
        // An existing path is an error in BOTH modes, checked before the host
        // call so the message is the same on every backend. Dir.mkdir raises
        // Errno::EEXIST and FileUtils.mkdir_p returns quietly, so this explicit
        // check is what pins the strict rule: a caller that creates a
        // directory and then writes into it needs to know whether *it* created
        // it.
        "def _mkdir(*args)\n  _error('mkdir expects (mkdir path) or (mkdir path option)') if args.length < 1 || args.length > 2\n  path = args[0]\n  opt = args.length > 1 ? args[1] : nil\n  _error(\"mkdir expects a str path, got #{_ainl_tname(path)}\") unless path.is_a?(String)\n  unless opt.nil?\n    _error(\"mkdir expects a str option, got #{_ainl_tname(opt)}\") unless opt.is_a?(String)\n    _error(\"mkdir: unknown option '#{opt}'\") unless opt == ':recursive'\n  end\n  # File.lstat, not File.exist?: the interpreter's symlink_metadata is an\n  # lstat, and a broken symlink is still a directory entry mkdir must refuse.\n  begin\n    File.lstat(_fs_probe(path))\n    _error(\"mkdir: cannot create '#{path}': it exists\")\n  rescue SystemCallError\n    # absent, which is the only case that may proceed\n  end\n  begin\n    if opt == ':recursive'\n      # The parents only, one component at a time. FileUtils is deliberately\n      # not used: it is a stdlib require, and mkdir -p there returns quietly\n      # on an existing leaf — which the check above has already excluded, so\n      # building the parents here is both stricter and dependency-free.\n      parent = File.dirname(path)\n      unless parent == '.' || File.directory?(parent)\n        begin\n          _fs_mkdir_p(parent)\n        rescue SystemCallError\n          # reported below as the same 'cannot create'\n        end\n      end\n    end\n    Dir.mkdir(path)\n  rescue SystemCallError\n    _error(\"mkdir: cannot create '#{path}'\")\n  end\nend",
    ),
    (
        // A dependency-free mkdir -p for the one call site above. Kept as its
        // own helper so the recursive body of _mkdir stays readable, and named
        // distinctly so it cannot be confused with the Tier 1 rule set.
        "_fs_mkdir_p",
        "def _fs_mkdir_p(path)\n  parts = path.split('/')\n  cur = path.start_with?('/') ? '/' : ''\n  parts.each do |seg|\n    next if seg.empty?\n    cur = cur.empty? || cur == '/' ? cur + seg : cur + '/' + seg\n    begin\n      Dir.mkdir(cur)\n    rescue SystemCallError\n      raise unless File.directory?(cur)\n    end\n  end\n  true\nend",
    ),
    (
        "_rename",
        // Both pre-checks happen before the host call, and that is the whole
        // point: File.rename CLOBBERS an existing destination silently, just
        // like os.rename and fs.renameSync, where POSIX rename(2) refuses.
        // Without the check, the same program would destroy a file under some
        // backends and preserve it under others.
        //
        // EXDEV is named rather than collapsed into the generic message,
        // because "different filesystems" and "not writable" need different
        // fixes. Ruby carries it as Errno::EXDEV, whose `Errno` module is
        // built in.
        "def _rename(*args)\n  _error('rename expects (rename from to)') if args.length != 2\n  src, dst = args\n  _error(\"rename expects a str path, got #{_ainl_tname(src)}\") unless src.is_a?(String)\n  _error(\"rename expects a str path, got #{_ainl_tname(dst)}\") unless dst.is_a?(String)\n  begin\n    File.lstat(_fs_probe(src))\n  rescue SystemCallError\n    _error(\"rename: cannot move '#{src}': it does not exist\")\n  end\n  begin\n    File.lstat(_fs_probe(dst))\n    _error(\"rename: cannot move '#{src}': '#{dst}' exists\")\n  rescue SystemCallError\n    # absent, which is the only case that may proceed\n  end\n  begin\n    File.rename(src, dst)\n  rescue Errno::EXDEV\n    _error(\"rename: cannot move '#{src}' to '#{dst}': different filesystems\")\n  rescue SystemCallError\n    _error(\"rename: cannot move '#{src}' to '#{dst}'\")\n  end\nend",
    ),
    (
        "_copy",
        // A full read + write, never File.link: a hardlink shares the inode, so
        // a later write-file on either path would silently change both. IO.copy_stream
        // is the stdlib byte copy and truncates an existing destination
        // exactly as write-file does. 'rb'/'wb' so no newline translation can
        // change the bytes.
        "def _copy(*args)\n  _error('copy expects (copy from to)') if args.length != 2\n  src, dst = args\n  _error(\"copy expects a str path, got #{_ainl_tname(src)}\") unless src.is_a?(String)\n  _error(\"copy expects a str path, got #{_ainl_tname(dst)}\") unless dst.is_a?(String)\n  p = _fs_probe(src)\n  begin\n    st = File.lstat(p)\n  rescue SystemCallError\n    _error(\"copy: cannot copy '#{src}': it does not exist\")\n  end\n  _error(\"copy: cannot copy '#{src}': it is a directory\") if st.directory?\n  begin\n    File.open(p, 'rb') do |i|\n      File.open(dst, 'wb') { |o| IO.copy_stream(i, o) }\n    end\n  rescue SystemCallError\n    _error(\"copy: cannot copy '#{src}' to '#{dst}'\")\n  end\nend",
    ),
    (
        "_is_dir",
        // File.lstat + .directory?, not File.directory?: the latter follows a
        // symlink, so a link to a directory would read as a directory where
        // the interpreter's lstat says nil. nil, not false, so
        // `(= (is-dir p) nil)` is the absence test, matching file-exists.
        "def _is_dir(*args)\n  _error('is-dir expects (is-dir path)') if args.length != 1\n  path = args[0]\n  _error(\"is-dir expects a str path, got #{_ainl_tname(path)}\") unless path.is_a?(String)\n  begin\n    st = File.lstat(_fs_probe(path))\n  rescue SystemCallError\n    return nil\n  end\n  st.directory? ? true : nil\nend",
    ),
    (
        "_file_size",
        // Bytes, not characters: a file is a sequence of bytes and there is no
        // encoding in the file, which is what makes this the companion to the
        // byte-indexed string primitives. File.lstat, not File.stat, to keep
        // the no-follow rule.
        //
        // A directory is an error, not a number: POSIX reports the directory's
        // own inode size (4096 on ext4, 60 on APFS, 0 on tmpfs), so answering
        // would report a filesystem implementation detail as a language value.
        "def _file_size(*args)\n  _error('file-size expects (file-size path)') if args.length != 1\n  path = args[0]\n  _error(\"file-size expects a str path, got #{_ainl_tname(path)}\") unless path.is_a?(String)\n  begin\n    st = File.lstat(_fs_probe(path))\n  rescue SystemCallError\n    _error(\"file-size: cannot read '#{path}'\")\n  end\n  _error(\"file-size: cannot read '#{path}': it is a directory\") if st.directory?\n  st.size\nend",
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
    // ---- Tier 3: byte-oriented string primitives ----
    // AINL strings are BYTE strings, but a Ruby String indexes by *character*:
    // `s[a, b]` slices characters, `s.index(sub)` returns a character offset,
    // and `s[a]` is a character. None of the six can delegate to the host, so
    // each works on `s.b` (an Array of Integer bytes) instead and decodes the
    // result back — the same reasoning as _list_dir, which already sorts by
    // `.b` to match the interpreter's byte order. See ainl-core/src/eval.rs for
    // the rules.
    //
    // Every failure goes through `_error`, i.e. raises AinlError, so `catch` can
    // intercept it and the message is AINL's own. A host TypeError would escape
    // the generated `rescue AinlError` clause and abort with a backtrace.
    (
        "_ainl_b",
        // The bytes of a string, rejecting a non-String under the *calling*
        // builtin's name. Shared so the six helpers agree on the type-error
        // wording. A quoted AINL symbol is a real Ruby Symbol, so `is_a?`
        // rejects it the way the interpreter's `as_str_arg` does.
        //
        // `s.bytes` (an Array of Integer), NOT `s.b` — `String#b` returns a
        // binary-encoded *String*, so `b[i]` would be a one-character String and
        // `b[i] & 0xC0` would raise NoMethodError. Every helper below indexes
        // with a single Integer and slices with a length, which is Array
        // behaviour. (`_list_dir` uses `n.b` for a different job — a sort key,
        // where a binary String is exactly right.)
        "def _ainl_b(s, who)\n  _error(\"#{who} expects a str, got #{_ainl_tname(s)}\") unless s.is_a?(String)\n  s.bytes\nend",
    ),
    (
        "_ainl_idx",
        // An int index operand. A Float is rejected rather than truncated: Ruby
        // would raise on a non-integer index anyway, and truncating here would
        // make an arithmetic bug in the caller invisible on three backends.
        "def _ainl_idx(i, who, which)\n  _error(\"#{who} expects an int #{which}index, got #{_ainl_tname(i)}\") unless i.is_a?(Integer)\n  i\nend",
    ),
    (
        "_ainl_off",
        // Resolve a byte offset, rejecting out-of-range and mid-character
        // positions. The `which` argument carries a trailing space for the
        // two-operand builtins, so the message reads `substring start index
        // out of bounds` with no double space.
        "def _ainl_off(b, i, who, which)\n  _error(\"#{who} #{which}index out of bounds\") if i < 0 || i > b.length\n  # A UTF-8 continuation byte cannot start or end a slice.\n  _error(\"#{who} #{which}index splits a multi-byte character\") if i < b.length && (b[i] & 0xC0) == 0x80\n  i\nend",
    ),
    (
        // `end` is a Ruby keyword and cannot be a parameter name, so the bound
        // is called `hi` here and the emitted helper keeps AINL's own
        // "start"/"end" wording in its messages.
        "_substring",
        "def _substring(s, lo_i, hi_i)\n  b = _ainl_b(s, 'substring')\n  lo_i = _ainl_idx(lo_i, 'substring', 'start ')\n  hi_i = _ainl_idx(hi_i, 'substring', 'end ')\n  _error('substring start index is greater than end index') if lo_i > hi_i\n  lo = _ainl_off(b, lo_i, 'substring', 'start ')\n  hi = _ainl_off(b, hi_i, 'substring', 'end ')\n  b[lo...hi].pack('C*').force_encoding('UTF-8')\nend",
    ),
    (
        "_char",
        "def _char(s, i)\n  b = _ainl_b(s, 'char')\n  i = _ainl_idx(i, 'char', '')\n  _error('char index out of bounds') if i < 0 || i >= b.length\n  _error('char index splits a multi-byte character') if (b[i] & 0xC0) == 0x80\n  # The lead byte's high bits give the sequence length: 10xxxxxx=2, 1110=3,\n  # 11110xxx=4. utf8_valid on the way in means no other case can occur.\n  c = b[i]\n  n = c >= 0xF0 ? 4 : (c >= 0xE0 ? 3 : (c >= 0xC0 ? 2 : 1))\n  b[i, n].pack('C*').force_encoding('UTF-8')\nend",
    ),
    (
        "_code",
        "def _code(s, i = nil)\n  b = _ainl_b(s, 'code')\n  if i.nil?\n    _error('code expects a non-empty string') if b.empty?\n    return b[0]\n  end\n  i = _ainl_idx(i, 'code', '')\n  _error('code index out of bounds') if i < 0 || i >= b.length\n  b[i]\nend",
    ),
    (
        "_starts_with",
        "def _starts_with(s, prefix)\n  b = _ainl_b(s, 'starts-with')\n  p = _ainl_b(prefix, 'starts-with')\n  b[0, p.length] == p\nend",
    ),
    (
        "_ends_with",
        "def _ends_with(s, suffix)\n  b = _ainl_b(s, 'ends-with')\n  p = _ainl_b(suffix, 'ends-with')\n  p.length <= b.length && b[b.length - p.length, p.length] == p\nend",
    ),
    (
        // An empty needle is 0, not -1 — the same answer str.find, Ruby and JS
        // all give, and the one that keeps `index-of` consistent with
        // `contains` (whose empty-needle case is already `true`).
        "_index_of",
        "def _index_of(s, sub)\n  b = _ainl_b(s, 'index-of')\n  p = _ainl_b(sub, 'index-of')\n  return 0 if p.empty?\n  _ainl_seq_find(b, p)\nend",
    ),
    (
        // A byte-pattern search. Array#index matches a single Integer, so a
        // multi-byte needle needs the sliding comparison below. Returns the
        // first offset whose window equals the needle, or -1. The needle cannot
        // be empty here (_index_of answers 0 for that), so `last` is >= 0
        // whenever any comparison can succeed.
        "_ainl_seq_find",
        "def _ainl_seq_find(b, p)\n  last = b.length - p.length\n  i = 0\n  while i <= last\n    return i if b[i, p.length] == p\n    i += 1\n  end\n  -1\nend",
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
        "def _sleep(secs)\n  _anumber('sleep', secs)\n  _error('sleep expects a non-negative number') if secs.respond_to?(:nan?) && secs.nan? || secs < 0\n  sleep(secs) if secs > 0\n  nil\nend",
    ),
    (
        "_abs",
        "def _abs(n)\n  _anumber('abs', n)\n  n < 0 ? -n : n\nend",
    ),
    (
        // `who` is passed in rather than derived from `want_max` because the
        // message leads with the builtin's own name, and the one caller that
        // knows which builtin it was is `_min`/`_max`.
        "_minmax",
        "def _minmax(xs, who, want_max)\n  _error(who + ' expects at least 1 argument') if xs.empty?\n  best = xs[0]\n  xs.each do |x|\n    _anumber(who, x)\n    best = x if (want_max ? x > best : x < best)\n  end\n  best\nend",
    ),
    ("_min", "def _min(*xs)\n  _minmax(xs, 'min', false)\nend"),
    ("_max", "def _max(*xs)\n  _minmax(xs, 'max', true)\nend"),
    (
        "_floor",
        "def _floor(n)\n  _anumber('floor', n)\n  n.is_a?(Integer) ? n : n.floor\nend",
    ),
    (
        "_sqrt",
        "def _sqrt(n)\n  _anumber('sqrt', n)\n  _error('sqrt expects a non-negative number') if n < 0\n  Math.sqrt(n)\nend",
    ),
];
