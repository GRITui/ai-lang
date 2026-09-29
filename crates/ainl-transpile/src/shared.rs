//! Logic genuinely identical across all three transpiler backends, pulled out
//! once here instead of hand-copied per target. Each backend still owns its
//! own statement/dispatch structure and literal formatting (`if`/`let`/`fn`
//! lowering, string/float syntax, keyword lists) — that part actually differs
//! per language and doesn't belong here. What's here is the arithmetic/logic
//! glue and the `let`-binding parse that were byte-for-byte duplicated.

use ainl_core::parser::Node;
use ainl_core::{Error, Result};
use std::collections::BTreeSet;

/// Anything that can turn a single AINL expression node into source text.
/// Implemented once per backend (`Js`, `Py`, `Rb`); the free functions below
/// are generic over it so the arithmetic/logic glue that calls back into
/// `expr` is written exactly once instead of three times.
pub(crate) trait ExprEmit {
    fn expr(&mut self, node: &Node) -> Result<String>;

    /// Record that the emitted text uses the target's `_truthy` helper, so the
    /// backend's prologue emits its definition.
    ///
    /// [`logic`] and `expr_if` both route a condition through `_truthy` and are
    /// generic over this trait, so they cannot call a target-specific
    /// `need("_truthy")` themselves — the requirement has to be declared here
    /// and satisfied at each call site. Missing it produced a program that
    /// *compiled* and printed the right first line, then died with
    /// `ReferenceError: _truthy is not defined` on the first `(and …)`.
    fn need_truthy(&mut self);

    /// Evaluate `body` once with `name` bound to `val`, and return the text of
    /// the result — the target's own binding form (an arrow IIFE in JS, a
    /// lambda call in Python and Ruby).
    ///
    /// Exists because [`logic`] needs an operand's text in two places (the
    /// `_truthy` test and the value it yields) but may only *evaluate* it once.
    /// Spelled generically, so each target keeps its own binding idiom.
    fn bind_once(&mut self, name: &str, val: &str, body: &str) -> String;

    /// `cond ? a : b`, the target's own conditional spelling. JS and Ruby put
    /// the condition first, Python puts the value first — the one place the
    /// three dialects genuinely cannot share an expression with.
    fn cond(&mut self, cond: &str, then: &str, els: &str) -> String;

    /// A fresh name for an `and`/`or` operand binding that is guaranteed not to
    /// collide with anything the program itself bound.
    ///
    /// Guaranteed by the caller, which hands over the identifiers already in
    /// use (see [`used_symbols`]): the chain emitted for `and`/`or` is a nest of
    /// real host closures, so a temp that shadowed a user binding would silently
    /// change what the rest of the operand reads. Picking names by counting up
    /// is not enough on its own — an AINL identifier is any run of
    /// non-delimiter characters, so `_ainl_t0` is a perfectly legal thing for a
    /// program to `def` — so the used set is consulted rather than assumed.
    fn logic_temp(&mut self) -> String;

    /// The target's spelling of the `false` literal. `or`'s answer when no
    /// operand is truthy IS this value, and Python spells it `False` — hardcoding
    /// `false` here would have compiled to a `NameError` at run time on the
    /// exact case the identity exists to answer.
    fn false_lit(&self) -> &'static str;

    /// The target's spelling of the `true` literal, for the same reason as
    /// [`ExprEmit::false_lit`]: `(and)` answers this, and Python spells it
    /// `True`.
    fn true_lit(&self) -> &'static str;

    fn expr_all(&mut self, nodes: &[Node]) -> Result<Vec<String>> {
        nodes.iter().map(|n| self.expr(n)).collect()
    }
}

/// `(and a b c…)` / `(or a b c…)` -> a short-circuit on **AINL** truthiness.
///
/// NOT an infix chain on the host's `&&` / `||`, for two reasons:
///
/// * AINL says only `nil` and `false` are falsey, so `0` and `""` are TRUTHY.
///   Every host used here treats both as falsey, and a chain written with the
///   host operator would therefore stop early on values AINL considers true —
///   `(and 0 "x")` answers `"x"` here and `0` in the interpreter.
/// * The host operator returns an *operand*, not a boolean. So the answer is
///   not merely the wrong truthiness reading but a different VALUE: `(or 0 "")`
///   returns `0` under `||` and `""` in AINL.
///
/// So a chain is a genuine short-circuit over **values**, not a boolean fold:
/// AINL's `and` returns the first falsey *operand* and `or` the first truthy
/// *operand*, so a host operator — which returns a boolean — cannot express it
/// no matter how it is parenthesised.
///
/// The shape that does is a host conditional, because it is the one host
/// construct that yields a chosen *value*:
///
/// ```text
/// (or a b)  ->  _truthy(a) ? a : b
/// (and a b) ->  _truthy(a) ? b : a
/// ```
///
/// `and` puts the falsey operand in the `else` arm precisely because that arm
/// is the one that runs when `_truthy(a)` is false. Chained left to right, each
/// step testing the previous operand.
///
/// Two things this fold has to get right beyond that shape, both of which it
/// got wrong once and both of which cost an all-backend parity gate to find.
///
/// **The accumulator is an IDENTITY, not the last operand.** Seeding it with
/// `parts[last]` made the all-falsy `or` fall off the end of the chain and
/// answer the *last operand* where the interpreter answers `false`: `(or nil
/// nil)` printed `nil` on python3/node/ruby and `false` on the interpreter and
/// the AOT binary. `sf_or` returns `Value::Bool(false)` when no operand is
/// truthy, and the C emitter seeds `v_bool(0)` for the same reason, so the
/// documented answer is the `false` identity and the seed has to be that. The
/// same reasoning fixes `(or nil)`, where the single operand is falsey and the
/// fold has no conditional at all. `and`'s seed is `true` by the same argument
/// — it is the identity its chain can never fall below.
///
/// **Each operand's text appears at most once in the emitted program.** An
/// operand is needed twice: once for `_truthy(...)` and once for the value it
/// yields. Repeating the text evaluates it twice, so `(or (note "A") 1)`
/// printed `A` twice — a side effect the interpreter runs once, which is a
/// divergence no value-only test can see. Every non-final operand is therefore
/// bound through [`ExprEmit::bind_once`] and referred to by name after that,
/// so the host evaluates it exactly once and the `_truthy` test and the
/// yielded value read the same binding.
///
/// Binding per operand is also what keeps the chain LAZY. The operands are
/// bound nested, each one's body being the rest of the chain, so an operand
/// that the short-circuit skips is never entered at all — the same property
/// the interpreter gets from returning early.
///
/// `op` is the **host** operator (`&&` / `||`), because that is what has to be
/// emitted. The choice of identity and of final join is therefore made from the
/// emitted operators, never from the AINL form name: `"and"` never arrives here,
/// so an identity or join keyed on `op == "and"` would always take the `or`
/// branch and silently turn every `and` into an `or`.
pub(crate) fn logic<E: ExprEmit>(e: &mut E, args: &[Node], op: &str) -> Result<String> {
    let parts = e.expr_all(args)?;
    // `&&` is the only operator that can be falsey on its left, so it is also
    // the only one whose chain can propagate false upward.
    let is_and = op == "&&";
    match parts.len() {
        // The identities: `(and)` is `true` and `(or)` is `false`.
        0 => Ok(if is_and { e.true_lit() } else { e.false_lit() }.to_string()),
        1 => {
            let only = parts.into_iter().next().unwrap();
            if is_and {
                return Ok(only);
            }
            // A lone `or` operand is NOT always the answer: `sf_or` answers
            // `false` when the operand is falsey, and only yields the operand
            // when it is truthy. `(or nil)` is `false`, not `nil`.
            //
            // The operand's truthiness is a RUNTIME property, so it has to be
            // tested on the host rather than read off the text — `0` and `""`
            // are TRUTHY in AINL (§1) and must come back unchanged, and a
            // variable's value is not knowable here at all. So the single-operand
            // case is the same conditional as the general one, with the `false`
            // identity as its tail.
            e.need_truthy();
            let name = e.logic_temp();
            let t = format!("_truthy({name})");
            let body = e.cond(&t, &name, e.false_lit());
            Ok(e.bind_once(&name, &only, &body))
        }
        _ => {
            e.need_truthy();
            // Fold left, wrapping each operand around the chain that follows it.
            //
            // `or` SEEDS WITH `false`, not with the last operand. `sf_or` returns
            // `Value::Bool(false)` when no operand was truthy, and the C emitter
            // seeds `v_bool(0)` for the same reason, so the tail of an all-falsy
            // chain has to BE the `false` identity. Seeding it with the last
            // operand is what made `(or nil nil)` answer `nil` on python3/node/
            // ruby and `false` on the interpreter and the AOT binary.
            //
            // The consequence is that `or`'s LAST operand has to be tested too,
            // not just the earlier ones: with `false` as the tail there is
            // nothing else to catch it, and an untested truthy last operand
            // would be discarded. So `or` wraps every operand, the last
            // included, while `and` emits its last operand bare and unrepeated —
            // the chain already ends on it.
            let (seed, end) = if is_and {
                (parts[parts.len() - 1].clone(), parts.len() - 1)
            } else {
                (e.false_lit().to_string(), parts.len())
            };
            let mut acc = seed;
            // Walk RIGHT to LEFT, so the FIRST operand ends up outermost — it is
            // the one that gets tested first, and the whole chain is that test
            // plus everything it guards. Folding left to right would bury the
            // first operand in the middle and test the last one first, which
            // evaluates operands in the wrong order and breaks short-circuiting.
            for p in parts[..end].iter().rev() {
                let name = e.logic_temp();
                // `and`: truthy carries on down the chain, falsey IS the answer.
                // `or`:  truthy IS the answer, falsey carries on down the chain.
                let t = format!("_truthy({name})");
                let body = if is_and {
                    e.cond(&t, &acc, &name)
                } else {
                    e.cond(&t, &name, &acc)
                };
                // Bind the operand and use only its NAME below. Its text used to
                // appear twice — once inside `_truthy`, once as the yielded value
                // — and so ran twice: `(or (note "A") 1)` printed `A` twice, a
                // side effect the interpreter runs once. The name appears twice
                // in the emitted text and evaluates once.
                acc = e.bind_once(&name, p, &body);
            }
            Ok(acc)
        }
    }
}

/// Every symbol the program mentions, as the *host* would spell it.
///
/// Handed to a backend so `logic_temp` can hand back a name that provably
/// collides with nothing. A blind counter is not enough: AINL identifiers are
/// any run of non-delimiter characters, so `_ainl_t$0` is a perfectly legal
/// thing for a program to `def`, and `sanitize` deliberately preserves `$`. The
/// chain emitted for `and`/`or` is a nest of real host closures, so a temp that
/// shadowed a user binding would change what the rest of the operand reads.
///
/// Over-approximating is safe and under-approximating is not, so this collects
/// *all* symbols, not just the ones in binding position: a name that only
/// appears in expression position is still a name the program chose, and
/// skipping it costs one counter increment while colliding with it costs
/// correctness.
pub(crate) fn used_symbols(forms: &[Node], sanitize: impl Fn(&str) -> String) -> BTreeSet<String> {
    fn walk(n: &Node, out: &mut BTreeSet<String>, sanitize: &impl Fn(&str) -> String) {
        match n {
            Node::Sym(s, _) => {
                out.insert(sanitize(s));
            }
            Node::List(items, _) => {
                for i in items {
                    walk(i, out, sanitize);
                }
            }
            _ => {}
        }
    }
    let mut out = BTreeSet::new();
    for f in forms {
        walk(f, &mut out, &sanitize);
    }
    out
}

/// Fixed-arity 1-argument prefix operator (`not`).
pub(crate) fn unary<E: ExprEmit>(e: &mut E, args: &[Node], op: &str) -> Result<String> {
    let [a] = args else {
        return Err(Error::runtime("expects 1 argument"));
    };
    Ok(format!("({op}{})", e.expr(a)?))
}

/// Neither JS nor Ruby has chained comparison, so expand `(< a b c)` to
/// `(a < b && b < c)`. (Python has native chained comparison and doesn't use
/// this — see `python::chain`.)
pub(crate) fn cmp<E: ExprEmit>(e: &mut E, args: &[Node], op: &str) -> Result<String> {
    if args.len() < 2 {
        return Ok("true".to_string());
    }
    let parts = e.expr_all(args)?;
    let clauses: Vec<String> = parts
        .windows(2)
        .map(|w| format!("{} {op} {}", w[0], w[1]))
        .collect();
    Ok(format!("({})", clauses.join(" && ")))
}

/// `((n v)...)` binding list shared by `let` across every target.
pub(crate) fn let_bindings(binds_node: &Node) -> Result<Vec<(&str, &Node)>> {
    let Node::List(binds, _) = binds_node else {
        return Err(Error::runtime("let bindings must be a list"));
    };
    let mut out = Vec::new();
    for b in binds {
        let Node::List(pair, _) = b else {
            return Err(Error::runtime("each let binding must be (name value)"));
        };
        let [Node::Sym(name, _), val] = &pair[..] else {
            return Err(Error::runtime("each let binding must be (name value)"));
        };
        out.push((name.as_str(), val));
    }
    Ok(out)
}

/// Parse `(p1 p2 ... [& rest])` into sanitized parameter names, the last one
/// prefixed by `rest_prefix` if a `& rest` was present. Each target spells
/// its rest-parameter marker differently (`*` in Python/Ruby, `...` in JS)
/// and sanitizes identifiers differently, so both are passed in.
pub(crate) fn parse_params(
    params_node: &Node,
    rest_prefix: &str,
    sanitize: impl Fn(&str) -> String,
) -> Result<Vec<String>> {
    let Node::List(param_nodes, _) = params_node else {
        return Err(Error::runtime("fn params must be a list"));
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < param_nodes.len() {
        let Node::Sym(p, _) = &param_nodes[i] else {
            return Err(Error::runtime("fn params must be symbols"));
        };
        if p == "&" {
            let Some(Node::Sym(rest, _)) = param_nodes.get(i + 1) else {
                return Err(Error::runtime("'&' must be followed by a rest parameter"));
            };
            out.push(format!("{rest_prefix}{}", sanitize(rest)));
            break;
        }
        out.push(sanitize(p));
        i += 1;
    }
    Ok(out)
}
