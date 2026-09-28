//! Bytecode for the AINL stack machine.
//!
//! The compiler (`vm::compile`) lowers the AST (`Node`) into a flat
//! `Vec<Instr>` plus per-function constant / name / local pools. The VM
//! (`vm::run`) executes the code on an operand stack with an explicit frame
//! stack, so recursion is a data operation rather than native call-stack
//! recursion — a core part of the speedup over the tree-walk.
//!
//! Each [`FnCode`] is self-contained: it carries its own constant pool, name
//! pool, nested-function table, and local-slot table. That makes a compiled
//! closure portable across `run_in` calls (a closure created in one run can be
//! invoked in a later run), which the `LIVE_SCOPES` escaping-closure test
//! requires.
//!
//! Variable access has two paths:
//! - **Local slots** (`LoadSlot`/`DefSlot`): hot-loop variables are resolved to
//!   fixed slot indices (a `Vec<Value>` in the frame), so the common case is an
//!   O(1) array access with no `HashMap`. `DefSlot` also writes the value into
//!   the frame's [`Env`], keeping the lexical scope in sync so closures can
//!   capture it and the `LIVE_SCOPES` cycle-breaking works.
//! - **Env lookups** (`Load`/`Def`): non-local variables go through the
//!   existing tree-walk [`Env`] (the same `HashMap`-backed lexical scope the
//!   tree-walking evaluator uses).
//!
//! The common builtins (`+ - * / mod = < > <= >= not`) are inlined as dedicated
//! instructions that call the *same* helper functions the tree-walk's builtins
//! use, so their semantics are identical by construction and the hot loop avoids
//! the head-eval + arg-`Vec` + `apply` dispatch the tree-walk pays per call.
//!
//! This is the fast execution path. The tree-walking evaluator in `eval.rs` is
//! retained as a fallback and as the semantic reference the VM is checked
//! against (see `run_in_tree_walk`).

use crate::parser::Span;
use crate::value::Value;
use std::rc::Rc;

/// A single bytecode instruction. Operands are indices into the owning
/// [`FnCode`]'s pools (constants, names, locals, nested functions) or bytecode
/// offsets (jump targets).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Instr {
    // --- constants & literals ---
    /// Push `consts[idx]` onto the stack (cloned).
    LoadConst(usize),
    /// Push `nil`.
    Nil,
    /// Push the boolean `b`.
    Bool(bool),

    // --- local-slot access (O(1) array, no HashMap) ---
    /// Push the value in local slot `slot` (cloned). Errors if unbound,
    /// matching the tree-walk's `unbound symbol` error.
    LoadSlot(usize),
    /// Pop a value, store it in local slot `slot` *and* bind `locals[slot]` in
    /// the frame's `Env`, then push the symbol `locals[slot]` — matching
    /// `sf_def`'s return value.
    DefSlot(usize),

    // --- env access (non-local variables) ---
    /// Push the value bound to `names[idx]` in the frame's `Env` (cloned).
    /// Errors if unbound, matching the tree-walk's `unbound symbol` error.
    Load(usize),
    /// Pop a value, bind it to `names[idx]` in the frame's `Env`, then push the
    /// symbol `names[idx]` — matching `sf_def`'s return value.
    Def(usize),

    // --- inlined numeric ops ---
    /// Pop 2 (a, b), push `a + b`.
    Add,
    /// Pop 2 (a, b), push `a - b`.
    Sub,
    /// Pop 2 (a, b), push `a * b`.
    Mul,
    /// Pop 2 (a, b), push `a / b` (float).
    Div,
    /// Pop 2 (a, b), push `a mod b` (int).
    Mod,
    /// Pop 1 (a), push `-a`.
    Neg,
    /// Pop 1 (a), push `1 / a` (float).
    Recip,

    // --- inlined comparisons (pop 2, push bool) ---
    CmpLt,
    CmpGt,
    CmpLe,
    CmpGe,
    CmpEq,
    /// Pop 1, push `!truthy`.
    Not,

    // --- control flow ---
    /// Unconditional jump to bytecode offset `target`.
    Jump(usize),
    /// Pop a value; if it is falsey, jump to `target`.
    JumpIfFalse(usize),
    /// Pop a value; if it is truthy, jump to `target`.
    JumpIfTrue(usize),
    /// Enter a protected region: push a [`crate::vm::TryHandler`] recording
    /// the enclosing frame, the operand-stack depth, the step count, and where
    /// to resume — `handler_pc` (the bytecode offset of this `try`'s `catch`
    /// code) plus `handler_fn` (the index of that handler's `FnCode`).
    ///
    /// The handler function is recorded rather than re-`MakeFn`'d on the way in
    /// because the unwind path has to push the callee *and* its argument onto
    /// the operand stack itself, in the order `Call` expects them.
    PushTryHandler {
        /// Bytecode offset to jump to when a frame inside the region raises.
        handler_pc: usize,
        /// Index into the enclosing frame's `fns` of the `catch` closure.
        handler_fn: usize,
    },
    /// Leave a protected region that completed without raising: pop its
    /// handler and restore the step budget to what it was on entry.
    PopTryHandler,

    // --- functions ---
    /// Push a closure for `fns[idx]`, capturing the current frame's env.
    MakeFn(usize),
    /// Pop `n` args (top = last) and the callee (below them); push the result.
    /// Dispatches to builtins (function pointer) or closures (new frame).
    Call(usize),
    /// Pop the frame's result; return it to the caller (or end the run).
    Ret,

    // --- stack plumbing ---
    /// Pop and discard the top value.
    Pop,
    /// Duplicate the top value.
    Dup,
    /// Swap the top two values.
    Swap,
}

/// A compiled function (or the top-level program). Self-contained: carries its
/// own pools so a closure can be invoked in a later `run_in` call.
#[derive(Debug, Clone)]
pub struct FnCode {
    /// Display name for diagnostics (`<top>` for the program, the param list
    /// for `fn`, `<let>` for let-scopes).
    pub name: String,
    /// Parameter names, in order. These occupy local slots `0..params.len()`.
    pub params: Vec<String>,
    /// Optional rest-parameter name introduced by `&`.
    pub variadic: Option<String>,
    /// The compiled body.
    pub body: Vec<Instr>,
    /// Constant pool (literals: ints, floats, strings, quoted forms, …).
    pub consts: Vec<Value>,
    /// Name pool for non-local (`Load`/`Def`) variable references.
    pub names: Vec<String>,
    /// Nested function bodies, indexed by `MakeFn`'s operand.
    pub fns: Vec<Rc<FnCode>>,
    /// Local-slot table: `slot` -> variable name. Slots `0..params.len()` are
    /// the params; the rest are variables `def`-bound in the body.
    pub locals: Vec<String>,
    /// Precomputed name symbol for each local slot (`Value::Sym`). `DefSlot`
    /// pushes a clone of this (an `Rc` refcount bump, no heap allocation) to
    /// match `sf_def`'s return value — precomputing it keeps the hot loop
    /// allocation-free.
    pub slot_syms: Vec<Value>,
    /// True if this scope (or a descendant scope) contains a closure. When
    /// true, `DefSlot`/param-binding sync the value into the frame's `Env` so
    /// closures can look it up and the `LIVE_SCOPES` cycle-breaking works. When
    /// false (no closures in the subtree), the hot loop skips all env updates.
    pub env_active: bool,
    /// Source span for each instruction in `body`, parallel to it: entry `i` is
    /// the span of the AST node that `body[i]` was compiled from.
    ///
    /// This is what lets the VM report *where* a failure happened, matching the
    /// tree-walk's `at line N, col M`. Without it the VM could only say what
    /// went wrong, and the two backends would disagree on stderr — which the
    /// 4-backend rule forbids.
    ///
    /// Parallel rather than embedded in [`Instr`] for one reason: `Instr` is
    /// `Copy` and 8 bytes wide, and it is matched in the hot loop. Widening it
    /// to carry a `Span` would grow every push/pop of the operand stack's
    /// instruction stream for a diagnostic only. A side table costs one
    /// `Span` per instruction, allocated at compile time only.
    ///
    /// **Read it on the error path only.** Indexing this table per instruction
    /// in the run loop cost about 2.5x of the VM's throughput — the 40k
    /// benchmark's speedup gate fell from 8.5x to 3.0x — because it breaks the
    /// single-borrow prologue the hot loop is built around. `vm::cur_span` is
    /// the only sanctioned reader.
    ///
    /// Always the same length as `body`; [`FnCode::push`] keeps them in step.
    pub spans: Vec<Span>,
    /// Source name of each call's callee, keyed by the `Call` instruction's
    /// index in `body`.
    ///
    /// The VM pushes a *value* at the callee, so by the time `Call` runs, the
    /// symbol the reader wrote is gone — and without it an arity error can only
    /// say "fn expects 2 args", not which of the program's functions was
    /// called wrongly. The tree-walk still has the AST and can say it. This
    /// table is what makes the two agree (the 4-backend rule).
    ///
    /// Sparse (`HashMap`, not a parallel `Vec`) and read **only on the error
    /// path**: most programs have few call sites relative to instruction
    /// count, and the hot loop must not pay for a name lookup per call.
    pub call_names: std::collections::HashMap<usize, String>,
    /// Close-match suggestion for each entry in `names`, for the same reason as
    /// `call_names` above: the *runtime* environment is missing names the
    /// *compiler* could see (a `def` further down the file has not run yet), so
    /// only the compiler can produce the same answer the tree-walk produces.
    ///
    /// Sparse: only entries that actually have a suggestion are present.
    pub name_suggestions: std::collections::HashMap<usize, String>,
    /// The same, for `locals` (the local-slot table), keyed by slot.
    pub slot_suggestions: std::collections::HashMap<usize, String>,
}

/// A compiled program is just the top-level [`FnCode`].
pub type Code = FnCode;

impl FnCode {
    pub fn new(name: &str) -> Self {
        FnCode {
            name: name.to_string(),
            params: Vec::new(),
            variadic: None,
            body: Vec::new(),
            consts: Vec::new(),
            names: Vec::new(),
            fns: Vec::new(),
            locals: Vec::new(),
            slot_syms: Vec::new(),
            env_active: false,
            spans: Vec::new(),
            call_names: std::collections::HashMap::new(),
            name_suggestions: std::collections::HashMap::new(),
            slot_suggestions: std::collections::HashMap::new(),
        }
    }

    /// Append an instruction together with the span of the node it came from.
    ///
    /// The only way to add to `body` — keeping `spans` the same length is the
    /// invariant the VM's error path relies on.
    pub fn push(&mut self, instr: Instr, span: Span) {
        self.body.push(instr);
        self.spans.push(span);
    }

    /// The source span of instruction `ip`, or `Span::new(0, 0)` if the table
    /// is somehow shorter (defensive: a wrong position is better than a panic
    /// on an error path).
    pub fn span_at(&self, ip: usize) -> Span {
        self.spans.get(ip).copied().unwrap_or(Span::new(0, 0))
    }

    /// Rebuild the precomputed slot name symbols from `locals`. Called after
    /// `locals` is finalized (at code emission) so `DefSlot` can push a name
    /// symbol without a heap allocation in the hot loop.
    pub fn sync_slot_syms(&mut self) {
        self.slot_syms = self
            .locals
            .iter()
            .map(|n| Value::Sym(Rc::new(n.clone())))
            .collect();
    }
}
