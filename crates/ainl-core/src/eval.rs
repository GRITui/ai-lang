//! Environment + tree-walking evaluator.

use crate::error::{Error, Result};
use crate::parser::Node;
use crate::value::{Closure, ConsCell, Value};
use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

// ---- resource limits --------------------------------------------------------
//
// The tree-walking evaluator recurses through native Rust call frames with no
// inherent bound, and `while` has no iteration cap. Both are reachable from a
// single AINL source form (deep recursion, deeply nested calls, `(while true
// ...)`), and a Rust stack overflow is not a catchable `Result` — it aborts
// the process outright. These two thread-local counters turn both failure
// modes into a clean `Error::Runtime` instead.

thread_local! {
    static DEPTH: Cell<usize> = const { Cell::new(0) };
    static STEPS: Cell<u64> = const { Cell::new(0) };
}

/// Max live `eval` call frames (recursion + nested-expression depth combined).
/// Kept well below where a native stack overflow could occur even on a
/// constrained thread stack (e.g. `cargo test`'s worker threads default to a
/// couple MiB, smaller than a typical main-thread stack).
const MAX_DEPTH: usize = 512;
/// Max total `eval` invocations per top-level `run_str`/`run_in` call — bounds
/// unbounded loops (`while true`) and runaway iteration generally.
const MAX_STEPS: u64 = 2_000_000;

/// Reset the step budget for a fresh top-level run. Depth is guaranteed back
/// at 0 between runs (the RAII guard below always decrements on the way out,
/// success or error), so only steps need an explicit reset.
pub(crate) fn reset_limits() {
    STEPS.with(|s| s.set(0));
}

struct DepthGuard;

impl DepthGuard {
    fn enter() -> Result<DepthGuard> {
        let exceeded = DEPTH.with(|d| {
            let v = d.get() + 1;
            d.set(v);
            v > MAX_DEPTH
        });
        if exceeded {
            DEPTH.with(|d| d.set(d.get() - 1));
            return Err(Error::runtime(format!(
                "recursion limit exceeded (max depth {MAX_DEPTH})"
            )));
        }
        Ok(DepthGuard)
    }
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get() - 1));
    }
}

fn tick() -> Result<()> {
    let exceeded = STEPS.with(|s| {
        let v = s.get() + 1;
        s.set(v);
        v > MAX_STEPS
    });
    if exceeded {
        return Err(Error::runtime(format!(
            "step limit exceeded (max {MAX_STEPS} evaluation steps) — likely an infinite loop or runaway recursion"
        )));
    }
    Ok(())
}

/// A lexical scope with an optional parent. `Env` is a cheap `Rc` handle so
/// closures can share and outlive the scope that created them.
#[derive(Clone)]
pub struct Env(Rc<Scope>);

struct Scope {
    vars: RefCell<HashMap<String, Value>>,
    parent: Option<Env>,
}

#[cfg(test)]
thread_local! {
    /// Count of currently-live `Scope` allocations, for whiteboxing the
    /// cycle-breaking fix below — see `tests::self_referential_closure_does_not_leak`.
    static LIVE_SCOPES: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
impl Drop for Scope {
    fn drop(&mut self) {
        LIVE_SCOPES.with(|c| c.set(c.get() - 1));
    }
}

impl Env {
    pub fn new() -> Env {
        #[cfg(test)]
        LIVE_SCOPES.with(|c| c.set(c.get() + 1));
        Env(Rc::new(Scope {
            vars: RefCell::new(HashMap::new()),
            parent: None,
        }))
    }

    pub fn child(&self) -> Env {
        #[cfg(test)]
        LIVE_SCOPES.with(|c| c.set(c.get() + 1));
        Env(Rc::new(Scope {
            vars: RefCell::new(HashMap::new()),
            parent: Some(self.clone()),
        }))
    }

    pub fn define(&self, name: impl Into<String>, val: Value) {
        self.0.vars.borrow_mut().insert(name.into(), val);
    }

    pub fn get(&self, name: &str) -> Option<Value> {
        if let Some(v) = self.0.vars.borrow().get(name) {
            return Some(v.clone());
        }
        self.0.parent.as_ref().and_then(|p| p.get(name))
    }

    /// A fresh global environment with all builtins bound.
    pub fn with_prelude() -> Env {
        let env = Env::new();
        crate::eval::install_prelude(&env);
        env
    }

    /// True if `self` is `other` or one of `other`'s ancestors — i.e.
    /// something reachable through `other`'s parent chain still needs
    /// `self`'s bindings.
    fn is_ancestor_of(&self, other: &Env) -> bool {
        let mut cur = other.clone();
        loop {
            if Rc::ptr_eq(&self.0, &cur.0) {
                return true;
            }
            match cur.0.parent.clone() {
                Some(p) => cur = p,
                None => return false,
            }
        }
    }

    /// Drop all bindings in this scope, breaking any `Rc` cycle rooted here.
    ///
    /// A closure that re-`def`s itself into its own call scope (the common
    /// named-recursive-function pattern) makes that scope's `vars` map hold
    /// a `Closure` whose `env` points right back at the scope — an `Rc`
    /// cycle that never frees on its own. Called only once a call/`let`
    /// scope's body has finished evaluating and nothing in its result still
    /// needs it (see `apply` and `sf_let`), so this never removes bindings a
    /// live closure could still look up.
    fn clear(&self) {
        self.0.vars.borrow_mut().clear();
    }
}

/// True if some closure reachable from `val` (directly, or nested inside a
/// list) was defined in `env` or in a scope that has `env` as an ancestor —
/// i.e. `env`'s bindings are still needed by something the caller now holds.
fn value_keeps_env_alive(val: &Value, env: &Env) -> bool {
    match val {
        Value::Closure(c) => env.is_ancestor_of(&c.env),
        Value::List(items) => {
            let mut cur = items;
            loop {
                match cur.first() {
                    Some(v) => {
                        if value_keeps_env_alive(v, env) {
                            return true;
                        }
                        match cur.rest() {
                            Some(next) => cur = next,
                            None => return false,
                        }
                    }
                    None => return false,
                }
            }
        }
        Value::Map(pairs) => pairs
            .iter()
            .any(|(k, v)| value_keeps_env_alive(k, env) || value_keeps_env_alive(v, env)),
        _ => false,
    }
}

impl Default for Env {
    fn default() -> Self {
        Env::new()
    }
}

pub fn eval(node: &Node, env: &Env) -> Result<Value> {
    let _guard = DepthGuard::enter()?;
    tick()?;
    match node {
        Node::Int(i, _) => Ok(Value::Int(*i)),
        Node::Float(x, _) => Ok(Value::Float(*x)),
        Node::Str(s, _) => Ok(Value::str(s.clone())),
        Node::Sym(name, _) => match name.as_str() {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            "nil" => Ok(Value::Nil),
            _ => env
                .get(name)
                .ok_or_else(|| Error::runtime(format!("unbound symbol '{name}'"))),
        },
        Node::List(items, _) => eval_list(items, env),
    }
}

fn eval_list(items: &[Node], env: &Env) -> Result<Value> {
    let Some(head) = items.first() else {
        // empty list evaluates to nil
        return Ok(Value::Nil);
    };

    // Special forms are dispatched by the head symbol before evaluating args.
    if let Node::Sym(op, _) = head {
        match op.as_str() {
            "def" => return sf_def(&items[1..], env),
            "fn" => return sf_fn(&items[1..], env),
            "if" => return sf_if(&items[1..], env),
            "do" => return sf_do(&items[1..], env),
            "let" => return sf_let(&items[1..], env),
            "while" => return sf_while(&items[1..], env),
            "quote" => return sf_quote(&items[1..]),
            "and" => return sf_and(&items[1..], env),
            "or" => return sf_or(&items[1..], env),
            _ => {}
        }
    }

    // Otherwise it's a call: evaluate head + args, then apply.
    let callee = eval(head, env)?;
    let mut args = Vec::with_capacity(items.len() - 1);
    for a in &items[1..] {
        args.push(eval(a, env)?);
    }
    apply(callee, &args)
}

pub fn apply(callee: Value, args: &[Value]) -> Result<Value> {
    match callee {
        Value::Builtin { f, .. } => f(args),
        Value::Closure(clos) => {
            let call_env = clos.env.child();
            let np = clos.params.len();
            if clos.variadic.is_some() {
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
            for (name, val) in clos.params.iter().zip(args.iter()) {
                call_env.define(name.clone(), val.clone());
            }
            if let Some(rest) = &clos.variadic {
                let extra: Vec<Value> = args[np..].to_vec();
                call_env.define(rest.clone(), Value::List(ConsCell::from_values(extra)));
            }
            let mut last = Value::Nil;
            let mut err = None;
            for form in &clos.body {
                match eval(form, &call_env) {
                    Ok(v) => last = v,
                    Err(e) => {
                        err = Some(e);
                        break;
                    }
                }
            }
            // Break a self-referential-def cycle rooted in this call's own
            // scope, unless the result still needs it (a closure escaped
            // that was defined in — or under — call_env).
            if err.is_some() || !value_keeps_env_alive(&last, &call_env) {
                call_env.clear();
            }
            match err {
                Some(e) => Err(e),
                None => Ok(last),
            }
        }
        other => Err(Error::runtime(format!(
            "cannot call a {}",
            other.type_name()
        ))),
    }
}

// ---- special forms ---------------------------------------------------------

fn sf_def(args: &[Node], env: &Env) -> Result<Value> {
    let [name_node, val_node] = args else {
        return Err(Error::runtime("def expects (def name value)"));
    };
    let Node::Sym(name, _) = name_node else {
        return Err(Error::runtime("def name must be a symbol"));
    };
    let val = eval(val_node, env)?;
    env.define(name.clone(), val);
    Ok(Value::Sym(Rc::new(name.clone())))
}

fn sf_fn(args: &[Node], env: &Env) -> Result<Value> {
    let Some((params_node, body)) = args.split_first() else {
        return Err(Error::runtime("fn expects (fn (params...) body...)"));
    };
    let Node::List(param_nodes, _) = params_node else {
        return Err(Error::runtime("fn params must be a list"));
    };
    let mut params = Vec::new();
    let mut variadic = None;
    let mut i = 0;
    while i < param_nodes.len() {
        let Node::Sym(p, _) = &param_nodes[i] else {
            return Err(Error::runtime("fn params must be symbols"));
        };
        if p == "&" {
            let Some(Node::Sym(rest, _)) = param_nodes.get(i + 1) else {
                return Err(Error::runtime("'&' must be followed by a rest parameter"));
            };
            variadic = Some(rest.clone());
            break;
        }
        params.push(p.clone());
        i += 1;
    }
    Ok(Value::Closure(Rc::new(Closure {
        params,
        variadic,
        body: body.to_vec(),
        env: env.clone(),
    })))
}

fn sf_if(args: &[Node], env: &Env) -> Result<Value> {
    match args {
        [cond, then] => {
            if eval(cond, env)?.is_truthy() {
                eval(then, env)
            } else {
                Ok(Value::Nil)
            }
        }
        [cond, then, els] => {
            if eval(cond, env)?.is_truthy() {
                eval(then, env)
            } else {
                eval(els, env)
            }
        }
        _ => Err(Error::runtime("if expects (if cond then [else])")),
    }
}

fn sf_do(args: &[Node], env: &Env) -> Result<Value> {
    let mut last = Value::Nil;
    for form in args {
        last = eval(form, env)?;
    }
    Ok(last)
}

fn sf_let(args: &[Node], env: &Env) -> Result<Value> {
    let Some((binds_node, body)) = args.split_first() else {
        return Err(Error::runtime("let expects (let ((n v)...) body...)"));
    };
    let Node::List(binds, _) = binds_node else {
        return Err(Error::runtime("let bindings must be a list"));
    };
    let scope = env.child();
    let result = sf_let_body(binds, body, &scope);
    // Break a self-referential-def cycle rooted in this let's own scope
    // (e.g. `(let () (def loop (fn () (loop))) ...)`), unless the result
    // still needs it (a closure escaped that was defined in — or under —
    // scope).
    let last = match &result {
        Ok(v) => v,
        Err(_) => &Value::Nil,
    };
    if result.is_err() || !value_keeps_env_alive(last, &scope) {
        scope.clear();
    }
    result
}

fn sf_let_body(binds: &[Node], body: &[Node], scope: &Env) -> Result<Value> {
    for b in binds {
        let Node::List(pair, _) = b else {
            return Err(Error::runtime("each let binding must be (name value)"));
        };
        let [Node::Sym(name, _), val_node] = &pair[..] else {
            return Err(Error::runtime("each let binding must be (name value)"));
        };
        let val = eval(val_node, scope)?;
        scope.define(name.clone(), val);
    }
    let mut last = Value::Nil;
    for form in body {
        last = eval(form, scope)?;
    }
    Ok(last)
}

fn sf_while(args: &[Node], env: &Env) -> Result<Value> {
    let Some((cond, body)) = args.split_first() else {
        return Err(Error::runtime("while expects (while cond body...)"));
    };
    let mut last = Value::Nil;
    while eval(cond, env)?.is_truthy() {
        for form in body {
            last = eval(form, env)?;
        }
    }
    Ok(last)
}

fn sf_quote(args: &[Node]) -> Result<Value> {
    let [node] = args else {
        return Err(Error::runtime("quote expects one form"));
    };
    Ok(quote_node(node))
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

fn sf_and(args: &[Node], env: &Env) -> Result<Value> {
    let mut last = Value::Bool(true);
    for a in args {
        last = eval(a, env)?;
        if !last.is_truthy() {
            return Ok(last);
        }
    }
    Ok(last)
}

fn sf_or(args: &[Node], env: &Env) -> Result<Value> {
    for a in args {
        let v = eval(a, env)?;
        if v.is_truthy() {
            return Ok(v);
        }
    }
    Ok(Value::Bool(false))
}

// ---- builtins --------------------------------------------------------------

fn install_prelude(env: &Env) {
    macro_rules! b {
        ($name:literal, $f:expr) => {
            env.define($name, Value::Builtin { name: $name, f: $f });
        };
    }

    b!("+", |a| numeric_fold(
        a,
        0.0,
        0,
        |x, y| x + y,
        |x, y| x.checked_add(y)
    ));
    b!("*", |a| numeric_fold(
        a,
        1.0,
        1,
        |x, y| x * y,
        |x, y| x.checked_mul(y)
    ));
    b!("-", builtin_sub);
    b!("/", builtin_div);
    b!("=", |a| Ok(Value::Bool(a.windows(2).all(|w| w[0] == w[1]))));
    b!("<", |a| compare(a, |o| o == std::cmp::Ordering::Less));
    b!(">", |a| compare(a, |o| o == std::cmp::Ordering::Greater));
    b!("<=", |a| compare(a, |o| o != std::cmp::Ordering::Greater));
    b!(">=", |a| compare(a, |o| o != std::cmp::Ordering::Less));
    b!("not", |a| Ok(Value::Bool(!arg1(a)?.is_truthy())));
    b!("mod", builtin_mod);

    b!("print", |a| {
        let parts: Vec<String> = a.iter().map(|v| v.to_string()).collect();
        println!("{}", parts.join(" "));
        Ok(Value::Nil)
    });
    b!("str", |a| {
        let s: String = a.iter().map(|v| v.to_string()).collect();
        Ok(Value::str(s))
    });

    b!("list", |a| Ok(Value::List(ConsCell::from_values(
        a.to_vec()
    ))));
    b!("len", builtin_len);
    b!("first", builtin_first);
    b!("rest", builtin_rest);
    b!("nth", builtin_nth);
    b!("cons", builtin_cons);
    b!("push", builtin_push);

    b!("hash", builtin_hash);
    b!("get", builtin_get);
    b!("assoc", builtin_assoc);
    b!("has", builtin_has);
    b!("keys", builtin_keys);
    b!("vals", builtin_vals);

    b!("error", |a| Err(Error::runtime(
        a.iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    )));
}

fn arg1(a: &[Value]) -> Result<&Value> {
    a.first()
        .ok_or_else(|| Error::runtime("expected 1 argument"))
}

fn as_f64(v: &Value) -> Result<f64> {
    match v {
        Value::Int(i) => Ok(*i as f64),
        Value::Float(x) => Ok(*x),
        other => Err(Error::runtime(format!(
            "expected a number, got {}",
            other.type_name()
        ))),
    }
}

/// Fold numeric args, staying in integer arithmetic until a float appears (or
/// an integer op overflows), matching typical dynamic-language semantics.
fn numeric_fold(
    args: &[Value],
    _f_id: f64,
    i_id: i64,
    ff: fn(f64, f64) -> f64,
    fi: fn(i64, i64) -> Option<i64>,
) -> Result<Value> {
    let mut acc_i = i_id;
    let mut acc_f = 0.0f64;
    let mut is_float = false;
    let mut first = true;
    for v in args {
        match v {
            Value::Int(i) if !is_float => match fi(acc_i, *i) {
                Some(r) => acc_i = r,
                None => {
                    is_float = true;
                    acc_f = ff(acc_i as f64, *i as f64);
                }
            },
            _ => {
                let x = as_f64(v)?;
                if !is_float {
                    is_float = true;
                    acc_f = acc_i as f64;
                }
                acc_f = ff(acc_f, x);
            }
        }
        first = false;
    }
    let _ = first;
    if is_float {
        Ok(Value::Float(acc_f))
    } else {
        Ok(Value::Int(acc_i))
    }
}

fn builtin_sub(args: &[Value]) -> Result<Value> {
    match args {
        [] => Err(Error::runtime("- expects at least 1 argument")),
        [one] => match one {
            // i64::MIN has no positive i64 counterpart; checked_neg catches
            // that (rather than silently wrapping in release builds) and we
            // promote to float, matching every other arithmetic op's overflow
            // behavior.
            Value::Int(i) => match i.checked_neg() {
                Some(r) => Ok(Value::Int(r)),
                None => Ok(Value::Float(-(*i as f64))),
            },
            Value::Float(x) => Ok(Value::Float(-*x)),
            other => Err(Error::runtime(format!(
                "- expected number, got {}",
                other.type_name()
            ))),
        },
        [first, rest @ ..] => {
            let mut all_int = matches!(first, Value::Int(_));
            for v in rest {
                all_int &= matches!(v, Value::Int(_));
            }
            if all_int {
                let mut acc = if let Value::Int(i) = first {
                    *i
                } else {
                    unreachable!()
                };
                for v in rest {
                    if let Value::Int(i) = v {
                        match acc.checked_sub(*i) {
                            Some(r) => acc = r,
                            None => return float_sub(first, rest),
                        }
                    }
                }
                Ok(Value::Int(acc))
            } else {
                float_sub(first, rest)
            }
        }
    }
}

fn float_sub(first: &Value, rest: &[Value]) -> Result<Value> {
    let mut acc = as_f64(first)?;
    for v in rest {
        acc -= as_f64(v)?;
    }
    Ok(Value::Float(acc))
}

fn builtin_div(args: &[Value]) -> Result<Value> {
    let [first, rest @ ..] = args else {
        return Err(Error::runtime("/ expects at least 1 argument"));
    };
    let mut acc = as_f64(first)?;
    if rest.is_empty() {
        return Ok(Value::Float(1.0 / acc));
    }
    for v in rest {
        let d = as_f64(v)?;
        if d == 0.0 {
            return Err(Error::runtime("division by zero"));
        }
        acc /= d;
    }
    Ok(Value::Float(acc))
}

fn builtin_mod(args: &[Value]) -> Result<Value> {
    let [Value::Int(a), Value::Int(b)] = args else {
        return Err(Error::runtime("mod expects (mod int int)"));
    };
    if *b == 0 {
        return Err(Error::runtime("mod by zero"));
    }
    // `i64::MIN.rem_euclid(-1)` panics: the *quotient* (i64::MAX + 1) doesn't
    // fit in i64, even though the mathematical remainder of dividing by ±1 is
    // always 0. Special-case it rather than letting the overflow through.
    if *b == -1 {
        return Ok(Value::Int(0));
    }
    Ok(Value::Int(a.rem_euclid(*b)))
}

fn compare(args: &[Value], keep: fn(std::cmp::Ordering) -> bool) -> Result<Value> {
    for w in args.windows(2) {
        let a = as_f64(&w[0])?;
        let b = as_f64(&w[1])?;
        let ord = a
            .partial_cmp(&b)
            .ok_or_else(|| Error::runtime("cannot compare NaN"))?;
        if !keep(ord) {
            return Ok(Value::Bool(false));
        }
    }
    Ok(Value::Bool(true))
}

fn builtin_len(args: &[Value]) -> Result<Value> {
    match arg1(args)? {
        Value::List(l) => Ok(Value::Int(l.len as i64)),
        Value::Str(s) => Ok(Value::Int(s.chars().count() as i64)),
        Value::Map(m) => Ok(Value::Int(m.len() as i64)),
        other => Err(Error::runtime(format!(
            "len expects list, str, or hash, got {}",
            other.type_name()
        ))),
    }
}

/// `(hash k v k v ...)` — build a map from variadic key/value pairs. A
/// repeated key keeps its *last* value and its *first* position, matching
/// the everyday "later assignment wins" expectation.
fn builtin_hash(args: &[Value]) -> Result<Value> {
    if !args.len().is_multiple_of(2) {
        return Err(Error::runtime(format!(
            "hash expects an even number of key/value arguments, got {}",
            args.len()
        )));
    }
    let mut pairs: Vec<(Value, Value)> = Vec::with_capacity(args.len() / 2);
    for i in (0..args.len()).step_by(2) {
        let (k, v) = (args[i].clone(), args[i + 1].clone());
        match pairs.iter_mut().find(|(ek, _)| *ek == k) {
            Some((_, ev)) => *ev = v,
            None => pairs.push((k, v)),
        }
    }
    Ok(Value::Map(Rc::new(pairs)))
}

fn as_map<'a>(v: &'a Value, who: &str) -> Result<&'a Rc<Vec<(Value, Value)>>> {
    match v {
        Value::Map(m) => Ok(m),
        other => Err(Error::runtime(format!(
            "{who} expects a hash, got {}",
            other.type_name()
        ))),
    }
}

/// `(get h k)` — look up a key; `nil` if absent, matching `nth`'s
/// out-of-range convention.
fn builtin_get(args: &[Value]) -> Result<Value> {
    let [h, k] = args else {
        return Err(Error::runtime("get expects (get hash key)"));
    };
    let m = as_map(h, "get")?;
    Ok(m.iter()
        .find(|(ek, _)| ek == k)
        .map(|(_, v)| v.clone())
        .unwrap_or(Value::Nil))
}

/// `(assoc h k v)` — a *new* map with `k` bound to `v`, like `cons`/`push`
/// leaving the original untouched. Updates in place (keeps position) if `k`
/// already exists, else appends.
fn builtin_assoc(args: &[Value]) -> Result<Value> {
    let [h, k, v] = args else {
        return Err(Error::runtime("assoc expects (assoc hash key value)"));
    };
    let m = as_map(h, "assoc")?;
    let mut pairs = (**m).clone();
    match pairs.iter_mut().find(|(ek, _)| ek == k) {
        Some((_, ev)) => *ev = v.clone(),
        None => pairs.push((k.clone(), v.clone())),
    }
    Ok(Value::Map(Rc::new(pairs)))
}

fn builtin_has(args: &[Value]) -> Result<Value> {
    let [h, k] = args else {
        return Err(Error::runtime("has expects (has hash key)"));
    };
    let m = as_map(h, "has")?;
    Ok(Value::Bool(m.iter().any(|(ek, _)| ek == k)))
}

fn builtin_keys(args: &[Value]) -> Result<Value> {
    let m = as_map(arg1(args)?, "keys")?;
    Ok(Value::List(ConsCell::from_values(
        m.iter().map(|(k, _)| k.clone()),
    )))
}

fn builtin_vals(args: &[Value]) -> Result<Value> {
    let m = as_map(arg1(args)?, "vals")?;
    Ok(Value::List(ConsCell::from_values(
        m.iter().map(|(_, v)| v.clone()),
    )))
}

fn builtin_first(args: &[Value]) -> Result<Value> {
    match arg1(args)? {
        Value::List(l) => Ok(l.first().cloned().unwrap_or(Value::Nil)),
        other => Err(Error::runtime(format!(
            "first expects list, got {}",
            other.type_name()
        ))),
    }
}

fn builtin_rest(args: &[Value]) -> Result<Value> {
    match arg1(args)? {
        Value::List(l) => Ok(Value::List(match l.rest() {
            Some(rest) => rest.clone(),
            None => ConsCell::empty(),
        })),
        other => Err(Error::runtime(format!(
            "rest expects list, got {}",
            other.type_name()
        ))),
    }
}

fn builtin_nth(args: &[Value]) -> Result<Value> {
    let [Value::List(l), Value::Int(i)] = args else {
        return Err(Error::runtime("nth expects (nth list int)"));
    };
    if *i < 0 {
        return Ok(Value::Nil);
    }
    Ok(l.nth(*i as usize).cloned().unwrap_or(Value::Nil))
}

fn builtin_cons(args: &[Value]) -> Result<Value> {
    let [head, Value::List(l)] = args else {
        return Err(Error::runtime("cons expects (cons value list)"));
    };
    // O(1): a fresh cell pointing at the existing tail.
    Ok(Value::List(ConsCell::cons(head.clone(), l)))
}

fn builtin_push(args: &[Value]) -> Result<Value> {
    let [Value::List(l), tail @ ..] = args else {
        return Err(Error::runtime("push expects (push list value...)"));
    };
    // O(n + k): one traversal of the existing list, then append every value.
    // (Acceptable — the O(n²) case was repeated `cons`, which is now O(1).)
    let mut items = Vec::with_capacity(l.len + tail.len());
    let mut cur = l;
    while let Some(h) = cur.first() {
        items.push(h.clone());
        match cur.rest() {
            Some(next) => cur = next,
            None => break,
        }
    }
    for v in tail {
        items.push(v.clone());
    }
    Ok(Value::List(ConsCell::from_values(items)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_scopes() -> usize {
        LIVE_SCOPES.with(|c| c.get())
    }

    #[test]
    fn self_referential_closure_does_not_leak_its_call_scope() {
        LIVE_SCOPES.with(|c| c.set(0));
        let env = Env::with_prelude();
        // `helper` re-defines itself into make_counter's own call scope —
        // exactly the Rc-cycle-forming pattern. Called many times in a loop:
        // without the cycle-breaking fix in `apply`, each call leaks its own
        // call scope and never frees it.
        let src = "\
            (def make_counter (fn ()
              (def helper (fn (x) (if (= x 0) 0 (helper (- x 1)))))
              (helper 3)))
            (def i 0)
            (while (< i 500) (make_counter) (def i (+ i 1)))
            ";
        crate::run_in(src, &env).unwrap();
        let live = live_scopes();
        // Only long-lived scopes should remain (the global env + a couple
        // still referenced by `env`/`make_counter`'s own defining scope) —
        // nowhere near the 500+ this would be if every call scope leaked.
        assert!(live < 20, "expected a bounded live-scope count, got {live}");
    }

    #[test]
    fn escaping_closure_keeps_its_captured_scope_alive() {
        LIVE_SCOPES.with(|c| c.set(0));
        let env = Env::with_prelude();
        let src = "\
            (def make_adder (fn (n) (fn (x) (+ x n))))
            (def add5 (make_adder 5))
            ";
        crate::run_in(src, &env).unwrap();
        // `add5`'s captured scope (holding `n = 5`) must still be alive and
        // correct — this is the correctness counterpart to the leak test
        // above: the fix must not clear a scope a live closure still needs.
        assert_eq!(crate::run_in("(add5 10)", &env).unwrap(), Value::Int(15));
    }

    // ---- list (cons cell) semantics ---------------------------------------

    #[test]
    fn cons_rest_first_len_round_trips() {
        // cons prepends; first/rest peel; len counts — all the same values
        // the old Vec-backed lists produced.
        assert_eq!(
            crate::run_str("(first (cons 1 (list 2 3)))").unwrap(),
            Value::Int(1)
        );
        assert_eq!(
            crate::run_str("(rest (cons 1 (list 2 3)))").unwrap(),
            crate::run_str("(list 2 3)").unwrap()
        );
        assert_eq!(
            crate::run_str("(len (cons 1 (list 2 3)))").unwrap(),
            Value::Int(3)
        );
        // cons is immutable: the original list is untouched.
        let src = "\
            (def xs (list 2 3))\
            (def ys (cons 1 xs))\
            (list (len xs) (len ys) (first xs) (first ys))\
            ";
        assert_eq!(
            crate::run_str(src).unwrap(),
            crate::run_str("(list 2 3 2 1)").unwrap()
        );
        // cons of two equal lists compares equal to a directly built list.
        assert_eq!(
            crate::run_str("(= (cons 1 (list 2)) (list 1 2))").unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn nth_and_empty_list_edge_cases() {
        assert_eq!(
            crate::run_str("(nth (list 1 2 3) 0)").unwrap(),
            Value::Int(1)
        );
        assert_eq!(
            crate::run_str("(nth (list 1 2 3) 1)").unwrap(),
            Value::Int(2)
        );
        assert_eq!(
            crate::run_str("(nth (list 1 2 3) 2)").unwrap(),
            Value::Int(3)
        );
        assert_eq!(crate::run_str("(nth (list 1 2 3) 3)").unwrap(), Value::Nil);
        assert_eq!(crate::run_str("(nth (list 1 2 3) -1)").unwrap(), Value::Nil);
        // Empty list: len 0, first/rest are nil/empty.
        assert_eq!(crate::run_str("(len (list))").unwrap(), Value::Int(0));
        assert_eq!(crate::run_str("(first (list))").unwrap(), Value::Nil);
        assert_eq!(
            crate::run_str("(len (rest (list)))").unwrap(),
            Value::Int(0)
        );
        // `rest` of a one-element list is the empty list (not nil).
        assert_eq!(
            crate::run_str("(rest (list 1))").unwrap(),
            crate::run_str("(list)").unwrap()
        );
        // and it prints as `()`.
        assert_eq!(
            crate::run_str("(str (rest (list 1)))").unwrap(),
            Value::str("()")
        );
    }

    #[test]
    fn push_appends_to_the_end_and_is_non_mutating() {
        assert_eq!(
            crate::run_str("(push (list 1) 2 3)").unwrap(),
            crate::run_str("(list 1 2 3)").unwrap()
        );
        assert_eq!(
            crate::run_str("(push (list) 1)").unwrap(),
            crate::run_str("(list 1)").unwrap()
        );
        let src = "\
            (def xs (list 1))\
            (def ys (push xs 2))\
            (list (len xs) (len ys) (first ys) (rest ys))\
            ";
        assert_eq!(
            crate::run_str(src).unwrap(),
            crate::run_str("(list 1 2 1 (list 2))").unwrap()
        );
    }

    #[test]
    fn quote_and_variadic_rest_build_cons_lists() {
        // Quoted list literals and variadic rest bindings both build lists.
        assert_eq!(
            crate::run_str("(nth (quote (a b c)) 1)").unwrap(),
            crate::run_str("(quote b)").unwrap()
        );
        // (The rest param is named `xs` — naming it `rest` would shadow the
        // `rest` builtin inside the body.)
        let src = "(def f (fn (a & xs) (list a (len xs) (first xs) (rest xs)))) (f 1 2 3 4)";
        assert_eq!(
            crate::run_str(src).unwrap(),
            crate::run_str("(list 1 3 2 (list 3 4))").unwrap()
        );
    }

    #[test]
    fn list_display_and_error_messages_unchanged() {
        // Public rendering: `(1 2 3)`, nested, empty.
        assert_eq!(
            crate::run_str("(str (list 1 (list 2 3) 4))").unwrap(),
            Value::str("(1 (2 3) 4)")
        );
        assert_eq!(crate::run_str("(str (list))").unwrap(), Value::str("()"));
        // Error messages keep the same type names and wording.
        let err = crate::run_str("(first 1)").unwrap_err().to_string();
        assert!(err.contains("first expects list, got int"), "got: {err}");
        let err = crate::run_str("(cons 1 2)").unwrap_err().to_string();
        assert!(err.contains("cons expects (cons value list)"), "got: {err}");
        let err = crate::run_str("(rest 1)").unwrap_err().to_string();
        assert!(err.contains("rest expects list, got int"), "got: {err}");
        let err = crate::run_str("(len true)").unwrap_err().to_string();
        assert!(
            err.contains("len expects list, str, or hash, got bool"),
            "got: {err}"
        );
    }

    /// Performance regression guard: building a 20,000-element list by
    /// repeated `cons` must stay well under 500ms. The old `Rc<Vec<Value>>`
    /// representation made each `cons` an O(n) whole-list clone, so this
    /// program was O(n²) (~1.3s at 20k on the PO's machine); with O(1) cons
    /// cells it is O(n) and runs in single-digit milliseconds. The bound is
    /// deliberately generous so slow CI runners don't flake — it only trips
    /// if the representation regresses to quadratic behavior.
    #[test]
    fn building_a_20k_list_via_cons_stays_linear() {
        let src = "\
            (def n 20000)\
            (def i 0)\
            (def acc (list))\
            (while (< i n)\
              (def acc (cons i acc))\
              (def i (+ i 1)))\
            (len acc)\
            ";
        let start = std::time::Instant::now();
        let result = crate::run_str(src).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(result, Value::Int(20000));
        assert!(
            elapsed.as_millis() < 500,
            "20k cons build took {elapsed:?} — list builtins may have regressed to O(n²)"
        );
    }
}
