//! `sort` — the one collection builtin that is a real builtin.
//!
//! ## The shape of this module
//!
//! There are four collection operations in AINL: `map`, `filter`, `reduce` and
//! `sort`. Only `sort` lives here. The other three are **special forms**, lowered
//! to `let` + `while` loops by `collection_forms` before anything evaluates
//! them, because:
//!
//! * `BuiltinFn` is `fn(&[Value]) -> Result<Value>` — a bare function pointer
//!   with no environment. A builtin gets its arguments and nothing else, so it
//!   cannot *call* a `Value::Closure` it was handed. The tree-walk's builtin arm
//!   is `f(args)` and the VM's is `f(&args)`; neither has the frame the call
//!   needs, and the VM's closure frame lives on a frame stack only `vm::run` can
//!   push onto.
//! * The AOT C runtime *can* recurse today (`Closure.fn` is a real function
//!   pointer) and all three transpiler targets can (they emit real host
//!   lambdas). So a Rust-only limitation would have shown up as a silent
//!   divergence — working in C/Python/JS/Ruby, failing in the interpreter — and
//!   the three transpiler parity suites would not have caught it. That is
//!   exactly the class of bug the 4-backend rule exists to prevent.
//!
//! `sort` gets away with being a builtin because its comparator is called
//! through `eval::apply` — the same entry point an ordinary `(f x)` call uses —
//! and that is in scope here because the tree-walk and the VM each own one.
//!
//! ## The rules every backend implements
//!
//! 1. **Pure.** A new list is returned. The input is never mutated — the list
//!    representation is shared, immutable cons cells, so this is free.
//! 2. **Empty input** is `()`.
//! 3. **Default order**: numbers by value, strings bytewise. A mixed-type list
//!    is an **error**, never an arbitrary-but-stable order — see
//!    [`default_compare`].
//! 4. **Stable**: equal elements keep their input order, on every backend.
//!
//! Rules 3 and 4 are why this is hand-written per backend rather than delegated
//! to a host `sort`: Ruby's `sort_by` is not stable, JS sorts strings by UTF-16
//! code unit, Python compares code points, and none of them rejects a mixed
//! list. One explicit merge sort per backend makes each rule a property of code
//! that is right there, rather than of a host's specification.

use crate::error::{Error, Result};
use crate::value::{ConsCell, Value};
use std::cmp::Ordering;

// ---------------------------------------------------------------------------
// calling a `fn` value
// ---------------------------------------------------------------------------

/// Call `callee` with `args`, from whichever evaluator is running.
///
/// This is the whole reason `sort`'s comparator form works at all, and the
/// reason it needed a seam rather than a bare `eval::apply` call:
///
/// * A **VM** closure carries `code` and an empty `body`; its frame has to be
///   pushed onto the VM's frame stack, which only `vm::run` can do.
/// * A **tree-walk** closure carries `body` and no `code`; it is interpreted
///   node by node, and `vm::run` refuses it by name.
///
/// So the same `Value::Closure` needs a different entry point per evaluator, and
/// a builtin — which is handed only `&[Value]` and cannot know which evaluator
/// called it — has to dispatch on the value itself. The discriminator is
/// `code.is_some()`, which is true for exactly the closures the VM made.
///
/// A closure that is neither (a builtin reaching one from outside a run) cannot
/// happen: every closure in a run was made by one of the two.
fn apply_fn(callee: &Value, args: &[Value], who: &str) -> Result<Value> {
    let Value::Closure(c) = callee else {
        return Err(Error::runtime(format!(
            "{who} expects a fn, got {}",
            callee.type_name()
        )));
    };
    if c.code.is_some() {
        crate::vm::call_closure(callee.clone(), args)
    } else {
        crate::eval::apply(callee.clone(), args)
    }
}

/// The list operand, with the builtin named in the error so a reader can see
/// which call was wrong — the existing stdlib convention (`abs expects a
/// number, got str`).
fn as_list<'a>(v: &'a Value, who: &str) -> Result<&'a ConsCell> {
    match v {
        Value::List(l) => Ok(l),
        other => Err(Error::runtime(format!(
            "{who} expects a list, got {}",
            other.type_name()
        ))),
    }
}

/// Flatten a cons list into a `Vec`, front to back.
///
/// O(n). The allocation is unavoidable: the result is a *new* list and the input
/// is shared and immutable, so there is nothing to mutate in place.
fn to_vec(l: &ConsCell) -> Vec<Value> {
    let mut out = Vec::with_capacity(l.len);
    let mut cur = l;
    while let Some(h) = cur.first() {
        out.push(h.clone());
        match cur.rest() {
            Some(next) => cur = next,
            None => break,
        }
    }
    out
}

// ---------------------------------------------------------------------------
// ordering
// ---------------------------------------------------------------------------

/// The default ordering: numbers by value, strings bytewise, nothing else.
///
/// A mixed-type list is **rejected**, not given a defined-but-arbitrary order.
/// A `sort` that quietly put every number before every string would return a
/// stable, reproducible answer to a program that has a bug in it, and that bug
/// would surface much later as a wrong number instead of here as a type error.
pub fn default_compare(a: &Value, b: &Value) -> Result<Ordering> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(x.cmp(y)),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y).ok_or_else(nan_err),
        (Value::Int(_), Value::Float(_)) | (Value::Float(_), Value::Int(_)) => {
            // int and float compare by value, not by tag. `Value`'s own
            // `PartialEq` already mixes them this way (`(= 1 1.0)` is true), and
            // a `sort` that ordered `[1, 1.0]` by tag would be a different
            // answer to the same question `(= 1 1.0)` says yes to.
            let (xf, yf) = match (a, b) {
                (Value::Int(x), Value::Float(y)) => (*x as f64, *y),
                (Value::Float(x), Value::Int(y)) => (*x, *y as f64),
                _ => unreachable!("matched by the outer arm"),
            };
            xf.partial_cmp(&yf).ok_or_else(nan_err)
        }
        (Value::Str(x), Value::Str(y)) => Ok(compare_bytes(x, y)),
        _ => Err(Error::runtime(format!(
            "sort expects a list of numbers or of strings, got a list mixing {} and {}",
            a.type_name(),
            b.type_name()
        ))),
    }
}

/// `sort` cannot produce a NaN from AINL arithmetic — `(/ 0 0)` raises
/// `division by zero` — but one can still *arrive* from JSON, whose grammar
/// admits `NaN`. Kept as one function so every backend can word it the same.
fn nan_err() -> Error {
    Error::runtime("sort cannot compare NaN")
}

/// Bytewise string order.
///
/// Deliberately **bytes**, not code points and not UTF-16 code units. Python
/// compares code points, JavaScript compares UTF-16 code units, Ruby compares
/// bytes; only "bytes" is the same total order in all three, and the AOT
/// runtime spells it `memcmp`. This is the same decision that pinned `upcase`
/// and `trim` to ASCII: a language whose strings are byte strings orders them
/// the way the bytes run.
pub fn compare_bytes(a: &str, b: &str) -> Ordering {
    a.as_bytes().cmp(b.as_bytes())
}

/// Normalise whatever a user comparator returned into a sign.
///
/// The contract is negative / zero / positive. A comparator that returned a bool
/// or a string is a mistake worth naming: coercing it to 0 would make a broken
/// comparator look like an "equal" one and leave the list in input order, which
/// reads as a working sort.
fn comparator_sign(v: &Value) -> Result<Ordering> {
    let n = match v {
        Value::Int(i) => *i as f64,
        Value::Float(x) => *x,
        other => {
            return Err(Error::runtime(format!(
                "sort comparator must return a number, got {}",
                other.type_name()
            )))
        }
    };
    if n.is_nan() {
        return Err(Error::runtime(
            "sort comparator must return a number, got NaN",
        ));
    }
    Ok(if n < 0.0 {
        Ordering::Less
    } else if n > 0.0 {
        Ordering::Greater
    } else {
        Ordering::Equal
    })
}

// ---------------------------------------------------------------------------
// sort
// ---------------------------------------------------------------------------

/// `(sort lst)` / `(sort cmp lst)` → a sorted **copy**, stably.
pub fn sort_list(args: &[Value]) -> Result<Value> {
    match args {
        [l] => {
            let lst = as_list(l, "sort")?;
            sort_ordered(lst, default_compare)
        }
        [f, l] => {
            // Check the comparator's TYPE here, not inside the comparison
            // closure. The closure only runs when two elements are actually
            // compared, so a 0- or 1-element list would never reach it and
            // `(sort "x" (list 1))` would return `(1)` — accepting a string as a
            // comparator and saying nothing. Every other backend checks the
            // comparator up front too, so doing it here is what keeps a short
            // list from being the one case that diverges.
            if !matches!(f, Value::Closure(_)) {
                return Err(Error::runtime(format!(
                    "sort expects a fn, got {}",
                    f.type_name()
                )));
            }
            let lst = as_list(l, "sort")?;
            sort_ordered(lst, |a, b| {
                comparator_sign(&apply_fn(f, &[a.clone(), b.clone()], "sort")?)
            })
        }
        _ => Err(Error::runtime("sort expects (sort list) or (sort fn list)")),
    }
}

/// Sort `lst`'s elements with `cmp`, stably.
///
/// A stable sort: equal elements keep their input order. The bottom-up merge
/// takes from the left run on ties, which makes stability a property of the
/// code rather than of whatever the host's `sort` happens to guarantee.
pub fn sort_ordered<F>(lst: &ConsCell, cmp: F) -> Result<Value>
where
    F: Fn(&Value, &Value) -> Result<Ordering>,
{
    let mut items = to_vec(lst);
    if items.len() > 1 {
        merge_sort(&mut items, &cmp)?;
    }
    Ok(Value::List(ConsCell::from_values(items)))
}

/// Bottom-up stable merge sort.
///
/// Iterative rather than the recursive top-down form so a long list cannot grow
/// the Rust stack — the same concern that made `ConsCell::drop` iterative.
fn merge_sort<F>(items: &mut Vec<Value>, cmp: &F) -> Result<()>
where
    F: Fn(&Value, &Value) -> Result<Ordering>,
{
    let n = items.len();
    let mut src: Vec<Value> = std::mem::take(items);
    let mut dst: Vec<Value> = Vec::with_capacity(n);
    let mut width = 1;
    while width < n {
        dst.clear();
        let mut start = 0;
        while start < n {
            let mid = (start + width).min(n);
            let end = (start + 2 * width).min(n);
            let (mut l, mut r) = (start, mid);
            while l < mid && r < end {
                // `!= Greater` takes from the left on ties — the stability rule.
                if cmp(&src[l], &src[r])? != Ordering::Greater {
                    dst.push(src[l].clone());
                    l += 1;
                } else {
                    dst.push(src[r].clone());
                    r += 1;
                }
            }
            while l < mid {
                dst.push(src[l].clone());
                l += 1;
            }
            while r < end {
                dst.push(src[r].clone());
                r += 1;
            }
            start = end;
        }
        std::mem::swap(&mut src, &mut dst);
        width *= 2;
    }
    *items = src;
    Ok(())
}

// ---------------------------------------------------------------------------
// installation
// ---------------------------------------------------------------------------

/// Bind `sort`.
///
/// `map`/`filter`/`reduce` have no binding at all — they are special forms, like
/// `let` and `while`, and are handled by `collection_forms` before the prelude
/// is ever consulted. `sort` is an ordinary builtin and so is reachable as a
/// value: `(def s sort)` works.
pub fn install(env: &crate::eval::Env) {
    env.define(
        "sort",
        Value::Builtin {
            name: "sort",
            f: sort_list,
        },
    );
}
