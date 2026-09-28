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

use crate::code::{FnCode, Instr};
use crate::error::{Error, Result};
use crate::eval::{
    builtin_div, builtin_mod, builtin_sub, compare, numeric_fold, value_keeps_env_alive, Env,
    MAX_DEPTH, MAX_STEPS,
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
}

impl Compiler {
    fn new_top() -> Self {
        Compiler {
            code: FnCode::new("<top>"),
            scope: Scope::default(),
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
        }
    }

    fn body_len(&self) -> usize {
        self.code.body.len()
    }

    fn emit(&mut self, instr: Instr) {
        self.code.body.push(instr);
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
                self.emit(Instr::Load(i));
            }
        }
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
    fn compile_expr(&mut self, node: &Node) -> Result<()> {
        match node {
            Node::Int(i, _) => self.push_const(Value::Int(*i)),
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
            match op.as_str() {
                "def" => return self.sf_def(&items[1..]),
                "fn" => return self.sf_fn(&items[1..]),
                "if" => return self.sf_if(&items[1..]),
                "do" => return self.sf_do(&items[1..]),
                "let" => return self.sf_let(&items[1..]),
                "while" => return self.sf_while(&items[1..]),
                "quote" => return self.sf_quote(&items[1..]),
                "and" => return self.sf_and(&items[1..]),
                "or" => return self.sf_or(&items[1..]),
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
                self.patch_jump(jump, self.body_len());
                self.emit(Instr::Nil);
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
                "fn" | "let" => continue, // new scope
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
    let mut c = Compiler::new_top();
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
        Node::Int(i, _) => Value::Int(*i),
        Node::Float(x, _) => Value::Float(*x),
        Node::Str(s, _) => Value::str(s.clone()),
        Node::Sym(s, _) => Value::Sym(Rc::new(s.clone())),
        Node::List(items, _) => Value::List(ConsCell::from_values(items.iter().map(quote_node))),
    }
}

// ---------------------------------------------------------------------------
// Stack machine
// ---------------------------------------------------------------------------

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

/// Execute a compiled top-level program in `env`. Returns the value of the
/// final form. `env` is the global (REPL) environment: top-level `def`s are
/// persisted into it so bindings survive across `run_in` calls.
pub fn run(top: &FnCode, env: &Env) -> Result<Value> {
    let mut stack: Vec<Value> = Vec::new();
    let mut steps: u64 = 0;

    // Top-level frame: pre-fill local slots from the global env so a var
    // defined in an earlier run is visible before it is re-`def`d.
    let mut top_locals: Vec<Option<Value>> = Vec::with_capacity(top.locals.len());
    for name in &top.locals {
        top_locals.push(env.get(name));
    }
    let mut frames: Vec<Frame> = vec![Frame {
        code: Rc::new(top.clone()),
        ip: 0,
        locals: top_locals,
        env: env.clone(),
    }];

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
            return Err(Error::runtime(format!(
                "step limit exceeded (max {MAX_STEPS} evaluation steps) — likely an infinite loop or runaway recursion"
            )));
        }
        let instr = frame.code.body[frame.ip];
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
                let v = match frame.locals.get(s).and_then(|o| o.clone()) {
                    Some(v) => v,
                    None => {
                        let name = &frame.code.locals[s];
                        match frame.env.get(name) {
                            Some(v) => v,
                            None => return Err(Error::runtime(format!("unbound symbol '{name}'"))),
                        }
                    }
                };
                stack.push(v);
            }
            Instr::DefSlot(s) => {
                let v = stack.pop().unwrap();
                let frame = frames.last_mut().unwrap();
                frame.locals.get_mut(s).unwrap().replace(v.clone());
                if frame.code.env_active {
                    let name = frame.code.locals[s].clone();
                    frame.env.define(name, v);
                }
                // Push the name symbol back (matches `sf_def`'s return value).
                // `slot_syms[s]` is precomputed, so this is an `Rc` refcount
                // bump — no heap allocation in the hot loop.
                stack.push(frame.code.slot_syms[s].clone());
            }

            Instr::Load(i) => {
                let frame = &frames.last().unwrap();
                let name = frame.code.names[i].clone();
                let v = frame
                    .env
                    .get(&name)
                    .ok_or_else(|| Error::runtime(format!("unbound symbol '{name}'")))?;
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
                // Integer fast path: stay in i64 until overflow, then promote
                // to f64 — identical to `numeric_fold` for two args, minus the
                // slice + closure call overhead (the benchmark's hot op).
                match (&a, &b) {
                    (Value::Int(x), Value::Int(y)) => match x.checked_add(*y) {
                        Some(r) => stack.push(Value::Int(r)),
                        None => stack.push(Value::Float((*x as f64) + (*y as f64))),
                    },
                    _ => stack.push(numeric_fold(
                        &[a, b],
                        0.0,
                        0,
                        |x, y| x + y,
                        i64::checked_add,
                    )?),
                }
            }
            Instr::Mul => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(numeric_fold(
                    &[a, b],
                    1.0,
                    1,
                    |x, y| x * y,
                    i64::checked_mul,
                )?);
            }
            Instr::Sub => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(builtin_sub(&[a, b])?);
            }
            Instr::Div => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(builtin_div(&[a, b])?);
            }
            Instr::Mod => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(builtin_mod(&[a, b])?);
            }
            Instr::Neg => {
                let a = stack.pop().unwrap();
                stack.push(builtin_sub(&[a])?);
            }
            Instr::Recip => {
                let a = stack.pop().unwrap();
                stack.push(builtin_div(&[a])?);
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
                // Integer fast path: same f64 comparison the tree-walk's
                // `compare` does (via `as_f64`), inlined to skip the slice +
                // helper call (the benchmark's hot op).
                match (&a, &b) {
                    (Value::Int(x), Value::Int(y)) => {
                        stack.push(Value::Bool((*x as f64) < (*y as f64)))
                    }
                    _ => stack.push(compare(&[a, b], |o| o == Ordering::Less)?),
                }
            }
            Instr::CmpGt => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(compare(&[a, b], |o| o == Ordering::Greater)?);
            }
            Instr::CmpLe => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(compare(&[a, b], |o| o != Ordering::Greater)?);
            }
            Instr::CmpGe => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(compare(&[a, b], |o| o != Ordering::Less)?);
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
                match callee {
                    Value::Builtin { f, .. } => {
                        let r = f(&args)?;
                        stack.push(r);
                    }
                    Value::Closure(c) => {
                        let fnc = c.code.as_ref().ok_or_else(|| {
                            Error::runtime("cannot call a tree-walk closure from the VM")
                        })?;
                        let np = fnc.params.len();
                        if fnc.variadic.is_some() {
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
                        if frames.len() > MAX_DEPTH {
                            return Err(Error::runtime(format!(
                                "recursion limit exceeded (max depth {MAX_DEPTH})"
                            )));
                        }
                        let call_env = c.env.child();
                        let n_body = fnc.locals.len() - np - usize::from(fnc.variadic.is_some());
                        let mut locals = Vec::with_capacity(fnc.locals.len());
                        for (pname, val) in fnc.params.iter().zip(args.iter()) {
                            locals.push(Some(val.clone()));
                            if fnc.env_active {
                                call_env.define(pname.clone(), val.clone());
                            }
                        }
                        if let Some(rest) = &fnc.variadic {
                            let extra: Vec<Value> = args[np..].to_vec();
                            let rest_val = Value::List(ConsCell::from_values(extra));
                            locals.push(Some(rest_val.clone()));
                            if fnc.env_active {
                                call_env.define(rest, rest_val);
                            }
                        }
                        for _ in 0..n_body {
                            locals.push(None);
                        }
                        frames.push(Frame {
                            code: Rc::clone(fnc),
                            ip: 0,
                            locals,
                            env: call_env,
                        });
                    }
                    other => {
                        return Err(Error::runtime(format!(
                            "cannot call a {}",
                            other.type_name()
                        )))
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
    let top_frame = &frames[0];
    for (name, slot) in top_frame.code.locals.iter().zip(top_frame.locals.iter()) {
        if let Some(v) = slot {
            env.define(name, v.clone());
        }
    }

    Ok(stack.pop().unwrap_or(Value::Nil))
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
    let code = compile_top(&forms)?;
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
