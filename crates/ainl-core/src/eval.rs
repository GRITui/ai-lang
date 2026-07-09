//! Environment + tree-walking evaluator.

use crate::error::{Error, Result};
use crate::parser::Node;
use crate::value::{Closure, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// A lexical scope with an optional parent. `Env` is a cheap `Rc` handle so
/// closures can share and outlive the scope that created them.
#[derive(Clone)]
pub struct Env(Rc<Scope>);

struct Scope {
    vars: RefCell<HashMap<String, Value>>,
    parent: Option<Env>,
}

impl Env {
    pub fn new() -> Env {
        Env(Rc::new(Scope {
            vars: RefCell::new(HashMap::new()),
            parent: None,
        }))
    }

    pub fn child(&self) -> Env {
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
}

impl Default for Env {
    fn default() -> Self {
        Env::new()
    }
}

pub fn eval(node: &Node, env: &Env) -> Result<Value> {
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
                call_env.define(rest.clone(), Value::List(Rc::new(extra)));
            }
            let mut last = Value::Nil;
            for form in &clos.body {
                last = eval(form, &call_env)?;
            }
            Ok(last)
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
    for b in binds {
        let Node::List(pair, _) = b else {
            return Err(Error::runtime("each let binding must be (name value)"));
        };
        let [Node::Sym(name, _), val_node] = &pair[..] else {
            return Err(Error::runtime("each let binding must be (name value)"));
        };
        let val = eval(val_node, &scope)?;
        scope.define(name.clone(), val);
    }
    let mut last = Value::Nil;
    for form in body {
        last = eval(form, &scope)?;
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
        Node::List(items, _) => Value::List(Rc::new(items.iter().map(quote_node).collect())),
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

    b!("list", |a| Ok(Value::List(Rc::new(a.to_vec()))));
    b!("len", builtin_len);
    b!("first", builtin_first);
    b!("rest", builtin_rest);
    b!("nth", builtin_nth);
    b!("cons", builtin_cons);
    b!("push", builtin_push);
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
            Value::Int(i) => Ok(Value::Int(-*i)),
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
        Value::List(l) => Ok(Value::Int(l.len() as i64)),
        Value::Str(s) => Ok(Value::Int(s.chars().count() as i64)),
        other => Err(Error::runtime(format!(
            "len expects list or str, got {}",
            other.type_name()
        ))),
    }
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
        Value::List(l) => {
            let rest: Vec<Value> = l.iter().skip(1).cloned().collect();
            Ok(Value::List(Rc::new(rest)))
        }
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
    Ok(l.get(*i as usize).cloned().unwrap_or(Value::Nil))
}

fn builtin_cons(args: &[Value]) -> Result<Value> {
    let [head, Value::List(l)] = args else {
        return Err(Error::runtime("cons expects (cons value list)"));
    };
    let mut out = Vec::with_capacity(l.len() + 1);
    out.push(head.clone());
    out.extend(l.iter().cloned());
    Ok(Value::List(Rc::new(out)))
}

fn builtin_push(args: &[Value]) -> Result<Value> {
    let [Value::List(l), tail @ ..] = args else {
        return Err(Error::runtime("push expects (push list value...)"));
    };
    let mut out: Vec<Value> = l.iter().cloned().collect();
    out.extend(tail.iter().cloned());
    Ok(Value::List(Rc::new(out)))
}
