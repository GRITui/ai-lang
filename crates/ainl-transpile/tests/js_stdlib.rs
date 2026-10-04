//! JavaScript projection of the Stage 3.1 stdlib.
//!
//! `scripts/check-transpile.sh` verifies the behavior end-to-end on
//! `examples/stdlib.ainl`. These tests cover the mapping itself, and in
//! particular JS's coercions — the reason each helper type-checks rather than
//! leaning on the host: `[] + 1 === "1"`, `"" + 1 === "1"`, and `"10" < 9` is
//! `true`, all of which would silently produce a value where AINL errors.

use ainl_transpile::transpile_js_src;

fn js(src: &str) -> String {
    transpile_js_src(src).unwrap_or_else(|e| panic!("transpile failed for `{src}`: {e}"))
}

#[test]
fn file_builtins_map_to_node_fs_synchronously() {
    let out = js(r#"(do (write-file "p" "a") (append-file "p" "b") (read-file "p"))"#);
    assert!(out.contains("_write_file(\"p\", \"a\")"), "got:\n{out}");
    assert!(out.contains("_append_file(\"p\", \"b\")"), "got:\n{out}");
    assert!(out.contains("_read_file(\"p\")"), "got:\n{out}");
    // Synchronous: a promise-returning API would not work in expression
    // position, which is the only place a builtin call can appear.
    assert!(out.contains("readFileSync"), "got:\n{out}");
    assert!(out.contains("writeFileSync"), "got:\n{out}");
    assert!(out.contains("appendFileSync"), "got:\n{out}");
}

#[test]
fn json_builtins_do_not_reach_for_the_host_json_library() {
    // JSON.parse / JSON.stringify are the obvious mapping and are wrong here on
    // three counts: the result is a plain object rather than a _Hash, so AINL
    // key order and the first-position duplicate-key rule are lost; JSON.parse
    // accepts NaN and Infinity, which AINL must reject; and JSON.stringify
    // prints floats its own way ("1e+300", and no ".0" on a whole number) and
    // escapes every non-ASCII character as \uXXXX, so any program with a
    // non-ASCII string in it would disagree with the other three backends.
    let out = js(r#"(do (json-parse "[1]") (json-serialize (list 1 2)))"#);
    assert!(out.contains("_json_parse("), "got:\n{out}");
    assert!(out.contains("_json_ser("), "got:\n{out}");
    for helper in [
        "_json_parse",
        "_json_ser",
        "_json_str",
        "_json_float",
        "_json_parse_b",
        "_json_serialize_b",
    ] {
        assert!(
            out.contains(&format!("function {helper}(")),
            "missing {helper}:\n{out}"
        );
    }
    for forbidden in ["JSON.parse", "JSON.stringify"] {
        assert!(
            !out.contains(forbidden),
            "the host JSON library must not be used ({forbidden}):\n{out}"
        );
    }
    // A whole number gets a ".0" that JSON.stringify would not emit, and
    // toFixed is not usable as the rule (undefined above 1e21).
    assert!(out.contains("return \"0.0\";"), "got:\n{out}");
    assert!(
        !out.contains("toFixed"),
        "_json_float must not use toFixed (only defined up to 1e21):\n{out}"
    );
}

#[test]
fn string_builtins_map_to_string_methods() {
    let out = js(r#"(do (split "a,b" ",") (join (list "a") ",") (trim " x ")
            (replace "a" "a" "b") (upcase "a") (downcase "A")
            (contains "ab" "a"))"#);
    for helper in [
        "_split",
        "_join",
        "_trim",
        "_replace",
        "_upcase",
        "_downcase",
        "_contains",
    ] {
        assert!(
            out.contains(&format!("function {helper}(")),
            "missing {helper}:\n{out}"
        );
    }
    assert!(out.contains("hay.indexOf(needle) !== -1"), "got:\n{out}");
}

#[test]
fn case_folding_is_ascii_only_not_to_upper_case() {
    // toUpperCase() is Unicode-aware, which the C runtime and the Python/Ruby
    // targets cannot match.
    let out = js(r#"(upcase "a")"#);
    assert!(
        !out.contains(".toUpperCase()"),
        "must not use toUpperCase:\n{out}"
    );
    assert!(out.contains("/[a-z]/g"), "got:\n{out}");
    let out = js(r#"(downcase "A")"#);
    assert!(
        !out.contains(".toLowerCase()"),
        "must not use toLowerCase:\n{out}"
    );
}

#[test]
fn trim_strips_the_ascii_set_not_native_trim() {
    // String.prototype.trim also removes U+00A0, U+FEFF and the Unicode
    // whitespace class, which the other backends do not strip.
    let out = js(r#"(trim " x ")"#);
    assert!(
        !out.contains(".trim()"),
        "must not use bare .trim():\n{out}"
    );
    assert!(
        out.contains("\\x0b\\x0c"),
        "must name all six ASCII ws chars:\n{out}"
    );
}

#[test]
fn split_and_replace_reject_an_empty_target() {
    // JS's "".split("") splits per character and "abc".replace("", "x")
    // inserts at every position; AINL rejects both.
    let out = js(r#"(split "a" ",")"#);
    assert!(
        out.contains("split expects a non-empty separator"),
        "got:\n{out}"
    );
    let out = js(r#"(replace "a" "b" "c")"#);
    assert!(
        out.contains("replace expects a non-empty target"),
        "got:\n{out}"
    );
}

#[test]
fn math_builtins_type_check_instead_of_coercing() {
    let out = js(r#"(do (abs -1) (min 1 2) (max 1 2) (floor 1.5) (sqrt 2.25))"#);
    for helper in [
        "_isnum", "_abs", "_min", "_max", "_minmax", "_floor", "_sqrt",
    ] {
        assert!(
            out.contains(&format!("function {helper}(")),
            "missing {helper}:\n{out}"
        );
    }
    // The check is a typeof test, not a truthiness/NaN test: `typeof NaN` is
    // "number", and JS would otherwise coerce "5" and true into numbers.
    //
    // An AINL number is a BigInt or a `_Float`, so `_isnum` accepts both — a
    // `typeof x === "number"` test would reject every int.
    assert!(
        out.contains(r#"typeof x === "bigint" || x instanceof _Float"#),
        "got:\n{out}"
    );
    // `Math.*` cannot take a BigInt, so both convert at the boundary through
    // `_num_f` — this is the "loses JS-native Math ergonomics" cost the
    // numeric model documents, and it has to be explicit or it is a runtime
    // TypeError rather than a build error.
    assert!(out.contains("function _num_f("), "got:\n{out}");
    assert!(out.contains("Math.sqrt(x)"), "got:\n{out}");
    assert!(out.contains("Math.floor(_num_f(n))"), "got:\n{out}");
    // A negative sqrt is AINL's error, not JS's NaN.
    assert!(
        out.contains("sqrt expects a non-negative number"),
        "got:\n{out}"
    );
}

#[test]
fn min_max_share_one_fold_helper() {
    let out = js("(min 1 2)");
    assert!(out.contains("function _minmax("), "got:\n{out}");
    assert!(out.contains("function _min("), "got:\n{out}");
    assert!(
        !out.contains("function _max("),
        "max unused, must not be emitted:\n{out}"
    );
}

#[test]
fn env_exit_and_time_map_to_node_globals() {
    let out = js(r#"(do (env-get "X") (exit 0) (now) (sleep 0))"#);
    for helper in ["_env_get", "_exit", "_now", "_sleep"] {
        assert!(
            out.contains(&format!("function {helper}(")),
            "missing {helper}:\n{out}"
        );
    }
    assert!(out.contains("process.env[name]"), "got:\n{out}");
    // An unset variable is `undefined` in JS but `nil` in AINL, so the helper
    // must normalize it — otherwise `(env-get "X")` is truthy in JS and falsey
    // in the interpreter.
    assert!(out.contains("v === undefined ? null : v"), "got:\n{out}");
    // `process.exit` takes a 32-bit int and cannot take a BigInt, so the code
    // is converted and clamped rather than wrapped. `now` answers an int on
    // every backend, so it is a BigInt here too.
    assert!(out.contains("Number(code)"), "got:\n{out}");
    assert!(out.contains("process.exit(n >="), "got:\n{out}");
    assert!(
        out.contains("BigInt(Math.floor(Date.now() / 1000))"),
        "got:\n{out}"
    );
}

#[test]
fn sleep_blocks_synchronously() {
    // There is no synchronous sleep in Node, so a bare setTimeout would
    // return before sleeping. Atomics.wait on a SharedArrayBuffer does block,
    // which is what keeps `(sleep 0.01)` observable in the transpiled output.
    let out = js("(sleep 0.01)");
    assert!(out.contains("Atomics.wait"), "got:\n{out}");
    assert!(!out.contains("setTimeout"), "got:\n{out}");
}

#[test]
fn tier1_file_builtins_map_to_node_fs_and_our_own_path_rules() {
    let out = js(r#"(do (file-exists "p") (delete-file "p") (list-dir "d")
                    (path-join "a" "b") (path-base "a/b") (path-dir "a/b"))"#);
    for helper in [
        "_file_exists",
        "_delete_file",
        "_list_dir",
        "_path_join",
        "_path_base",
        "_path_dir",
    ] {
        assert!(
            out.contains(&format!("function {helper}(")),
            "missing {helper}:\n{out}"
        );
    }
    // The filesystem ones use node's own synchronous fs calls.
    assert!(out.contains("lstatSync"), "got:\n{out}");
    assert!(out.contains("readdirSync"), "got:\n{out}");
    assert!(out.contains("unlinkSync"), "got:\n{out}");
    // ...but the path ones must NOT use node's `path` module: path.join and
    // path.dirname disagree with the interpreter on empty parts, duplicate
    // separators and a bare relative name, and the whole point of pinning the
    // rules in eval.rs is that no host gets to choose.
    assert!(
        !out.contains("require(\"path\")"),
        "must not delegate to node's path module:\n{out}"
    );
    // list-dir sorts by UTF-8 bytes, not JS string order.
    assert!(
        out.contains("Buffer.compare"),
        "list-dir must sort by byte order:\n{out}"
    );
}

#[test]
fn path_join_reports_a_positional_type_error_in_ainl_wording() {
    // The interpreter says "got int at position 2". JS's own `typeof` would say
    // "number" for both an int and a float, so the helper carries its own
    // type-name function — and must pull in the classes it tests against.
    let out = js(r#"(path-join "a" 1)"#);
    assert!(
        out.contains("at position ${i + 1}"),
        "must report the position:\n{out}"
    );
    assert!(out.contains("function _ainl_tname("), "got:\n{out}");
    assert!(out.contains("class _Sym"), "_ainl_tname needs _Sym:\n{out}");
    assert!(
        out.contains("class _Hash"),
        "_ainl_tname needs _Hash:\n{out}"
    );
    // A bare (path-join) is an error, not "".
    let out = js("(path-join)");
    assert!(
        out.contains("path-join expects at least 1 argument"),
        "got:\n{out}"
    );
}

#[test]
fn stdlib_runtime_is_omitted_when_unused() {
    // `(print "hi")` is the one program that genuinely needs no arithmetic and
    // no type-name helper. `(+ 1 2)` no longer qualifies: it routes through
    // `_add`, which calls `_ainl_tname` (to name a bad operand) and `_isnum`,
    // so those two are now correctly emitted. They are dropped from the list
    // below rather than from the language — see js.rs's
    // `hash_runtime_omitted_when_unused` for the same reasoning.
    let out = js(r#"(print "hi")"#);
    for helper in [
        "_read_file",
        "_write_file",
        "_append_file",
        "_split",
        "_join",
        "_trim",
        "_replace",
        "_upcase",
        "_downcase",
        "_contains",
        "_env_get",
        "_exit",
        "_now",
        "_sleep",
        "_abs",
        "_min",
        "_max",
        "_floor",
        "_sqrt",
        "_isnum",
        "_file_exists",
        "_delete_file",
        "_list_dir",
        "_path_join",
        "_path_base",
        "_path_dir",
        "_path_canonical",
        "_ainl_tname",
    ] {
        assert!(!out.contains(helper), "unused {helper} was emitted:\n{out}");
    }
}
