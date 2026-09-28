//! Logic genuinely identical across all three transpiler backends, pulled out
//! once here instead of hand-copied per target. Each backend still owns its
//! own statement/dispatch structure and literal formatting (`if`/`let`/`fn`
//! lowering, string/float syntax, keyword lists) — that part actually differs
//! per language and doesn't belong here. What's here is the arithmetic/logic
//! glue and the `let`-binding parse that were byte-for-byte duplicated.

use ainl_core::parser::Node;
use ainl_core::{Error, Result};

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
        0 => Ok(if is_and { "true" } else { "false" }.to_string()),
        1 => Ok(parts.into_iter().next().unwrap()),
        _ => {
            e.need_truthy();
            // Fold right: `acc` starts at the last operand, and each earlier
            // one becomes a conditional against it. The last operand is never
            // re-tested, which keeps its side effects to exactly one run.
            let mut acc = parts[parts.len() - 1].clone();
            for p in parts[..parts.len() - 1].iter().rev() {
                acc = if is_and {
                    format!("(_truthy({p}) ? {acc} : {p})")
                } else {
                    format!("(_truthy({p}) ? {p} : {acc})")
                };
            }
            Ok(acc)
        }
    }
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
