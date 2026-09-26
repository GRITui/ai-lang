//! JSON → AST round-trip tests (C4).
//!
//! For each program: `parse(src) == json_to_forms(&parse_to_json(src, ..))` —
//! structural equality INCLUDING every node's byte `Span` (and therefore the
//! `loc` derived from it). Also asserts the deserialized AST evaluates to the
//! same value as the original, and that re-serializing is byte-stable.

use ainl_core::{json_to_forms, parse, parse_to_json, Env};

/// The full round-trip assertion: parse → JSON → parse back, spans included.
fn assert_round_trip(src: &str) {
    let original = parse(src).unwrap_or_else(|e| panic!("parse failed for {src:?}: {e}"));
    let json = parse_to_json(src, Some("t.ainl")).unwrap();
    let back = json_to_forms(&json)
        .unwrap_or_else(|e| panic!("json_to_forms failed for {src:?} (json: {json}): {e}"));
    assert_eq!(back, original, "round trip mismatch for {src:?}");

    // The recovered AST must also be behaviorally identical: evaluating the
    // deserialized forms in a fresh prelude env yields the same final value
    // (or the same runtime error — e.g. for `(error "boom")`).
    let v1 = eval_program(&original);
    let v2 = eval_program(&back);
    assert_eq!(v1, v2, "eval mismatch for {src:?}");

    // Re-serializing the recovered tree is byte-stable (fixpoint).
    let again = ainl_core::forms_to_json(&back, src, Some("t.ainl"));
    assert_eq!(again, json, "re-serialization not stable for {src:?}");
}

fn eval_program(forms: &[ainl_core::Node]) -> Result<ainl_core::Value, String> {
    let env = Env::with_prelude();
    let mut last = ainl_core::Value::Nil;
    for form in forms {
        last = match ainl_core::eval::eval(form, &env) {
            Ok(v) => v,
            Err(e) => return Err(e.to_string()),
        };
    }
    Ok(last)
}

/// Every file in examples/ must round-trip.
#[test]
fn all_examples_round_trip() {
    let examples = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("examples");
    let mut n = 0;
    for entry in std::fs::read_dir(&examples).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|s| s.to_str()) != Some("ainl") {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        assert_round_trip(&src);
        n += 1;
    }
    assert!(n >= 4, "expected at least 4 example files, found {n}");
}

/// A program exercising every node type and every builtin's call shape:
/// atoms (ints, floats, strings with escapes, symbols, bools, nil), lists,
/// and each builtin from the prelude.
#[test]
fn every_node_type_and_builtin_round_trips() {
    let src = r#"
; atoms: int, float, string (escapes), symbol, bools, nil
-42
3.14
1e3
2.5E-2
"hello \"quoted\" world"
"tab\there"
"back\\slash"
"line1
line2"
some-symbol
true
false
nil
; lists: empty, nested, mixed
()
(1 2 3)
((a b) (c (d e)) 7 "x")
; every builtin's call shape
(+ 1 2 3)
(* 2 3 4)
(- 10 4)
(- 5)
(/ 7 2)
(= 1 1.0)
(< 1 2 3)
(> 3 2)
(<= 2 2)
(>= 3 3)
(not nil)
(mod 10 3)
(print "hi")
(str 1 2.5 "x")
(list)
(list 1 "two" (list 3))
(len (list 1 2 3))
(first (list 1 2))
(rest (list 1 2 3))
(nth (list 10 20 30) 1)
(cons 0 (list 1))
(push (list 1) 2)
(hash "name" "Ada" "age" 36)
(hash)
(get (hash "k" 1) "k")
(assoc (hash "k" 1) "k" 2)
(has (hash "k" 1) "k")
(keys (hash "a" 1 "b" 2))
(vals (hash "a" 1))
(error "boom")
; special forms + user fns (def/fn/let/if/while/quote/and/or/do)
(def sq (fn (x) (* x x)))
(def pick (fn (x & rest) (if x x (first rest))))
(def acc (fn (n)
  (let ((i 0) (s 0))
    (while (< i n)
      (def i (+ i 1))
      (def s (+ s i)))
    s)))
(sq 6)
(pick nil 9 8)
(acc 4)
(quote (a b "c" 3.5))
(and 1 (list 2) "ok")
(or nil false 42)
(do 1 2 3)
"#;
    assert_round_trip(src);
}

/// Deep nesting (near the parser's 512 bound) must survive the round trip.
#[test]
fn deep_nesting_round_trips() {
    let depth = 500;
    let src = format!("{}1{}", "(".repeat(depth), ")".repeat(depth));
    assert_round_trip(&src);
}

/// A document with no `source` field (serializer omits it when None) must
/// still deserialize.
#[test]
fn json_without_source_field() {
    let src = "(+ 1 2)";
    let json = parse_to_json(src, None).unwrap();
    assert!(!json.contains("\"source\""));
    let back = json_to_forms(&json).unwrap();
    assert_eq!(back, parse(src).unwrap());
}

/// Malformed documents must be rejected, not silently mis-parsed.
#[test]
fn malformed_json_is_rejected() {
    let bad = [
        "", // empty input
        "null", // top-level not an object
        "[]",
        "{\"forms\": []}", // missing version
        "{\"version\": \"9.9\", \"forms\": []}", // unsupported version
        "{\"version\": 0.1, \"forms\": []}", // version not a string
        "{\"version\": \"0.1\"}", // missing forms
        "{\"version\": \"0.1\", \"forms\": {}}", // forms not an array
        "{\"version\": \"0.1\", \"bogus\": 1, \"forms\": []}", // unknown top-level field
        // node-level problems
        "{\"version\": \"0.1\", \"forms\": [42]}", // node not an object
        "{\"version\": \"0.1\", \"forms\": [{\"t\": \"int\"}]}", // missing span
        "{\"version\": \"0.1\", \"forms\": [{\"t\": \"int\", \"span\": [0, 1]}]}", // missing v
        "{\"version\": \"0.1\", \"forms\": [{\"t\": \"int\", \"span\": [0, 1], \"v\": \"x\"}]}", // v wrong type
        "{\"version\": \"0.1\", \"forms\": [{\"t\": \"float\", \"span\": [0, 1], \"v\": \"1.5\"}]}", // v wrong type
        "{\"version\": \"0.1\", \"forms\": [{\"t\": \"str\", \"span\": [0, 1], \"v\": 1}]}", // v wrong type
        "{\"version\": \"0.1\", \"forms\": [{\"t\": \"blob\", \"span\": [0, 1], \"v\": 1}]}", // unknown node type
        "{\"version\": \"0.1\", \"forms\": [{\"t\": \"list\", \"span\": [0, 1], \"v\": 1}]}", // list needs items
        "{\"version\": \"0.1\", \"forms\": [{\"t\": \"list\", \"span\": [0, 1], \"items\": 1}]}", // items not array
        "{\"version\": \"0.1\", \"forms\": [{\"t\": \"int\", \"span\": [0], \"v\": 1}]}", // span wrong arity
        "{\"version\": \"0.1\", \"forms\": [{\"t\": \"int\", \"span\": [-1, 1], \"v\": 1}]}", // negative span
        "{\"version\": \"0.1\", \"forms\": [{\"t\": \"int\", \"span\": [0, 1], \"v\": 1, \"extra\": 2}]}", // unknown node field
        "{\"version\": \"0.1\", \"forms\": [{\"t\": \"int\", \"span\": [0, 1], \"v\": 1}]} trailing", // trailing data
        "{\"version\": \"0.1\", \"forms\": [", // truncated
        "{\"version\": \"0.1\", \"forms\": [{\"t\": \"int\", \"span\": [0, 1], \"v\": 01}]}", // leading-zero int (invalid JSON)
        "{\"version\": \"0.1\", \"forms\": [{\"t\": \"int\", \"span\": [0, 1], \"v\": 1e}]}", // bad exponent
    ];
    for (i, doc) in bad.iter().enumerate() {
        assert!(
            json_to_forms(doc).is_err(),
            "case {i} should be rejected: {doc:?}"
        );
    }
}

/// Every JSON escape the serializer can emit must come back unescaped.
#[test]
fn string_escapes_round_trip() {
    let src = r#""a\"b\\c\/d\ne\tf\rg""#;
    assert_round_trip(src);
}

/// A float that is a whole number keeps its float-ness through the round trip
/// (the serializer emits `3.0` for whole floats, so the JSON number carries a
/// fractional part and is read back as a Float).
#[test]
fn whole_float_stays_float() {
    let src = "(+ 1.0 2.0)";
    let original = parse(src).unwrap();
    let json = parse_to_json(src, None).unwrap();
    let back = json_to_forms(&json).unwrap();
    assert_eq!(back, original);
    match &back[0] {
        ainl_core::Node::List(items, _) => match &items[1] {
            ainl_core::Node::Float(x, _) => assert_eq!(*x, 1.0),
            other => panic!("expected Float, got {other:?}"),
        },
        other => panic!("expected List, got {other:?}"),
    }
}
