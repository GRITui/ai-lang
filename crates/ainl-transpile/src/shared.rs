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

    fn expr_all(&mut self, nodes: &[Node]) -> Result<Vec<String>> {
        nodes.iter().map(|n| self.expr(n)).collect()
    }
}

/// `(op a b c...)` -> `(a op b op c)`, with `identity` for the 0-arg case and
/// no parens for the 1-arg case. Used for `+`/`*` everywhere, and for
/// `and`/`or` on the two targets (JS, Ruby) with no native chained form —
/// the shape is identical either way.
pub(crate) fn infix<E: ExprEmit>(
    e: &mut E,
    args: &[Node],
    op: &str,
    identity: &str,
) -> Result<String> {
    let parts = e.expr_all(args)?;
    match parts.len() {
        0 => Ok(identity.to_string()),
        1 => Ok(parts.into_iter().next().unwrap()),
        _ => Ok(format!("({})", parts.join(&format!(" {op} ")))),
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
