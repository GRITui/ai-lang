//! The AINL bytecode compiler and stack machine.
//!
//! `compile_top` lowers the AST (`Node`) into a top-level [`code::FnCode`] (a
//! flat `Vec<Instr>` plus per-function constant / name / local pools). `run`
//! executes that code on an operand stack with an explicit frame stack, so
//! recursion is a data operation rather than native call-stack recursion.
//!
//! This is the fast execution path. The tree-walking evaluator in `eval.rs` is
//! retained as a fallback and as the semantic reference the VM is checked
//! against (see `run_in_tree_walk`).
//!
//! # Where the speed comes from
//!
//! The tree-walk pays, per node of a hot loop, for: a `DepthGuard`
//! (thread-local depth bump) + `tick()` (thread-local step bump), a `HashMap`
//! lookup per variable, and — for every operator call — a head-eval, an arg
//! `Vec` build, and an `apply` dispatch.
//!
//! The VM replaces all of that:
//! - **O(1) local slots.** Variables `def`-bound in a scope are resolved at
//!   compile time to fixed slot indices into a `Vec<Option<Value>>` in the
//!   frame. `LoadSlot`/`DefSlot` are array accesses — no `HashMap`.
//! - **Inlined ops.** `+ - * / mod = < > <= >= not` are dedicated
//!   instructions that call the *same* helper functions the tree-walk's
//!   builtins use, so their semantics are identical by construction while
//!   skipping the call machinery.
//! - **No TLS in the hot loop.** The step counter is a plain local `u64` and
//!   the depth limit is checked when a frame is pushed (the explicit frame
//!   stack doubles as the recursion guard).
//! - **Env sync only where needed.** A scope only writes its slot values into
//!   the frame's `Env` when a closure exists in its subtree (`env_active`), so
//!   a closure-free hot loop never touches the `HashMap` at all.
//!
//! # Semantics
//!
//! Identical to the tree-walk: same operator results (shared helpers), same
//! `unbound symbol` / `cannot call` / arity errors, same `step limit` and
//! `recursion limit` messages, and the same `LIVE_SCOPES` cycle-breaking (a
//! self-referential `def` cycle rooted in a call's scope is cleared on return
//! unless the result still keeps that scope alive).

use crate::bignum::{self, BigNum};
use crate::code::{FnCode, Instr};
use crate::error::{Error, Result};
use crate::eval::{
    builtin_div, builtin_mod, builtin_sub, compare, numeric_fold, value_keeps_env_alive, Env,
    TryForm, MAX_DEPTH, MAX_STEPS,
};
use crate::parser::{Node, Span};
use crate::value::{Closure, ConsCell, Value};
use std::cmp::Ordering;
use std::rc::Rc;

// ---------------------------------------------------------------------------
// Compiler
// ---------------------------------------------------------------------------

/// A scope being compiled: its local-slot table and its `env_active` flag.
#[derive(Default)]
struct Scope {
    /// `slot -> name`. Slots `0..params.len()` are the params.
    locals: Vec<String>,
    /// True if this scope or a descendant contains a closure.
    env_active: bool,
}

impl Scope {
    /// The slot a variable is bound to, if it is local to this scope.
    fn slot_of(&self, name: &str) -> Option<usize> {
        self.locals.iter().position(|n| n == name)
    }

    /// Bind a new local variable, returning its slot index.
    fn define(&mut self, name: String) -> usize {
        self.locals.push(name);
        self.locals.len() - 1
    }
}

/// Lowers a list of forms into one [`FnCode`]. Owns the scope it compiles
/// against; nested functions / `let`s spawn child compilers with child scopes.
struct Compiler {
    code: FnCode,
    scope: Scope,
    /// Span of the node currently being lowered, stamped onto every
    /// instruction emitted for it.
    ///
    /// Saved and restored around each [`Compiler::compile_expr`], so an
    /// instruction emitted *after* a nested subexpression (a `Pop`, a `MakeFn`)
    /// still carries the span of the form that owns it rather than the last
    /// leaf visited.
    cur_span: Span,
    /// Names bound outside this compilation unit, for suggestion candidates
    /// only — they are never resolved as locals. The REPL needs this: it
    /// compiles one submission at a time, so a name bound on an earlier line
    /// is in the environment but in none of the forms it is compiling.
    extra_names: Vec<String>,
}

impl Compiler {
    fn new_top() -> Self {
        Compiler {
            code: FnCode::new("<top>"),
            scope: Scope::default(),
            cur_span: Span::new(0, 0),
            extra_names: Vec::new(),
        }
    }

    fn new_fn(name: String, params: Vec<String>, variadic: Option<String>) -> Self {
        let mut code = FnCode::new(&name);
        code.params = params.clone();
        code.variadic = variadic.clone();
        let mut locals = params;
        if let Some(r) = &variadic {
            locals.push(r.clone());
        }
        Compiler {
            code,
            scope: Scope {
                locals,
                env_active: false,
            },
            cur_span: Span::new(0, 0),
            extra_names: Vec::new(),
        }
    }

    /// A synthetic function for a `let`: its locals are the enclosing scope's
    /// locals (so `LoadSlot` for an enclosing var keeps working) followed by
    /// the let's binding names.
    fn new_let(bind_names: Vec<String>) -> Self {
        let mut code = FnCode::new("<let>");
        code.params = Vec::new();
        Compiler {
            code,
            scope: Scope {
                locals: bind_names,
                env_active: false,
            },
            cur_span: Span::new(0, 0),
            extra_names: Vec::new(),
        }
    }

    fn body_len(&self) -> usize {
        self.code.body.len()
    }

    fn emit(&mut self, instr: Instr) {
        self.code.push(instr, self.cur_span);
    }

    fn emit_jump(&mut self, kind: fn(usize) -> Instr) -> usize {
        self.emit(kind(0));
        self.body_len() - 1
    }

    fn patch_jump(&mut self, idx: usize, target: usize) {
        match self.code.body[idx] {
            Instr::Jump(_) => self.code.body[idx] = Instr::Jump(target),
            Instr::JumpIfFalse(_) => self.code.body[idx] = Instr::JumpIfFalse(target),
            Instr::JumpIfTrue(_) => self.code.body[idx] = Instr::JumpIfTrue(target),
            _ => unreachable!("patch_jump on non-jump instruction"),
        }
    }

    fn push_const(&mut self, v: Value) {
        self.code.consts.push(v);
        self.emit(Instr::LoadConst(self.code.consts.len() - 1));
    }

    fn name_idx(&mut self, name: &str) -> usize {
        self.code
            .names
            .iter()
            .position(|n| n == name)
            .unwrap_or_else(|| {
                self.code.names.push(name.to_string());
                self.code.names.len() - 1
            })
    }

    fn emit_load_var(&mut self, name: &str) {
        match self.scope.slot_of(name) {
            Some(slot) => self.emit(Instr::LoadSlot(slot)),
            None => {
                let i = self.name_idx(name);
                // Resolve the close-match suggestion *here*, at compile time:
                // the compiler knows every name the program will ever bind
                // (including a `def` further down the file, which the runtime
                // env does not hold yet), so it is the only place that can
                // produce the same answer the tree-walking evaluator does.
                // Storing it keeps the hot loop free of any suggestion work.
                let cands = self.all_known_names();
                if let Some(m) = crate::suggest::close_match(name, cands) {
                    self.code.name_suggestions.insert(i, m);
                }
                self.emit(Instr::Load(i));
            }
        }
    }

    /// Every name the compiler can see: the prelude's builtins plus every
    /// `def`-bound name collected in this scope chain, plus any `extra` the
    /// caller supplied.
    fn all_known_names(&self) -> Vec<String> {
        let mut names: std::collections::BTreeSet<String> = crate::eval::builtin_names()
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        names.extend(self.scope.locals.iter().cloned());
        names.extend(self.extra_names.iter().cloned());
        names.into_iter().collect()
    }

    fn emit_def_var(&mut self, name: &str) {
        match self.scope.slot_of(name) {
            Some(slot) => self.emit(Instr::DefSlot(slot)),
            None => {
                let i = self.name_idx(name);
                self.emit(Instr::Def(i));
            }
        }
    }

    /// Compile a single expression (leaves one value on the stack).
    ///
    /// Every instruction emitted while lowering `node` is stamped with `node`'s
    /// span, so a failure at run time can be traced back to the form that
    /// caused it. The previous span is restored on the way out, so a `Pop` or
    /// `MakeFn` emitted by the *caller* after this call still points at the
    /// caller's form.
    fn compile_expr(&mut self, node: &Node) -> Result<()> {
        let saved = self.cur_span;
        self.cur_span = node.span();
        let r = self.compile_expr_inner(node);
        self.cur_span = saved;
        r
    }

    fn compile_expr_inner(&mut self, node: &Node) -> Result<()> {
        match node {
            Node::Int(i, _) => self.push_const(Value::Int(BigNum::small(*i))),
            Node::Float(x, _) => self.push_const(Value::Float(*x)),
            Node::Str(s, _) => self.push_const(Value::str(s.clone())),
            Node::Sym(name, _) => match name.as_str() {
                "true" => self.emit(Instr::Bool(true)),
                "false" => self.emit(Instr::Bool(false)),
                "nil" => self.emit(Instr::Nil),
                _ => self.emit_load_var(name),
            },
            Node::List(items, _) => self.compile_list(items)?,
        };
        Ok(())
    }

    /// Compile a list node: a special form, an inlined op, or a call.
    fn compile_list(&mut self, items: &[Node]) -> Result<()> {
        let Some(head) = items.first() else {
            self.emit(Instr::Nil); // empty list evaluates to nil
            return Ok(());
        };
        if let Node::Sym(op, _) = head {
            // A malformed special form fails on its *shape*; the form's own
            // span is the most precise position there is, so it is stamped on
            // the way out. Mirrors the tree-walk's dispatch, which keeps the
            // two backends' messages identical (4-backend rule).
            let at = self.cur_span.start;
            let shape = |r: Result<()>| r.map_err(|e| e.or_at(at));
            match op.as_str() {
                "def" => return shape(self.sf_def(&items[1..])),
                "fn" => return shape(self.sf_fn(&items[1..])),
                "if" => return shape(self.sf_if(&items[1..])),
                "do" => return shape(self.sf_do(&items[1..])),
                "let" => return shape(self.sf_let(&items[1..])),
                "while" => return shape(self.sf_while(&items[1..])),
                "quote" => return shape(self.sf_quote(&items[1..])),
                "try" => return shape(self.sf_try(&items[1..])),
                "and" => return shape(self.sf_and(&items[1..])),
                "or" => return shape(self.sf_or(&items[1..])),
                op if is_inlined_op(op) => return self.compile_inlined_op(op, &items[1..]),
                _ => {}
            }
        }
        self.compile_call(items)
    }

    // ---- special forms -----------------------------------------------------

    fn sf_def(&mut self, args: &[Node]) -> Result<()> {
        let [name_node, val_node] = args else {
            return Err(Error::runtime("def expects (def name value)"));
        };
        let Node::Sym(name, _) = name_node else {
            return Err(Error::runtime("def name must be a symbol"));
        };
        self.compile_expr(val_node)?;
        self.emit_def_var(name);
        Ok(())
    }

    fn sf_fn(&mut self, args: &[Node]) -> Result<()> {
        let Some((params_node, body)) = args.split_first() else {
            return Err(Error::runtime("fn expects (fn (params...) body...)"));
        };
        let Node::List(param_nodes, _) = params_node else {
            return Err(Error::runtime("fn params must be a list"));
        };
        let (params, variadic) = parse_params(param_nodes).map_err(Error::runtime)?;
        let name = fn_name(&params, &variadic);
        let mut fnc = Compiler::new_fn(name, params, variadic);
        collect_defs(body, &mut fnc.scope);
        // A fn body is a `do`: only the last form's value is returned
        // (tree-walk keeps `last` and discards intermediates). Without the
        // `Pop`s, the name symbols that `def` pushes back would accumulate on
        // the operand stack and corrupt the enclosing call.
        if body.is_empty() {
            fnc.emit(Instr::Nil);
        } else {
            let last = body.len() - 1;
            for (i, form) in body.iter().enumerate() {
                fnc.compile_expr(form)?;
                if i != last {
                    fnc.emit(Instr::Pop);
                }
            }
        }
        fnc.emit(Instr::Ret);
        fnc.code.locals = fnc.scope.locals.clone();
        fnc.code.env_active = fnc.scope.env_active;
        fnc.code.sync_slot_syms();
        let rc = Rc::new(fnc.code);
        self.scope.env_active = true; // a closure now exists in this subtree
        self.code.fns.push(rc);
        self.emit(Instr::MakeFn(self.code.fns.len() - 1));
        Ok(())
    }

    fn sf_if(&mut self, args: &[Node]) -> Result<()> {
        match args {
            [cond, then] => {
                self.compile_expr(cond)?;
                let jump = self.emit_jump(Instr::JumpIfFalse);
                self.compile_expr(then)?;
                // A 2-arg `if` is the 3-arg form with an implicit `nil` else,
                // so it needs the SAME jump over that trailing `Nil` the
                // 3-arg case emits. Without it a TRUE condition falls through
                // and pushes the then-value AND the `Nil`, leaving two values
                // where the enclosing form expects one: in a `fn`/`let` body
                // the extra `Nil` survives the form's own `Pop` and shifts
                // every following form's operand ("cannot call a nil"). A
                // `while` loop masked it, because its per-iteration pop ate the
                // surplus.
                let jump2 = self.emit_jump(Instr::Jump);
                self.patch_jump(jump, self.body_len());
                self.emit(Instr::Nil);
                self.patch_jump(jump2, self.body_len());
            }
            [cond, then, els] => {
                self.compile_expr(cond)?;
                let jump = self.emit_jump(Instr::JumpIfFalse);
                self.compile_expr(then)?;
                let jump2 = self.emit_jump(Instr::Jump);
                self.patch_jump(jump, self.body_len());
                self.compile_expr(els)?;
                self.patch_jump(jump2, self.body_len());
            }
            _ => return Err(Error::runtime("if expects (if cond then [else])")),
        }
        Ok(())
    }

    fn sf_do(&mut self, args: &[Node]) -> Result<()> {
        if args.is_empty() {
            self.emit(Instr::Nil);
            return Ok(());
        }
        let last = args.len() - 1;
        for (i, form) in args.iter().enumerate() {
            self.compile_expr(form)?;
            if i != last {
                self.emit(Instr::Pop);
            }
        }
        Ok(())
    }

    fn sf_let(&mut self, args: &[Node]) -> Result<()> {
        let Some((binds_node, body)) = args.split_first() else {
            return Err(Error::runtime("let expects (let ((n v)...) body...)"));
        };
        let Node::List(binds, _) = binds_node else {
            return Err(Error::runtime("let bindings must be a list"));
        };
        let mut bind_names = Vec::new();
        for b in binds {
            let Node::List(pair, _) = b else {
                return Err(Error::runtime("each let binding must be (name value)"));
            };
            let [Node::Sym(name, _), _] = &pair[..] else {
                return Err(Error::runtime("each let binding must be (name value)"));
            };
            bind_names.push(name.clone());
        }

        // The let-scope's locals = enclosing locals + binding names, so the
        // body (and the binding values) can `LoadSlot` enclosing vars.
        let mut fnc = Compiler::new_let(bind_names.clone());
        collect_defs(body, &mut fnc.scope);

        // Bindings: evaluate each value (in the let scope, which sees the
        // enclosing vars through the copied slots / child env), then bind it.
        for (b, name) in binds.iter().zip(bind_names.iter()) {
            let Node::List(pair, _) = b else {
                unreachable!("validated above");
            };
            let [_, val_node] = &pair[..] else {
                unreachable!("validated above");
            };
            fnc.compile_expr(val_node)?;
            fnc.emit_def_var(name);
            // `DefSlot`/`Def` pushes the binding's name symbol back (matching
            // tree-walk's `def` return value); drop it — only the last body
            // form's value is the let's result.
            fnc.emit(Instr::Pop);
        }
        // The let body is a `do`: keep only the last form's value.
        if body.is_empty() {
            fnc.emit(Instr::Nil);
        } else {
            let last = body.len() - 1;
            for (i, form) in body.iter().enumerate() {
                fnc.compile_expr(form)?;
                if i != last {
                    fnc.emit(Instr::Pop);
                }
            }
        }
        fnc.emit(Instr::Ret);
        fnc.code.locals = fnc.scope.locals.clone();
        fnc.code.env_active = fnc.scope.env_active;
        fnc.code.sync_slot_syms();
        let rc = Rc::new(fnc.code);
        self.scope.env_active = true; // let body may reference enclosing vars via env
        self.code.fns.push(rc);
        self.emit(Instr::MakeFn(self.code.fns.len() - 1));
        self.emit(Instr::Call(0));
        Ok(())
    }

    fn sf_while(&mut self, args: &[Node]) -> Result<()> {
        let Some((cond, body)) = args.split_first() else {
            return Err(Error::runtime("while expects (while cond body...)"));
        };
        // `while` returns the value of the last body form of the last
        // iteration (or nil if the body never ran) — matching tree-walk
        // `sf_while`. A running `last` is kept on the operand stack: each
        // iteration evaluates the condition on top of it, and on a truthy
        // condition we drop the stale `last` and run the body as a `do`
        // (every form but the last is popped; the last form's value becomes
        // the new `last`). This is 2 instructions cheaper per iteration than
        // the per-form `Swap`+`Pop` bookkeeping.
        self.emit(Instr::Nil); // initial `last`
        let start = self.body_len();
        self.compile_expr(cond)?;
        let exit = self.emit_jump(Instr::JumpIfFalse);
        if !body.is_empty() {
            self.emit(Instr::Pop); // drop the stale `last` before re-running
            let last_idx = body.len() - 1;
            for (i, form) in body.iter().enumerate() {
                self.compile_expr(form)?;
                if i != last_idx {
                    self.emit(Instr::Pop);
                }
            }
        }
        self.emit(Instr::Jump(start));
        self.patch_jump(exit, self.body_len());
        // `exit`: the operand stack holds `last`.
        Ok(())
    }

    fn sf_quote(&mut self, args: &[Node]) -> Result<()> {
        let [node] = args else {
            return Err(Error::runtime("quote expects one form"));
        };
        self.push_const(quote_node(node));
        Ok(())
    }

    /// `(try body... (catch (e) handler...))`.
    ///
    /// Lowered as **two sibling `FnCode`s** — one for the body, one for the
    /// handler — plus a `PushTryHandler`/`PopTryHandler` pair around the body.
    /// Each side is then an ordinary function call, which is what makes the
    /// body a genuine scope: its `def`s are its own local slots, invisible to
    /// the handler. Reusing the `let` machinery is deliberate — `let` already
    /// needs "a nested scope with the enclosing locals visible", and `try`'s
    /// body needs exactly that.
    ///
    /// The handler is reached two ways, and the VM must not care which: the
    /// body's `Ret` lands on `PopTryHandler` when it completed normally (the
    /// body's own value is the result of the whole `try`), or the unwind path
    /// in `vm::run` sets `ip` to this `try`'s recorded `handler_pc` when a
    /// frame inside the body raised.
    fn sf_try(&mut self, args: &[Node]) -> Result<()> {
        // Shared with the tree-walk (`eval::parse_try`) so the two evaluators
        // cannot disagree about which body is protected or what `e` binds to.
        let TryForm {
            body,
            param,
            handler,
        } = crate::eval::parse_try(args)?;

        // The body, as its own scope. `new_let` with no bindings gives a child
        // scope that can still LoadSlot the enclosing function's locals.
        let mut bodyc = Compiler::new_let(Vec::new());
        collect_defs(body, &mut bodyc.scope);
        self.emit_fn_tail(&mut bodyc, body)?;
        self.push_fn(bodyc.code);

        // The handler, likewise its own scope, taking the error as its single
        // parameter.
        let mut handc = Compiler::new_fn(format!("(catch {param})"), vec![param.to_string()], None);
        collect_defs(handler, &mut handc.scope);
        self.emit_fn_tail(&mut handc, handler)?;
        self.push_fn(handc.code);
        let handler_idx = self.code.fns.len() - 1;

        // --- layout ---
        //   PushTryHandler{pc, fn}              ; also resets the step budget
        //   MakeFn(body) ; Call(0) ; PopTryHandler ; Jump(join)
        //   handler_pc: Call(1)                 ; <- the unwind path lands here
        //   join:
        //
        // The success path jumps *over* the handler, so the body's own value
        // is the result of the whole `try` and the handler never runs.
        //
        // The unwind path arrives at `handler_pc` having pushed the handler
        // closure and the error value itself, in `Call`'s calling order
        // (callee below argument). That is why there is no `MakeFn` here: the
        // marker already recorded `handler_idx`, so the unwind can build the
        // callee directly instead of re-entering bytecode mid-expression.
        // Both paths therefore arrive at the same `Call(1)` with the same
        // operand shape, and the body's value never needs a `Pop` to get out
        // of the way.
        self.emit(Instr::PushTryHandler {
            handler_pc: 0,
            handler_fn: handler_idx,
        });
        let marker = self.body_len() - 1;
        self.emit(Instr::MakeFn(self.code.fns.len() - 2));
        self.emit(Instr::Call(0));
        self.emit(Instr::PopTryHandler);
        let join = self.emit_jump(Instr::Jump);

        let handler_pc = self.body_len();
        self.emit(Instr::Call(1));
        self.patch_jump(join, self.body_len());

        match self.code.body[marker] {
            Instr::PushTryHandler { handler_fn, .. } => {
                self.code.body[marker] = Instr::PushTryHandler {
                    handler_pc,
                    handler_fn,
                }
            }
            _ => unreachable!("sf_try: marker is a PushTryHandler"),
        }
        Ok(())
    }

    /// Emit `forms` as a `do` body (all but the last popped) plus `Ret`, and
    /// finalize the code: locals table, env flag, slot symbols. Shared by
    /// `sf_let`, `sf_try`'s two halves, and so they cannot drift in how a
    /// nested scope is finalized.
    fn emit_fn_tail(&mut self, c: &mut Compiler, forms: &[Node]) -> Result<()> {
        if forms.is_empty() {
            c.emit(Instr::Nil);
        } else {
            let last = forms.len() - 1;
            for (i, form) in forms.iter().enumerate() {
                c.compile_expr(form)?;
                if i != last {
                    c.emit(Instr::Pop);
                }
            }
        }
        c.emit(Instr::Ret);
        c.code.locals = c.scope.locals.clone();
        c.code.env_active = c.scope.env_active;
        c.code.sync_slot_syms();
        Ok(())
    }

    /// Register a finished nested `FnCode` and mark this scope as
    /// closure-bearing (its body may reference the enclosing env).
    fn push_fn(&mut self, code: FnCode) {
        self.scope.env_active = true;
        self.code.fns.push(Rc::new(code));
    }

    fn sf_and(&mut self, args: &[Node]) -> Result<()> {
        // Returns the first falsey value, or the last value if all truthy
        // (nil-ary -> true). `Dup` preserves the value across `JumpIfFalse`
        // (which pops the tested copy).
        if args.is_empty() {
            self.emit(Instr::Bool(true));
            return Ok(());
        }
        let mut jumps = Vec::new();
        for (i, a) in args.iter().enumerate() {
            self.compile_expr(a)?;
            self.emit(Instr::Dup);
            let jump = self.emit_jump(Instr::JumpIfFalse);
            jumps.push(jump);
            if i != args.len() - 1 {
                self.emit(Instr::Pop); // discard the truthy value, eval next
            }
        }
        for j in jumps {
            self.patch_jump(j, self.body_len());
        }
        Ok(())
    }

    fn sf_or(&mut self, args: &[Node]) -> Result<()> {
        // Returns the first truthy value, or false if none (nil-ary -> false).
        if args.is_empty() {
            self.emit(Instr::Bool(false));
            return Ok(());
        }
        let mut jumps = Vec::new();
        for a in args {
            self.compile_expr(a)?;
            self.emit(Instr::Dup);
            let jump = self.emit_jump(Instr::JumpIfTrue);
            jumps.push(jump);
            self.emit(Instr::Pop); // discard the falsey value, eval next
        }
        self.emit(Instr::Bool(false));
        for j in jumps {
            self.patch_jump(j, self.body_len());
        }
        Ok(())
    }

    // ---- inlined ops & calls ----------------------------------------------

    fn compile_inlined_op(&mut self, op: &str, args: &[Node]) -> Result<()> {
        // `not` is unary.
        if op == "not" && args.len() == 1 {
            self.compile_expr(&args[0])?;
            self.emit(Instr::Not);
            return Ok(());
        }
        // `-` / `/` are unary (neg / reciprocal) or binary.
        if (op == "-" || op == "/") && args.len() == 1 {
            self.compile_expr(&args[0])?;
            self.emit(if op == "-" { Instr::Neg } else { Instr::Recip });
            return Ok(());
        }
        // Binary inlining for the common 2-arg case.
        if args.len() == 2 {
            self.compile_expr(&args[0])?;
            self.compile_expr(&args[1])?;
            self.emit(match op {
                "+" => Instr::Add,
                "*" => Instr::Mul,
                "-" => Instr::Sub,
                "/" => Instr::Div,
                "mod" => Instr::Mod,
                "=" => Instr::CmpEq,
                "<" => Instr::CmpLt,
                ">" => Instr::CmpGt,
                "<=" => Instr::CmpLe,
                ">=" => Instr::CmpGe,
                _ => unreachable!("is_inlined_op"),
            });
            return Ok(());
        }
        // Other arities: fall back to a general call of the (possibly
        // shadowed) operator symbol, which dispatches to the same builtin.
        let mut full = Vec::with_capacity(args.len() + 1);
        full.push(Node::Sym(op.to_string(), Span::new(0, 0)));
        full.extend(args.to_vec());
        self.compile_call(&full)
    }

    fn compile_call(&mut self, items: &[Node]) -> Result<()> {
        let n = items.len() - 1;
        self.compile_expr(&items[0])?;
        for a in &items[1..] {
            self.compile_expr(a)?;
        }
        self.emit(Instr::Call(n));
        // Record the callee's source name against this `Call` so an arity
        // error can name the function the reader called, not just "fn".
        if let Node::Sym(name, _) = &items[0] {
            let idx = self.body_len() - 1;
            self.code.call_names.insert(idx, name.clone());
        }
        Ok(())
    }
}

/// Collect the names `def`-bound directly in `forms` (recursing through
/// non-scoping forms, stopping at `fn`/`let` which open new scopes). This lets
/// the compiler resolve every reference to a `def`-bound name as a local slot,
/// even when the reference textually precedes the `def`.
fn collect_defs(forms: &[Node], scope: &mut Scope) {
    for node in forms {
        let Node::List(items, _) = node else {
            continue;
        };
        if let Some(Node::Sym(op, _)) = items.first() {
            match op.as_str() {
                // New scope. `try`'s body and handler are each their own
                // scope (see `Compiler::sf_try`), so a `def` in either is that
                // nested function's local — not a slot in the enclosing one.
                // This also stops the enclosing scope from reserving a slot
                // for a name the body can never see.
                "fn" | "let" | "try" => continue,
                "def" => {
                    if let Some(Node::Sym(name, _)) = items.get(1) {
                        scope.define(name.clone());
                    }
                    for item in items.get(2..).unwrap_or(&[]) {
                        collect_defs(std::slice::from_ref(item), scope);
                    }
                    continue;
                }
                _ => {}
            }
        }
        collect_defs(items, scope);
    }
}

/// Compile a top-level program (a sequence of forms) into one `FnCode`.
/// Malformed special forms (wrong arity, non-symbol `def` name, …) are
/// compile-time errors — the compiler returns `Err` rather than emitting a
/// runtime-error instruction.
pub fn compile_top(forms: &[Node]) -> Result<FnCode> {
    compile_top_seeded(forms, Vec::new())
}

/// [`compile_top`], with extra names available as suggestion candidates.
///
/// The extra names are *not* resolved as locals — they live in a caller's
/// environment, not in these forms — so this only widens what an unbound-symbol
/// error can propose.
fn compile_top_seeded(forms: &[Node], extra_names: Vec<String>) -> Result<FnCode> {
    let mut c = Compiler::new_top();
    c.extra_names = extra_names;
    // Lower the `map` / `filter` / `reduce` special forms to `let` + `while`
    // loops *before* `collect_defs`, which is the whole reason it is here and
    // not inside `compile_expr`: the generated accumulators are `def`-bound, so
    // the scope pass has to see them to give them a local slot. Lowering
    // later would compile a `def` for a name the slot table never reserved, and
    // the read of that name in the next iteration would find it unbound.
    //
    // `collect_defs` is recursive over the whole tree, so the two `def`s
    // generated inside the `do` block are found along with the user's own.
    let forms = &crate::collection_forms::lower(forms)?;
    collect_defs(forms, &mut c.scope);
    let last = forms.len().saturating_sub(1);
    for (i, form) in forms.iter().enumerate() {
        c.compile_expr(form)?;
        if i != last {
            c.emit(Instr::Pop);
        }
    }
    if forms.is_empty() {
        c.emit(Instr::Nil);
    }
    c.code.locals = c.scope.locals.clone();
    c.code.env_active = c.scope.env_active;
    c.code.sync_slot_syms();
    Ok(c.code)
}

// ---------------------------------------------------------------------------
// Compiler helpers
// ---------------------------------------------------------------------------

fn is_inlined_op(op: &str) -> bool {
    matches!(
        op,
        "+" | "*" | "-" | "/" | "mod" | "=" | "<" | ">" | "<=" | ">=" | "not"
    )
}

fn parse_params(
    param_nodes: &[Node],
) -> std::result::Result<(Vec<String>, Option<String>), &'static str> {
    let mut params = Vec::new();
    let mut variadic = None;
    let mut i = 0;
    while i < param_nodes.len() {
        let Node::Sym(p, _) = &param_nodes[i] else {
            return Err("fn params must be symbols");
        };
        if p == "&" {
            let Some(Node::Sym(rest, _)) = param_nodes.get(i + 1) else {
                return Err("'&' must be followed by a rest parameter");
            };
            variadic = Some(rest.clone());
            break;
        }
        params.push(p.clone());
        i += 1;
    }
    Ok((params, variadic))
}

fn fn_name(params: &[String], variadic: &Option<String>) -> String {
    let mut s = String::from("(");
    for (i, p) in params.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(p);
    }
    if let Some(r) = variadic {
        if !params.is_empty() {
            s.push(' ');
        }
        s.push_str(&format!("& {r}"));
    }
    s.push(')');
    s
}

fn quote_node(node: &Node) -> Value {
    match node {
        Node::Int(i, _) => Value::Int(BigNum::small(*i)),
        Node::Float(x, _) => Value::Float(*x),
        Node::Str(s, _) => Value::str(s.clone()),
        Node::Sym(s, _) => Value::Sym(Rc::new(s.clone())),
        Node::List(items, _) => Value::List(ConsCell::from_values(items.iter().map(quote_node))),
    }
}

// ---------------------------------------------------------------------------
// Stack machine
// ---------------------------------------------------------------------------

/// Clone a local slot's value, taking the in-range integer case as a bare
/// `i64` copy.
///
/// `LoadSlot` clones whatever it finds in the slot, and an integer variable is
/// the common case in every loop. `Value`'s own `Clone` must branch on the
/// variant to decide whether an `Rc` refcount needs bumping — which it cannot
/// know statically — so the int case pays for a branch it would never take.
/// Rebuilding `Value::Int(BigNum::Small(n))` from the known-small `n` skips
/// that: the refcount question cannot arise, because there is no `Rc`.
///
/// Anything else (a bignum, a list, a closure, an unbound slot) falls through
/// to an ordinary clone.
#[inline]
fn small_int_or_clone(slot: &Option<Value>) -> Option<Value> {
    match slot {
        None => None,
        Some(Value::Int(BigNum::Small(n))) => Some(Value::Int(BigNum::Small(*n))),
        Some(Value::Int(big @ BigNum::Big(_))) => Some(Value::Int(big.clone())),
        Some(other) => Some(other.clone()),
    }
}

/// One activation on the explicit frame stack.
struct Frame {
    code: Rc<FnCode>,
    ip: usize,
    /// Local slots. `None` = not yet bound. Slots `0..params.len()` are the
    /// params; the rest are variables `def`-bound in the body.
    locals: Vec<Option<Value>>,
    /// The frame's lexical scope (for `Load`/`Def` and closure capture).
    env: Env,
}

/// The source name of the callee of the `Call` at instruction index `call_ip`,
/// or `"anonymous"` when the call site was not a symbol.
///
/// Only called on error paths: it clones out of the compile-time table, and a
/// per-call lookup on the hot loop would cost a hash and an allocation for a
/// diagnostic that almost never fires.
fn self_callee_name(frames: &[Frame], call_ip: usize) -> String {
    frames
        .last()
        .and_then(|f| f.code.call_names.get(&call_ip))
        .cloned()
        .unwrap_or_else(|| "anonymous".to_string())
}

/// Bind `args` into a fresh local-slot vector for a call to `fnc`, defining
/// each parameter in `call_env` when the function's scope is env-backed.
///
/// The parameter slots come first, then the variadic rest slot (if any), then
/// one `None` per remaining body local. Shared by the in-loop `Call` and by
/// [`call_closure`] so a closure called from a builtin is entered exactly the
/// same way as one called by bytecode — a divergence here would show up as a
/// comparator that misbehaves only inside `sort`.
fn bind_params(fnc: &FnCode, args: &[Value], call_env: &Env) -> Vec<Option<Value>> {
    let np = fnc.params.len();
    let n_body = fnc.locals.len() - np - usize::from(fnc.variadic.is_some());
    let mut locals = Vec::with_capacity(fnc.locals.len());
    for (pname, val) in fnc.params.iter().zip(args.iter()) {
        locals.push(Some(val.clone()));
        if fnc.env_active {
            call_env.define(pname.clone(), val.clone());
        }
    }
    if let Some(rest) = &fnc.variadic {
        let extra: Vec<Value> = args[np.min(args.len())..].to_vec();
        let rest_val = Value::List(ConsCell::from_values(extra));
        locals.push(Some(rest_val.clone()));
        if fnc.env_active {
            call_env.define(rest, rest_val);
        }
    }
    for _ in 0..n_body {
        locals.push(None);
    }
    locals
}

/// The source span of the instruction at index `ip` in the current frame.
///
/// Called only from error paths. Reading the span table on *every* instruction
/// instead cost roughly 2.5x of the VM's throughput (the 40k benchmark's
/// speedup gate fell from 8.5x to 3.0x) because the extra indexing breaks the
/// single-borrow prologue the hot loop is built around. Fetching it here, once
/// something has already gone wrong, costs nothing measurable and returns
/// exactly the same value.
fn cur_span(frames: &[Frame], ip: usize) -> usize {
    frames.last().map(|f| f.code.span_at(ip).start).unwrap_or(0)
}

/// One live `(try … (catch …))`, recorded by [`Instr::PushTryHandler`].
///
/// The interpreter and the tree-walk return `Err` up their native call stacks,
/// so unwinding is free there. The VM has no native stack to unwind — it has a
/// frame stack and a `Result` in each loop body — so it records the same
/// information here and turns a raised `Error` into a jump. What has to be
/// captured is exactly what a native unwinder would restore:
///
/// * **`stack_len`** — the operand-stack depth on entry. A frame that raised
///   may have left partial operands (a call's arguments, say) on the stack;
///   truncating back to this depth is what makes the handler see the same
///   operand stack the body's start did.
/// * **`frame_depth`** — how many *call* frames the body pushed. Those frames
///   are popped, which also means their scopes get cleared (see the `Ret`
///   note) rather than leaked.
/// * **`steps`** — the step count on entry, so the body gets a fresh budget
///   (see `eval::sf_try` for why) and the *enclosing* run is not charged for
///   the caught one.
struct TryHandler {
    /// Bytecode offset to jump to when a frame inside the region raises.
    handler_pc: usize,
    /// Index into the enclosing frame's `fns` of the `catch` closure. The
    /// unwind path builds the callee itself rather than re-entering bytecode
    /// at a `MakeFn`, so the marker has to carry it.
    handler_fn: usize,
    /// Operand-stack depth when the protected region began.
    stack_len: usize,
    /// Number of call frames on `frames` when it began.
    frame_depth: usize,
    /// Step count when the region began.
    steps: u64,
}

/// Execute a compiled top-level program in `env`. Returns the value of the
/// final form. `env` is the global (REPL) environment: top-level `def`s are
/// persisted into it so bindings survive across `run_in` calls.
pub fn run(top: &FnCode, env: &Env) -> Result<Value> {
    // Top-level frame: pre-fill local slots from the global env so a var
    // defined in an earlier run is visible before it is re-`def`d.
    let mut top_locals: Vec<Option<Value>> = Vec::with_capacity(top.locals.len());
    for name in &top.locals {
        top_locals.push(env.get(name));
    }
    run_with_locals(top, env, top_locals, false)
}

/// [`run`], with the initial frame's local slots supplied by the caller instead
/// of being looked up by name in `env`, and an optional **sentinel** base frame
/// underneath it.
///
/// Two things [`run`] does by name are wrong for a *function* run, and
/// [`call_closure`] needs both undone:
///
/// * `run` seeds the top frame's slots by looking each name up in `env`. For a
///   closure the parameters were bound positionally by [`bind_params`], and they
///   are not defined in the fresh call env — so `run` would find nothing and
///   every parameter would read as unbound (`unbound symbol 'a'` inside a `sort`
///   comparator). Hence the caller-supplied slots.
/// * `run`'s top frame is a top-level `FnCode`, which ends by *falling off the
///   end of `body`*. A compiled function ends with `Ret`, which **pops** the
///   frame — and when that frame is the only one, the next loop iteration
///   unwraps `None` and panics. Hence the sentinel: an empty `FnCode` beneath,
///   which the loop exhausts and breaks on.
fn run_with_locals(
    top: &FnCode,
    env: &Env,
    top_locals: Vec<Option<Value>>,
    sentinel: bool,
) -> Result<Value> {
    let mut stack: Vec<Value> = Vec::new();
    let mut steps: u64 = 0;
    // Live `try` regions, innermost last. Empty for any program without a
    // `try`, and never touched by the hot loop, so a program that does not use
    // `catch` pays nothing for the feature.
    let mut handlers: Vec<TryHandler> = Vec::new();

    let real = Frame {
        code: Rc::new(top.clone()),
        ip: 0,
        locals: top_locals,
        env: env.clone(),
    };
    let mut frames: Vec<Frame> = Vec::with_capacity(if sentinel { 2 } else { 1 });
    if sentinel {
        // An empty program **underneath**, so the function's own `Ret` pops
        // *itself* and leaves this one, which the loop then exhausts and breaks
        // on. See the note on `run_with_locals` for why a function run needs
        // this and a program run does not.
        //
        // It must be pushed *first*: the loop executes `frames.last_mut()`, so
        // the frame that actually runs is the one on top. Pushing the sentinel
        // last made it the frame that ran — an empty body, so the loop broke on
        // the first iteration and the function never executed at all (every
        // comparator silently returned nil).
        frames.push(Frame {
            code: Rc::new(FnCode::new("")),
            ip: 0,
            locals: Vec::new(),
            env: Env::new(),
        });
    }
    frames.push(real);

    // Raise `e` in the VM loop: deliver it to the innermost enclosing `try` if
    // there is one, otherwise fail the run.
    //
    // A macro rather than a function so it can `continue`/`return` out of the
    // loop; that is the whole point, and it is why the unwind body is written
    // once here instead of at each of the loop's error sites. Using it at
    // every site is what makes "innermost catch wins" true *by construction*
    // rather than by remembering to route each new error path through here.
    macro_rules! raise {
        ($e:expr) => {{
            let err: Error = $e;
            let Some(h) = handlers.pop() else {
                return Err(err);
            };
            // Discard whatever the failing frame left on the operand stack:
            // a partially-applied call's arguments, a half-built list. The
            // handler must see the same stack the body's first instruction did.
            stack.truncate(h.stack_len);
            // Drop the frames the body pushed, clearing each scope exactly as
            // `Instr::Ret` does — a self-referential `def` cycle rooted in a
            // discarded frame would otherwise leak the whole scope chain.
            while frames.len() > h.frame_depth {
                let f = frames.pop().expect("depth checked above");
                f.env.clear();
            }
            // The body ran on its own budget; give it back rather than
            // charging the enclosing run for work it did not do.
            steps = h.steps;
            // Resume at `handler_pc`, which is the `Pop ; Call(1)` pair. `Call`
            // pops its arguments first and the callee *below* them, so both
            // operands are pushed here, callee-first. The callee is built from
            // the enclosing frame's fn table — the same closure a `MakeFn` at
            // that point would have produced, so the handler runs in exactly
            // the scope the surrounding code is in.
            let frame = frames
                .last_mut()
                .expect("a `try` is always pushed by a frame");
            let fnc = &frame.code.fns[h.handler_fn];
            stack.push(Value::Closure(Rc::new(Closure {
                params: fnc.params.clone(),
                variadic: fnc.variadic.clone(),
                body: Vec::new(), // the VM runs `code`, not `body`
                env: frame.env.clone(),
                code: Some(Rc::clone(fnc)),
            })));
            stack.push(err.to_value());
            frame.ip = h.handler_pc;
            continue;
        }};
    }

    // [`raise!`] for an expression that already yields a `Result`: the `Ok`
    // arm is the value, the `Err` arm unwinds.
    //
    // This exists because `?` in this loop would `return Err(..)` straight out
    // of `run`, skipping every enclosing `try`. Using it at each `Result`-typed
    // site is what lets the inlined numeric ops, the comparisons and the
    // builtin call all be catchable without a second error channel.
    macro_rules! value {
        ($e:expr) => {
            match $e {
                Ok(v) => v,
                Err(e) => raise!(e),
            }
        };
    }

    loop {
        // Advance the instruction pointer and charge a step. A single
        // `last_mut()` borrows the frame for the whole prologue (bounds check,
        // step charge, instruction load, ip increment) — no repeated pointer
        // chases on the hot path.
        let frame = frames.last_mut().unwrap();
        if frame.ip >= frame.code.body.len() {
            // Top-level frame exhausted: the run is done.
            break;
        }
        steps += 1;
        if steps > MAX_STEPS {
            raise!(Error::runtime(format!(
                "step limit exceeded (max {MAX_STEPS} evaluation steps) — likely an infinite loop or runaway recursion"
            )));
        }
        // The index of the instruction about to run, captured as a plain
        // `usize` so the error paths below can look up its span without
        // capturing (and so conflicting with) the loop's `&mut Frame` borrow.
        let ip = frame.ip;
        let instr = frame.code.body[ip];
        // The span of the node this instruction came from is deliberately NOT
        // read here. Reading `spans[ip]` on every instruction cost ~2.5x of the
        // VM's throughput (the 40k benchmark's speedup gate went 8.5x -> 3.0x)
        // because it breaks the single-borrow prologue the hot loop is built
        // around. The error paths below call `span_at(frame.ip - 1)` instead —
        // the same value, fetched only when something has already gone wrong.
        frame.ip += 1;

        match instr {
            Instr::LoadConst(i) => {
                let frame = &frames.last().unwrap();
                stack.push(frame.code.consts[i].clone());
            }
            Instr::Nil => stack.push(Value::Nil),
            Instr::Bool(b) => stack.push(Value::Bool(b)),

            Instr::LoadSlot(s) => {
                let frame = &frames.last().unwrap();
                // If the slot is already bound, use it. Otherwise fall through
                // to the lexical env — this mirrors tree-walk, where a `def`'s
                // value is evaluated in the enclosing env before the local
                // binding exists (so `(def counter (+ counter 1))` in a fn body
                // reads the *outer* `counter`). The name is resolved only on
                // the fall-through path (a `&String`, no clone), keeping the
                // hot loop allocation-free.
                let v = match frame.locals.get(s).and_then(small_int_or_clone) {
                    Some(v) => v,
                    None => {
                        let name = frame.code.locals[s].clone();
                        match frame.env.get(&name) {
                            Some(v) => v,
                            None => {
                                // Same rule as `Instr::Load`: the suggestion
                                // comes from the compile-time table, because
                                // the frame's slot names are known here while
                                // the env may not hold them yet.
                                let mut e = Error::runtime_at(
                                    format!("unbound symbol '{name}'"),
                                    cur_span(&frames, ip),
                                );
                                if let Some(m) = frame.code.slot_suggestions.get(&s) {
                                    e = e.with_suggestion(m.clone());
                                }
                                raise!(e);
                            }
                        }
                    }
                };
                stack.push(v);
            }
            Instr::DefSlot(s) => {
                let v = stack.pop().unwrap();
                let frame = frames.last_mut().unwrap();
                // Only clone when the value genuinely has to live in two places
                // (the slot *and* the env). With no active env the slot takes
                // ownership, so the clone is pure cost — and for an int it is a
                // variant branch to decide whether an `Rc` refcount needs
                // bumping, which is the hot loop's third such branch. This is
                // the `def` in `(def i (+ i 1))`.
                let env_active = frame.code.env_active;
                if env_active {
                    frame.locals.get_mut(s).unwrap().replace(v.clone());
                    let name = frame.code.locals[s].clone();
                    frame.env.define(name, v);
                } else {
                    frame.locals.get_mut(s).unwrap().replace(v);
                }
                // Push the name symbol back (matches `sf_def`'s return value).
                // `slot_syms[s]` is precomputed, so this is an `Rc` refcount
                // bump — no heap allocation in the hot loop.
                stack.push(frame.code.slot_syms[s].clone());
            }

            Instr::Load(i) => {
                let frame = &frames.last().unwrap();
                let name = frame.code.names[i].clone();
                let found = frame.env.get(&name);
                let v = match found {
                    Some(v) => v,
                    None => {
                        // The suggestion was resolved at *compile* time (see
                        // `emit_load_var`), when the compiler knew every name in
                        // scope — including `def`s that had not executed yet. The
                        // runtime env at this point holds fewer names than the
                        // program will eventually have, so it cannot re-derive
                        // the same answer, and doing so would make the VM disagree
                        // with the tree-walk (4-backend rule).
                        let mut e = Error::runtime_at(
                            format!("unbound symbol '{name}'"),
                            cur_span(&frames, ip),
                        );
                        if let Some(m) = frame.code.name_suggestions.get(&i) {
                            e = e.with_suggestion(m.clone());
                        }
                        raise!(e)
                    }
                };
                stack.push(v);
            }
            Instr::Def(i) => {
                let v = stack.pop().unwrap();
                let frame = frames.last_mut().unwrap();
                let name = frame.code.names[i].clone();
                frame.env.define(&name, v);
                stack.push(Value::Sym(Rc::new(name)));
            }

            // ---- inlined numeric ops (same helpers as the tree-walk) ----
            Instr::Add => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                // Integer fast path: exact add that stays in the inline i64 form
                // (allocation-free) while it fits, and widens to a bignum only on
                // overflow. Identical to `numeric_fold` for two args, minus the
                // slice + closure call overhead (the benchmark's hot op).
                match (&a, &b) {
                    (Value::Int(BigNum::Small(x)), Value::Int(BigNum::Small(y))) => {
                        match x.checked_add(*y) {
                            Some(r) => stack.push(Value::Int(BigNum::Small(r))),
                            None => stack.push(Value::Int(bignum::add_i64(*x, *y))),
                        }
                    }
                    (Value::Int(x), Value::Int(y)) => stack.push(Value::Int(x.add(y))),
                    _ => stack.push(value!(numeric_fold(
                        &[a, b],
                        0.0,
                        0,
                        |x, y| x + y,
                        BigNum::add
                    )
                    .map_err(|e| e.or_at(cur_span(&frames, ip))))),
                }
            }
            Instr::Mul => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                // Integer fast path: exact product, in-range case kept at a bare
                // `i64` like Add, widening to a bignum only on overflow.
                match (&a, &b) {
                    (Value::Int(BigNum::Small(x)), Value::Int(BigNum::Small(y))) => {
                        match x.checked_mul(*y) {
                            Some(r) => stack.push(Value::Int(BigNum::Small(r))),
                            None => stack.push(Value::Int(bignum::mul_i64(*x, *y))),
                        }
                    }
                    (Value::Int(x), Value::Int(y)) => stack.push(Value::Int(x.mul(y))),
                    _ => stack.push(value!(numeric_fold(
                        &[a, b],
                        1.0,
                        1,
                        |x, y| x * y,
                        BigNum::mul
                    )
                    .map_err(|e| e.or_at(cur_span(&frames, ip))))),
                }
            }
            Instr::Sub => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(value!(
                    builtin_sub(&[a, b]).map_err(|e| e.or_at(cur_span(&frames, ip)))
                ));
            }
            Instr::Div => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(value!(
                    builtin_div(&[a, b]).map_err(|e| e.or_at(cur_span(&frames, ip)))
                ));
            }
            Instr::Mod => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(value!(
                    builtin_mod(&[a, b]).map_err(|e| e.or_at(cur_span(&frames, ip)))
                ));
            }
            Instr::Neg => {
                let a = stack.pop().unwrap();
                stack.push(value!(
                    builtin_sub(&[a]).map_err(|e| e.or_at(cur_span(&frames, ip)))
                ));
            }
            Instr::Recip => {
                let a = stack.pop().unwrap();
                stack.push(value!(
                    builtin_div(&[a]).map_err(|e| e.or_at(cur_span(&frames, ip)))
                ));
            }

            // ---- inlined comparisons ----
            Instr::CmpEq => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(Value::Bool(a == b));
            }
            Instr::CmpLt => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                // Integer fast path: exact comparison (not via f64, which
                // would collapse two large distinct ints), inlined to skip the
                // slice + helper call (the benchmark's hot op). The
                // Small/Small case is compared as bare `i64`s so no `BigNum`
                // is dereferenced on the hot path.
                match (&a, &b) {
                    (Value::Int(BigNum::Small(x)), Value::Int(BigNum::Small(y))) => {
                        stack.push(Value::Bool(x < y))
                    }
                    (Value::Int(x), Value::Int(y)) => {
                        stack.push(Value::Bool(x.cmp(y) == Ordering::Less))
                    }
                    _ => stack.push(value!(compare(&[a, b], |o| o == Ordering::Less))),
                }
            }
            Instr::CmpGt => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(value!(compare(&[a, b], |o| o == Ordering::Greater)));
            }
            Instr::CmpLe => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(value!(compare(&[a, b], |o| o != Ordering::Greater)));
            }
            Instr::CmpGe => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(value!(compare(&[a, b], |o| o != Ordering::Less)));
            }
            Instr::Not => {
                let a = stack.pop().unwrap();
                stack.push(Value::Bool(!a.is_truthy()));
            }

            // ---- control flow ----
            Instr::Jump(t) => {
                frames.last_mut().unwrap().ip = t;
            }
            Instr::JumpIfFalse(t) => {
                let v = stack.pop().unwrap();
                if !v.is_truthy() {
                    frames.last_mut().unwrap().ip = t;
                }
            }
            Instr::JumpIfTrue(t) => {
                let v = stack.pop().unwrap();
                if v.is_truthy() {
                    frames.last_mut().unwrap().ip = t;
                }
            }
            Instr::PushTryHandler {
                handler_pc,
                handler_fn,
            } => {
                // The body is a containment boundary, so it gets its own step
                // budget: an infinite loop inside one is bounded on its own
                // rather than consuming the enclosing run's. Recorded in the
                // marker so the unwind path can hand the budget back.
                handlers.push(TryHandler {
                    handler_pc,
                    handler_fn,
                    stack_len: stack.len(),
                    frame_depth: frames.len(),
                    steps,
                });
                steps = 0;
            }
            Instr::PopTryHandler => {
                // The protected region completed: drop the marker and give the
                // enclosing run the steps the body actually used. Restoring the
                // *whole* count rather than adding it back keeps a `try` inside
                // a long-running loop from starving the run that contains it.
                if let Some(h) = handlers.pop() {
                    steps = h.steps + steps.min(MAX_STEPS);
                }
            }

            // ---- functions ----
            Instr::MakeFn(i) => {
                let frame = &frames.last().unwrap();
                let fnc = &frame.code.fns[i];
                let clos = Closure {
                    params: fnc.params.clone(),
                    variadic: fnc.variadic.clone(),
                    body: Vec::new(), // the VM runs `code`, not `body`
                    env: frame.env.clone(),
                    code: Some(Rc::clone(fnc)),
                };
                stack.push(Value::Closure(Rc::new(clos)));
            }
            Instr::Call(n) => {
                let mut args = Vec::with_capacity(n);
                for _ in 0..n {
                    args.push(stack.pop().unwrap());
                }
                args.reverse();
                let callee = stack.pop().unwrap();
                // The call's instruction index, for the *error* paths below to
                // look up the callee's source name recorded at compile time.
                // `ip` was already advanced past this `Call`, so it is the
                // index. Nothing is resolved here: a HashMap lookup and a
                // String clone per call would tax the hot loop for a
                // diagnostic that almost never happens.
                let call_ip = frame.ip - 1;
                match callee {
                    Value::Builtin { f, .. } => {
                        let r = value!(f(&args).map_err(|e| e.or_at(cur_span(&frames, ip))));
                        stack.push(r);
                    }
                    Value::Closure(c) => {
                        let fnc = match c.code.as_ref() {
                            Some(f) => f,
                            None => raise!(Error::runtime(
                                "cannot call a tree-walk closure from the VM"
                            )),
                        };
                        let np = fnc.params.len();
                        if fnc.variadic.is_some() {
                            if args.len() < np {
                                let who = self_callee_name(&frames, call_ip);
                                raise!(Error::runtime_at(
                                    format!(
                                        "arity mismatch: ({who}) takes at least {np} args, got {}",
                                        args.len()
                                    ),
                                    cur_span(&frames, ip),
                                ));
                            }
                        } else if args.len() != np {
                            let who = self_callee_name(&frames, call_ip);
                            raise!(crate::eval::arity_mismatch(
                                &who,
                                np,
                                args.len(),
                                cur_span(&frames, ip),
                            ));
                        }
                        if frames.len() > MAX_DEPTH {
                            raise!(Error::runtime(format!(
                                "recursion limit exceeded (max depth {MAX_DEPTH})"
                            )));
                        }
                        let call_env = c.env.child();
                        let locals = bind_params(fnc, &args, &call_env);
                        frames.push(Frame {
                            code: Rc::clone(fnc),
                            ip: 0,
                            locals,
                            env: call_env,
                        });
                    }
                    other => {
                        raise!(crate::eval::not_callable(
                            other.type_name(),
                            cur_span(&frames, ip),
                        ));
                    }
                }
            }
            Instr::Ret => {
                let result = stack.pop().unwrap();
                let frame = frames.pop().unwrap();
                // Break a self-referential-def cycle rooted in this call's own
                // scope, unless the result still needs it (a closure escaped
                // that was defined in — or under — the call env).
                if !value_keeps_env_alive(&result, &frame.env) {
                    frame.env.clear();
                }
                stack.push(result);
            }

            // ---- stack plumbing ----
            Instr::Pop => {
                stack.pop();
            }
            Instr::Dup => {
                let v = stack.last().unwrap().clone();
                stack.push(v);
            }
            Instr::Swap => {
                let l = stack.len();
                stack.swap(l - 1, l - 2);
            }
        }
    }

    // Persist top-level `def`s into the global env so they survive across
    // `run_in` calls (REPL contract). Slots already synced during the run
    // (env_active) are re-written with the same value — a harmless no-op.
    //
    // The frame that ran is `frames.last()` — it is the one on top. With a
    // sentinel the *first* frame is the empty base, so index from the end.
    let top_frame = frames.last().expect("a frame always exists here");
    for (name, slot) in top_frame.code.locals.iter().zip(top_frame.locals.iter()) {
        if let Some(v) = slot {
            env.define(name, v.clone());
        }
    }

    Ok(stack.pop().unwrap_or(Value::Nil))
}

/// Call a VM-compiled closure from outside the run loop, with `args`.
///
/// This is the seam that lets a **builtin** call a function value — which is
/// what `sort`'s comparator form needs. A builtin is a `fn(&[Value]) -> …` with
/// no access to the running frame stack, so it cannot push a frame itself; this
/// runs the closure's bytecode in a **nested** [`run`] instead.
///
/// The nested run is a real cost, and it is paid only on the comparator path:
/// a program that never passes a function to a builtin never calls this, and
/// `sort`'s no-comparator form never does either. The alternative — a `Call`
/// instruction that the builtin could inject — would mean giving builtins a
/// handle on the frame stack, which is the thing `BuiltinFn`'s bare-fn-pointer
/// type exists to prevent.
///
/// The step budget is fresh, exactly as it is per `run_in` call, so a
/// comparator cannot exhaust the enclosing program's budget (or vice versa) and
/// report a misleading step-limit error. A `try` in the enclosing frame is not
/// visible from in here: an error propagates as `Err` to the builtin, which
/// returns it, and the enclosing `Call` hands it to the enclosing `raise!`.
/// That is the same unwinding the tree-walk gets for free.
pub fn call_closure(callee: Value, args: &[Value]) -> Result<Value> {
    let Value::Closure(c) = callee else {
        return Err(crate::eval::not_callable(callee.type_name(), 0));
    };
    let Some(fnc) = c.code.as_ref() else {
        return Err(Error::runtime(
            "cannot call a tree-walk closure from the VM",
        ));
    };
    let np = fnc.params.len();
    if fnc.variadic.is_some() {
        if args.len() < np {
            return Err(Error::runtime(format!(
                "arity mismatch: takes at least {np} args, got {}",
                args.len()
            )));
        }
    } else if args.len() != np {
        return Err(crate::eval::arity_mismatch("anonymous", np, args.len(), 0));
    }
    let call_env = c.env.child();
    let locals = bind_params(fnc, args, &call_env);
    // `sentinel = true`: the frame being run is a *function*, so its `Ret` pops
    // itself and needs a frame underneath to land on. And caller-supplied
    // locals: the parameters were bound positionally above, and `run` would
    // re-resolve them by name in the fresh call env — where they are not
    // defined — so every parameter would read as unbound (`unbound symbol 'a'`
    // inside a `sort` comparator).
    run_with_locals(fnc, &call_env, locals, true)
}

/// Parse + compile + run a program in a fresh preloaded environment.
pub fn run_str(src: &str) -> Result<Value> {
    let env = Env::with_prelude();
    run_in(src, &env)
}

/// Parse + compile + run a program in an existing environment (REPL). Each
/// call gets a fresh step budget (the VM's step counter is local to `run`).
pub fn run_in(src: &str, env: &Env) -> Result<Value> {
    let forms = crate::parse(src)?;
    run_forms(&forms, env).map_err(|e| e.located_in(src))
}

/// Compile + run already-parsed forms in `env`, returning the last form's
/// value. The form-list counterpart of [`run_in`], which the module loader
/// needs: it has the AST in hand and must not re-parse text it already holds.
pub fn run_forms(forms: &[Node], env: &Env) -> Result<Value> {
    let code = compile_top(forms)?;
    run(&code, env)
}

/// Compile and run a *single* already-parsed top-level form in `env`.
///
/// The REPL needs this to submit one form at a time: a submission that is
/// several top-level forms must not lose the `def`s its earlier forms made
/// just because a later one failed. Evaluating form-by-form commits each form
/// to `env` as it completes, so `(def a 1) (nosuch)` still leaves `a` bound —
/// which is also what the tree-walking evaluator does, and therefore a case the
/// two paths must agree on.
///
/// Each call gets a fresh step budget, same as [`run_in`].
pub fn run_form(form: &Node, env: &Env) -> Result<Value> {
    let code = compile_top(std::slice::from_ref(form))?;
    run(&code, env)
}

/// [`run_form`], but the error is located against `src` and the suggestion
/// candidates are seeded from `env`.
///
/// The REPL holds the whole submission text while it evaluates the forms one at
/// a time, so it can hand the source down and get a real `at line N, col M`
/// rather than a bare byte offset.
///
/// Seeding matters for the same reason: the REPL compiles each submission
/// alone, so a name bound on an *earlier* line is in `env` but in none of the
/// forms being compiled. Without this, a typo of an earlier binding would get
/// no suggestion in the REPL while a whole-file run would suggest it.
pub fn run_form_in(form: &Node, env: &Env, src: &str) -> Result<Value> {
    let code = compile_top_seeded(std::slice::from_ref(form), env.all_names())?;
    run(&code, env).map_err(|e| e.located_in(src))
}
