//! Runtime values.

use crate::bignum::BigNum;
use crate::code::FnCode;
use crate::error::Result;
use crate::eval::Env;
use crate::parser::Node;
use std::fmt;
use std::rc::Rc;

/// A user-defined function (closure) capturing its defining environment.
pub struct Closure {
    pub params: Vec<String>,
    /// Optional rest-parameter name introduced by `&`, e.g. `(fn (a & rest) ...)`.
    pub variadic: Option<String>,
    /// The AST body — used by the tree-walking evaluator.
    pub body: Vec<Node>,
    pub env: Env,
    /// Compiled bytecode for this closure, set by the VM (`MakeFn`). `None` for
    /// closures created by the tree-walk, which interpret `body` instead. A run
    /// is either all-VM or all-tree-walk, so `code` and `body` are never mixed
    /// within a single run.
    pub code: Option<Rc<FnCode>>,
}

/// A native function implemented in Rust.
pub type BuiltinFn = fn(&[Value]) -> Result<Value>;

/// One element of a list: a value plus a pointer to the rest of the list.
///
/// Lists are `Value::List(Rc<ConsCell>)` — a linked list of cons cells, not a
/// `Vec`. `cons`/`first`/`rest` are O(1) pointer ops, so building a list by
/// repeated prepend is O(n) total instead of the O(n²) whole-`Vec` clone the
/// old `Rc<Vec<Value>>` representation forced on every mutation. `push`/`nth`
/// are O(n) (traverse to the end / to index `i`); `len` is O(1) via the
/// cached length. See docs/PERFORMANCE.md ("Root cause of the 3.23s").
///
/// The empty list is a single canonical cell with `head == Nil` and
/// `tail == None`; its `head` is never observed (every accessor treats an
/// empty list as having no first element), so it is safe to reuse one shape
/// for every empty list.
pub struct ConsCell {
    pub head: Value,
    pub tail: Option<Rc<ConsCell>>,
    /// Cached element count, kept in sync by the two constructors below so
    /// `(len ...)` stays O(1).
    pub len: usize,
}

#[derive(Clone)]
pub enum Value {
    Nil,
    Bool(bool),
    Int(BigNum),
    Float(f64),
    Str(Rc<String>),
    /// A quoted symbol (from `(quote x)`), distinct from a variable reference.
    Sym(Rc<String>),
    List(Rc<ConsCell>),
    /// A key-value map. Backed by an ordered association list (not a native
    /// hash table) so lookups use `Value`'s own `PartialEq` — this is what
    /// keeps e.g. a quoted symbol and an equal-content string correctly
    /// distinct as keys, matching every other equality rule in the language,
    /// at the cost of O(n) lookup. See docs/SYNTAX.md §3 "Maps".
    Map(Rc<Vec<(Value, Value)>>),
    Builtin {
        name: &'static str,
        f: BuiltinFn,
    },
    Closure(Rc<Closure>),
}

impl ConsCell {
    /// The canonical empty list.
    pub fn empty() -> Rc<ConsCell> {
        Rc::new(ConsCell {
            head: Value::Nil,
            tail: None,
            len: 0,
        })
    }

    /// `(cons head tail)` — O(1).
    pub fn cons(head: Value, tail: &Rc<ConsCell>) -> Rc<ConsCell> {
        Rc::new(ConsCell {
            head,
            tail: Some(tail.clone()),
            len: tail.len + 1,
        })
    }

    /// Build a list from an iterator, front to back — O(n). Each cell is
    /// allocated exactly once by building the chain from the back.
    pub fn from_values<I: IntoIterator<Item = Value>>(items: I) -> Rc<ConsCell> {
        let items: Vec<Value> = items.into_iter().collect();
        let mut tail: Option<Rc<ConsCell>> = None;
        // Enumerating the reversed items gives each cell's cached length for
        // free: the k-th item from the back heads a list of k elements.
        for (len, v) in items.into_iter().rev().enumerate() {
            tail = Some(Rc::new(ConsCell {
                head: v,
                tail: tail.clone(),
                len: len + 1,
            }));
        }
        match tail {
            Some(c) => c,
            None => Self::empty(),
        }
    }

    /// First element, or `None` for the empty list.
    pub fn first(&self) -> Option<&Value> {
        if self.len == 0 {
            None
        } else {
            Some(&self.head)
        }
    }

    /// Everything after the first element, or `None` for the empty list. O(1).
    pub fn rest(&self) -> Option<&Rc<ConsCell>> {
        self.tail.as_ref()
    }

    /// Element at index `i`, or `None` if out of range. O(n).
    pub fn nth(&self, i: usize) -> Option<&Value> {
        let mut cur = self;
        let mut idx = 0;
        loop {
            if idx == i {
                return cur.first();
            }
            let Some(tail) = &cur.tail else {
                return None;
            };
            cur = tail;
            idx += 1;
        }
    }
}

impl Drop for ConsCell {
    fn drop(&mut self) {
        // Drop the tail chain *iteratively*. The default derived drop would
        // recurse once per cell (cell -> tail Rc -> cell -> ...), and a long
        // list (tens of thousands of cells) overflows the stack of a small
        // thread — e.g. `cargo test` workers — aborting the whole process.
        // The old `Rc<Vec<Value>>` representation dropped iteratively for
        // free; this restores that property for the linked representation.
        //
        // `try_unwrap` succeeds while each cell is solely owned by its
        // predecessor (the common case), letting the loop break the chain
        // without recursion. A cell shared with a live binding (e.g. the
        // result of `rest`) is handed back to the normal drop, which only
        // runs when its last reference goes away — at which point the chain
        // below it is again solely owned and unwrapped iteratively.
        let mut tail = self.tail.take();
        while let Some(t) = tail {
            match Rc::try_unwrap(t) {
                Ok(mut cell) => tail = cell.tail.take(),
                Err(shared) => {
                    drop(shared);
                    break;
                }
            }
        }
    }
}

impl Value {
    pub fn str(s: impl Into<String>) -> Value {
        Value::Str(Rc::new(s.into()))
    }

    /// Truthiness: only `nil` and `false` are falsey. Everything else is true.
    pub fn is_truthy(&self) -> bool {
        !matches!(self, Value::Nil | Value::Bool(false))
    }

    /// Human-facing type name, used in error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Nil => "nil",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Str(_) => "str",
            Value::Sym(_) => "sym",
            Value::List(_) => "list",
            Value::Map(_) => "hash",
            Value::Builtin { .. } => "builtin",
            Value::Closure(_) => "fn",
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Nil, Value::Nil) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Int(a), Value::Float(b)) | (Value::Float(b), Value::Int(a)) => a.to_f64() == *b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Sym(a), Value::Sym(b)) => a == b,
            (Value::List(a), Value::List(b)) => cons_cells_eq(a, b),
            // Order-sensitive, like List — see the Map variant's doc comment.
            (Value::Map(a), Value::Map(b)) => a == b,
            _ => false,
        }
    }
}

/// Structural, order-sensitive equality for two cons-cell lists — the same
/// element-by-element rule the old `Rc<Vec<Value>>` comparison gave us,
/// minus the pointer identity.
fn cons_cells_eq(a: &ConsCell, b: &ConsCell) -> bool {
    let mut ca = a;
    let mut cb = b;
    loop {
        match (ca.first(), cb.first()) {
            (None, None) => return true,
            (Some(x), Some(y)) => {
                if x != y {
                    return false;
                }
                match (ca.rest(), cb.rest()) {
                    (Some(na), Some(nb)) => {
                        ca = na;
                        cb = nb;
                    }
                    // Both exhausted after this element — equal.
                    (None, None) => return true,
                    // One list is longer than the other.
                    _ => return false,
                }
            }
            _ => return false,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Nil => write!(f, "nil"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => {
                if x.fract() == 0.0 && x.is_finite() {
                    write!(f, "{x:.1}")
                } else {
                    write!(f, "{x}")
                }
            }
            Value::Str(s) => write!(f, "{s}"),
            Value::Sym(s) => write!(f, "{s}"),
            Value::List(items) => {
                write!(f, "(")?;
                let mut first = true;
                let mut cur = items;
                while let Some(v) = cur.first() {
                    if !first {
                        write!(f, " ")?;
                    }
                    first = false;
                    write!(f, "{}", v.repr())?;
                    match cur.rest() {
                        Some(next) => cur = next,
                        None => break,
                    }
                }
                write!(f, ")")
            }
            Value::Map(pairs) => {
                write!(f, "{{")?;
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    write!(f, "{} {}", k.repr(), v.repr())?;
                }
                write!(f, "}}")
            }
            Value::Builtin { name, .. } => write!(f, "<builtin {name}>"),
            Value::Closure(_) => write!(f, "<fn>"),
        }
    }
}

impl Value {
    /// Like `Display` but strings are quoted — used inside list rendering so a
    /// list of strings is unambiguous.
    pub fn repr(&self) -> String {
        match self {
            Value::Str(s) => format!("{s:?}"),
            other => other.to_string(),
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.repr())
    }
}
