//! The `map` / `filter` / `reduce` special forms, lowered to the `while` loops
//! they semantically are.
//!
//! ## Why a desugar, and why here
//!
//! These three cannot be `Value::Builtin`s (see `collections` for the full
//! argument): a builtin is a bare `fn(&[Value]) -> Result<Value>` with no
//! environment, so it cannot *call* a `Value::Closure` it was handed. But the
//! language already has every piece needed to express them — a `while` loop, a
//! list built by `push`, a function value called as `(f x)`, an accumulator
//! rebound by `def` — so the backends do not each need a new higher-order
//! primitive. They need the *same* loop.
//!
//! So each form is rewritten, once, in [`rewrite`], into `def` + `while` over
//! primitives that already behave identically on all six evaluators
//! (interpreter, VM, AOT C, Python, JS, Ruby). Everything downstream — the
//! tree-walk, the VM's compiler, the C codegen, the three emitters — then
//! handles the rewritten form through the path it already has.
//!
//! That is a deliberate trade. Teaching each backend its own higher-order
//! builtin means four places to keep the same loop semantics aligned, and the
//! failure mode is a *silent* divergence (a program that filters correctly on
//! the VM and not in C). Here the loop is written once and the six backends
//! cannot disagree about it, because they never see it differently.
//!
//! ## The loop, and the two rules it depends on
//!
//! AINL has no mutation, so the accumulator is a `def`-bound local rewritten on
//! each iteration:
//!
//! ```text
//! (def out ())
//! (def cur lst)
//! (while (not (= (first cur) nil))
//!   (def out (push out (f (first cur))))
//!   (def cur (rest cur)))
//! out
//! ```
//!
//! Two properties of the language make this exact rather than approximate, and
//! both were verified against all five evaluators before being relied on:
//!
//! 1. **The empty-list test is `(= (first cur) nil)`.** `(first ())` is `nil` by
//!    definition and `nil` is the one value an element can never be, so this is
//!    a termination test with no off-by-one and no separate length check.
//! 2. **The accumulators are `def`s in the enclosing function's scope, not
//!    `let` bindings.** A `let` body is a *different* scope: a `def` inside one
//!    binds a second slot that shadows the `let` binding, so reading the name
//!    back in the same body sees `nil` rather than the value just stored
//!    (`(let ((a 1)) (def a 2) a)` is a trap, not a reassignment). Plain `def`s
//!    in the function body rebind in place, which is what the loop needs.
//!
//! ## Scoping
//!
//! The generated names are `_ainl_`-prefixed so they cannot capture a program
//! variable, and they are the only names the expansion binds. `let`, `fn` and
//! `try` bodies are not descended into for *rebinding* purposes — a `def` in a
//! generated loop lives in whatever scope the `map` form itself was written in,
//! which is exactly where a hand-written loop would have put it.
//!
//! ## What is *not* desugared
//!
//! `sort` is a real builtin in every backend. It has no `fn` in the
//! no-comparator form, and in the comparator form the comparator is just a
//! function value each backend already knows how to call. It is implemented
//! natively so a common operation does not expand into a bubble sort, and so
//! its stability is a stated property of each backend's code rather than an
//! emergent consequence of a generated loop.

use crate::parser::{Node, Span};

/// Generated locals, prefixed so they cannot capture a program variable.
const OUT: &str = "_ainl_out";
const ACC: &str = "_ainl_acc";
const CUR: &str = "_ainl_cur";
/// The function operand, a parameter of the generated helpers.
const FNAME: &str = "_ainl_f";
/// The list operand, a parameter of the generated helpers.
const LST: &str = "_ainl_lst";
/// The three generated helpers, bound once by [`prelude`].
const MAP_HELPER: &str = "_ainl_map";
const FILTER_HELPER: &str = "_ainl_filter";
const REDUCE_HELPER: &str = "_ainl_reduce";
/// The initial accumulator, passed to the `reduce` helper alongside the operands.
const REDUCE_INIT: &str = "_ainl_init";

/// True when `op` heads one of the three collection special forms.
pub fn is_collection_form(op: &str) -> bool {
    matches!(op, "map" | "filter" | "reduce")
}

/// Rewrite `node` if it is a well-formed `map` / `filter` / `reduce` form.
///
/// `Some(expansion)` when the node was one of the three and its shape was
/// right; `None` for every other node, **and** for a malformed one — a wrong
/// arity is left alone so the ordinary dispatch reports it with its own wording
/// and span instead of this module inventing a second, different message.
///
/// This is the single entry point every backend calls, which is the point: the
/// loop is written once, so the six evaluators cannot drift apart on it.
///
/// The result is lowered once more, so a collection form nested inside another
/// one's arguments (a `map` over a `map`) is rewritten too.
pub fn rewrite(node: &Node) -> Option<Node> {
    let Node::List(items, span) = node else {
        return None;
    };
    let Some(Node::Sym(op, _)) = items.first() else {
        return None;
    };
    if !is_collection_form(op) {
        return None;
    }
    let expanded = expand(op, &items[1..], *span).ok()?;
    Some(lower_nested(&expanded))
}

/// Lower every collection form in a tree of top-level forms, prefixing the
/// generated helpers when any is used.
///
/// The helpers are bound once, here, rather than at each use — see [`prelude`]
/// for why they cannot be inlined into expression position.
///
/// The VM needs the whole program lowered *before* `collect_defs` runs: the
/// helpers are `def`-bound, so their local slots have to be reserved in the same
/// pass as everything else, which means this cannot be done form-by-form as
/// `compile_expr` walks the tree. The tree-walk does not need it (it keeps no
/// slot table), but it calls the same `rewrite`, so both evaluators run the same
/// expansion of the same source.
///
/// A program that uses no collection form is returned **unchanged** — no
/// prelude, no generated names in its output. That keeps `ainl ast` and the
/// AOT / transpile output byte-identical to before for every existing program.
pub fn lower(forms: &[Node]) -> Vec<Node> {
    // Check the **original** forms, not `out`. A well-formed collection form has
    // already been rewritten into a call by this point, so scanning `out` for
    // one finds nothing and the helpers never get bound — the use then fails with
    // "unbound symbol '_ainl_map'". The malformed case (wrong arity, left
    // unrewritten) is the reason the check cannot be "did `rewrite` fire?": a
    // program that *uses* a collection form must get the prelude either way, or
    // the real arity error would be buried under an unbound-helper error.
    let uses_collection = forms.iter().any(needs_prelude);
    let mut out: Vec<Node> = forms
        .iter()
        .map(|n| match rewrite(n) {
            Some(r) => r,
            None => lower_nested(n),
        })
        .collect();
    if uses_collection {
        let span = forms.first().map(|f| f.span()).unwrap_or(Span::new(0, 0));
        let mut all = prelude(span);
        all.append(&mut out);
        return all;
    }
    out
}

/// True when `node` contains a collection form anywhere inside it.
///
/// Deliberately conservative in one direction only: a `map` whose arity is
/// wrong still counts, because leaving the prelude out for a program that *does*
/// use a collection form would leave the malformed call referring to an unbound
/// helper name — a second, misleading error on top of the real one.
fn needs_prelude(node: &Node) -> bool {
    let Node::List(items, _) = node else {
        return false;
    };
    if let Some(Node::Sym(op, _)) = items.first() {
        if is_collection_form(op) {
            return true;
        }
    }
    items.iter().any(needs_prelude)
}

/// Rewrite any collection form inside a freshly built expansion.
///
/// The expansion's own shape is generated, so this only has to look at the two
/// subtrees that came from the user's source: the function operand and the list
/// operand.
fn lower_nested(node: &Node) -> Node {
    let Node::List(items, span) = node else {
        return node.clone();
    };
    Node::List(
        items
            .iter()
            .map(|n| match rewrite(n) {
                Some(r) => r,
                None => lower_nested(n),
            })
            .collect(),
        *span,
    )
}

// ---------------------------------------------------------------------------
// expansion
// ---------------------------------------------------------------------------

/// Build the expansion for one well-formed form. `Err(())` means "not
/// well-formed" — see [`rewrite`] for why that is not an error here.
fn expand(op: &str, args: &[Node], span: Span) -> Result<Node, ()> {
    match op {
        "map" | "filter" => {
            let [f, lst] = args else { return Err(()) };
            let helper = if op == "map" {
                MAP_HELPER
            } else {
                FILTER_HELPER
            };
            Ok(call2(helper, f.clone(), lst.clone(), span))
        }
        "reduce" => {
            let [f, init, lst] = args else { return Err(()) };
            // Argument order must match the helper's parameter list
            // `(_ainl_f _ainl_lst _ainl_init)`: callback, **list**, then init.
            // The source order is `(reduce f init lst)`, so `init` and `lst` are
            // swapped on the way in. Getting this backwards is not a type error
            // — the helper is variadic-free but happily binds the int `0` to
            // `_ainl_lst` — and it surfaced much later as "first expects list,
            // got int" from inside the loop, with the wrong name in the message.
            Ok(call3(
                REDUCE_HELPER,
                f.clone(),
                lst.clone(),
                init.clone(),
                span,
            ))
        }
        _ => Err(()),
    }
}

/// `(def cur lst)` — bind the list being walked to the loop cursor.
fn step(span: Span) -> Node {
    def(
        span,
        CUR,
        Node::List(vec![sym("rest"), ref_(CUR)], Span::new(0, 0)),
    )
}

/// The three loop helpers, bound once at the top of a program that uses any
/// collection form.
///
/// **Why the helpers are hoisted instead of inlined at each use.** A `map` is an
/// *expression* — it appears in `(def doubled (map …))` and inside
/// `(print (map …))` — and AINL's expression position is single-statement. That
/// rules out, one by one, every shape the loop could otherwise take:
///
/// * a bare `do` — all three transpilers reject it outright ("cannot
///   transpile `multi-statement do` in expression position").
/// * a multi-statement `let` — same rejection, worded for `let`.
/// * an inline `(fn (f lst) …)`, applied to the operands — the transpilers emit
///   an inline `fn` as a host *lambda*, and all three refuse a multi-statement
///   body there ("fn with a multi-statement body cannot be a Python lambda …
///   bind it with def"). The VM and the tree-walk accept it happily, so this
///   shape is a silent cross-backend divergence: it works on the interpreter
///   and fails only once the program is transpiled.
///
/// Binding each helper once with `def` — statement position, where a
/// multi-statement `fn` is legal everywhere — leaves the *use site* a plain
/// function call. That is an expression on every backend, and it is why the
/// expansion is one shared prelude plus a one-line call rather than a loop
/// repeated per use.
///
/// The helpers are ordinary generated top-level `def`s, so the VM reserves
/// their slots in the same `collect_defs` pass as everything else, and the
/// transpilers emit them as ordinary top-level definitions.
pub fn prelude(span: Span) -> Vec<Node> {
    vec![
        def(
            span,
            MAP_HELPER,
            helper_fn(&["_ainl_f", LST], map_body(span), span),
        ),
        def(
            span,
            FILTER_HELPER,
            helper_fn(&["_ainl_f", LST], filter_body(span), span),
        ),
        def(
            span,
            REDUCE_HELPER,
            helper_fn(&[FNAME, LST, REDUCE_INIT], reduce_body(span), span),
        ),
    ]
}

/// `(fn (params…) prelude… while… result)` — the wrapper the three helper
/// bodies share.
///
/// The body forms are **siblings** of the parameter list, not one nested list:
/// `fn` is variadic in its body, so a wrapped list would become a single body
/// form that *is* a list literal — the function would evaluate that list and
/// return it, silently discarding the loop.
fn helper_fn(params: &[&str], body: Vec<Node>, span: Span) -> Node {
    let mut items = vec![
        sym("fn"),
        Node::List(
            params
                .iter()
                .map(|p| Node::Sym((*p).to_string(), span))
                .collect(),
            span,
        ),
    ];
    items.extend(body);
    list(span, items)
}

/// `(while (not (= (len _ainl_cur) 0)) form …)` — the loop shell shared by
/// `map` and `filter`.
///
/// **The termination test is `(= (len cur) 0)`, not `(= (first cur) nil)`.** The
/// `nil` test looks equivalent and is not: `nil` is a perfectly good *element*
/// of a list, so `(map (fn (x) x) (list 1 nil 2))` returned `(1)` — the loop
/// stopped at the `nil` instead of walking past it. AINL has no `nil?` or
/// `empty?`, and `(= (first ()) (first (list 1)))` is `false`, so neither the
/// obvious predicate nor a head comparison separates "empty" from "holds nil".
/// `len` does: 0 for the empty list, 1 for a one-element list whose element is
/// `nil`, and N for any other. One builtin call per step, and it is exact for
/// every value a list can hold.
///
/// This is why the first version of this loop — which is what the early
/// handwritten probes used — looked fine on numeric lists and only lost
/// elements once a test put a `nil` in one.
///
/// The body forms are **siblings** of the condition, not a nested list:
/// `sf_while` splits the first argument off as the condition and treats the
/// rest as the body, so wrapping them would make the loop evaluate a list
/// literal instead of running the `def`s.
fn loop_with(per_element: Node, span: Span) -> Node {
    let mut items = vec![sym("while"), more(span)];
    items.push(def(span, OUT, per_element));
    items.push(step(span));
    list(span, items)
}

/// `(> (len _ainl_cur) 0)` — "there is still an element".
///
/// See [`loop_with`] for why this is not `(= (first cur) nil)`.
fn more(span: Span) -> Node {
    Node::List(
        vec![
            sym(">"),
            Node::List(vec![sym("len"), ref_(CUR)], span),
            Node::Int(0, span),
        ],
        span,
    )
}

/// `map`'s helper: push the transformed element.
fn map_body(span: Span) -> Vec<Node> {
    vec![
        def(span, OUT, node_list(span)),
        def(span, CUR, ref_(LST)),
        loop_with(push(OUT, apply1(span)), span),
        ref_(OUT),
    ]
}

/// `filter`'s helper: push the element itself when the predicate is truthy, and
/// leave the accumulator alone otherwise.
///
/// Written as `(if (p x) (push out x) out)` rather than an `if` with an empty
/// branch, because AINL's `if` is an expression with no null statement.
fn filter_body(span: Span) -> Vec<Node> {
    vec![
        def(span, OUT, node_list(span)),
        def(span, CUR, ref_(LST)),
        loop_with(if_(span, apply1(span), push(OUT, first()), ref_(OUT)), span),
        ref_(OUT),
    ]
}

/// `reduce`'s helper: fold accumulator-first, so the callback sees
/// `(acc x)`.
fn reduce_body(span: Span) -> Vec<Node> {
    let mut items = vec![sym("while"), more(span)];
    items.push(def(span, ACC, apply2(span)));
    items.push(step(span));
    vec![
        def(span, ACC, ref_(REDUCE_INIT)),
        def(span, CUR, ref_(LST)),
        list(span, items),
        ref_(ACC),
    ]
}

/// `(_ainl_f x)` — call the function operand on one element.
///
/// The operand arrives as the helper's own parameter and is called by that
/// name, rather than being spliced into the callee position. The obvious
/// alternative is wrong, and silently so: `fn` is variadic in its *body*, so
/// appending an argument to a `fn` form produced a function with one more body
/// form, applied to no arguments — failing at run time as "cannot call a sym",
/// far from the cause.
///
/// Calling by name is also the more honest reading: it says the operand is
/// *called* once per element, which is what `map` means.
fn apply1(span: Span) -> Node {
    Node::List(vec![Node::Sym(FNAME.to_string(), span), first()], span)
}

/// `(_ainl_f acc x)` — the two-argument, accumulator-first call `reduce` folds
/// with.
fn apply2(span: Span) -> Node {
    Node::List(
        vec![Node::Sym(FNAME.to_string(), span), ref_(ACC), first()],
        span,
    )
}

/// `(_ainl_map f lst)`.
fn call2(helper: &str, a: Node, b: Node, span: Span) -> Node {
    Node::List(vec![Node::Sym(helper.to_string(), span), a, b], span)
}

/// `(_ainl_reduce f init lst)`.
fn call3(helper: &str, a: Node, b: Node, c: Node, span: Span) -> Node {
    Node::List(vec![Node::Sym(helper.to_string(), span), a, b, c], span)
}

// ---------------------------------------------------------------------------
// AST constructors
//
// Each carries the span of the `map` form the user wrote, so a diagnostic
// raised inside the generated code points at the source the reader wrote, not
// at a synthetic node with no position.
// ---------------------------------------------------------------------------

fn sym(s: &str) -> Node {
    Node::Sym(s.to_string(), Span::new(0, 0))
}

fn nil(span: Span) -> Node {
    Node::Sym("nil".to_string(), span)
}

fn first() -> Node {
    Node::List(vec![sym("first"), ref_(CUR)], Span::new(0, 0))
}

fn ref_(name: &str) -> Node {
    Node::Sym(name.to_string(), Span::new(0, 0))
}

fn list(span: Span, items: Vec<Node>) -> Node {
    Node::List(items, span)
}

fn def(span: Span, name: &str, value: Node) -> Node {
    list(
        span,
        vec![sym("def"), Node::Sym(name.to_string(), span), value],
    )
}

fn if_(span: Span, cond: Node, then: Node, els: Node) -> Node {
    list(span, vec![sym("if"), cond, then, els])
}

/// `()` — the empty list, written as a call to the `list` builtin with no
/// arguments.
///
/// **Not** `Node::List(vec![], …)`: that is the AST for the literal `()`, which
/// the evaluator reads as nil (an empty list node evaluates to nil — see
/// `eval::eval_list`), and which `push` would then reject as "not a list". The
/// empty *list value* is `(list)`, a one-element list node whose head is the
/// symbol `list`.
fn node_list(span: Span) -> Node {
    Node::List(vec![sym("list")], span)
}

/// `(push <accumulator> value)` — a plain builtin call.
fn push(accumulator: &str, value: Node) -> Node {
    Node::List(vec![sym("push"), ref_(accumulator), value], Span::new(0, 0))
}
