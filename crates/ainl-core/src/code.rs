//! Bytecode for the AINL stack machine.
//!
//! The compiler (`vm::compile`) lowers the AST (`Node`) into a flat
//! `Vec<Instr>` plus a constant pool, a name pool, and a table of nested
//! function bodies. The VM (`vm::run`) executes the code on an operand stack
//! with an explicit frame stack, so recursion is a data operation rather than
//! native call-stack recursion — the core of the speedup over the tree-walk.
//!
//! Every call (builtin or user function) is compiled to `Load(head)` + args +
//! `Call(n)`, dispatching to the *same* builtin function pointers the
//! tree-walking evaluator uses, so semantics are identical by construction.
//!
//! This is the fast execution path. The tree-walking evaluator in `eval.rs` is
//! retained as a fallback and as the semantic reference the VM is checked
//! against (see `run_in_tree_walk`).

use crate::value::Value;
use std::rc::Rc;

/// A single bytecode instruction. Operands are indices into the `Code`'s
/// constant pool, name pool, function table, or bytecode offsets (jump
/// targets).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Instr {
    // --- constants & literals ---
    /// Push `consts[idx]` onto the stack (cloned).
    LoadConst(usize),
    /// Push `nil`.
    Nil,
    /// Push the boolean `b`.
    Bool(bool),

    // --- variables (lexical scoping via the `Env` parent chain) ---
    /// Push the value bound to `names[idx]` in the current env (cloned).
    /// Errors if unbound, matching the tree-walk's `unbound symbol` error.
    Load(usize),
    /// Pop a value, bind it to `names[idx]` in the current env, then push the
    /// symbol `names[idx]` — matching `sf_def`'s return value (the name).
    Def(usize),

    // --- control flow ---
    /// Unconditional jump to bytecode offset `target`.
    Jump(usize),
    /// Pop a value; if it is falsey, jump to `target`.
    JumpIfFalse(usize),
    /// Pop a value; if it is truthy, jump to `target`.
    JumpIfTrue(usize),

    // --- functions ---
    /// Push a closure for `fns[idx]`, capturing the current env.
    MakeFn(usize),
    /// Pop `n` args (top = last) and the callee (below them); push the result.
    /// Dispatches to builtins (function pointer) or closures (new frame).
    Call(usize),
    /// Pop the frame's result; return it to the caller (or end the run).
    Ret,

    // --- stack plumbing ---
    /// Pop and discard the top value.
    Pop,
    /// Swap the top two values.
    Swap,
    /// Duplicate the top value.
    Dup,
}

/// A named function's compiled body.
#[derive(Debug, Clone)]
pub struct FnCode {
    /// Display name for diagnostics (`<top>` for the program, `<let>` for
    /// let-scopes).
    pub name: String,
    pub params: Vec<String>,
    /// Optional rest-parameter name introduced by `&`.
    pub variadic: Option<String>,
    pub body: Vec<Instr>,
}

/// A compiled program: top-level instructions plus the shared pools.
#[derive(Debug, Clone)]
pub struct Code {
    /// Top-level instructions.
    pub instrs: Vec<Instr>,
    /// Constant pool (literals: ints, floats, strings, quoted forms, …).
    pub consts: Vec<Value>,
    /// Name pool (variable / function names), indexed by `Load`/`Def`.
    pub names: Vec<String>,
    /// Function bodies, indexed by `MakeFn`'s operand.
    pub fns: Vec<Rc<FnCode>>,
}

impl Code {
    pub fn new() -> Self {
        Code {
            instrs: Vec::new(),
            consts: Vec::new(),
            names: Vec::new(),
            fns: Vec::new(),
        }
    }
}
