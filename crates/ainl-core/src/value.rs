//! Runtime values.

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
    pub body: Vec<Node>,
    pub env: Env,
}

/// A native function implemented in Rust.
pub type BuiltinFn = fn(&[Value]) -> Result<Value>;

#[derive(Clone)]
pub enum Value {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Rc<String>),
    /// A quoted symbol (from `(quote x)`), distinct from a variable reference.
    Sym(Rc<String>),
    List(Rc<Vec<Value>>),
    Builtin {
        name: &'static str,
        f: BuiltinFn,
    },
    Closure(Rc<Closure>),
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
            (Value::Int(a), Value::Float(b)) | (Value::Float(b), Value::Int(a)) => {
                (*a as f64) == *b
            }
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Sym(a), Value::Sym(b)) => a == b,
            (Value::List(a), Value::List(b)) => a == b,
            _ => false,
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
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    write!(f, "{}", v.repr())?;
                }
                write!(f, ")")
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
