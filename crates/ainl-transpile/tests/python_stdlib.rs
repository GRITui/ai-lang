//! Python projection of the Stage 3.1 stdlib.
//!
//! The behavior contract is checked end-to-end by
//! `scripts/check-transpile.sh` on `examples/stdlib.ainl` (interpreter stdout
//! vs transpiled stdout, byte for byte). These tests cover the other half: that
//! each builtin maps to the *host's* idiom and that the helpers needed for that
//! mapping are actually emitted.

use ainl_transpile::transpile_python_src;

fn py(src: &str) -> String {
    transpile_python_src(src).unwrap_or_else(|e| panic!("transpile failed for `{src}`: {e}"))
}

#[test]
fn file_builtins_map_to_open() {
    let out = py(r#"(do (write-file "p" "a") (append-file "p" "b") (read-file "p"))"#);
    // The host's own idiom, not a reimplementation of file I/O.
    assert!(out.contains("_write_file(\"p\", \"a\")"), "got:\n{out}");
    assert!(out.contains("_append_file(\"p\", \"b\")"), "got:\n{out}");
    assert!(out.contains("_read_file(\"p\")"), "got:\n{out}");
    for helper in ["_read_file", "_write_file", "_append_file"] {
        assert!(
            out.contains(&format!("def {helper}(")),
            "missing {helper}:\n{out}"
        );
    }
    // 'w' truncates and 'a' appends — the semantic difference between the two.
    assert!(out.contains("open(path, 'w')"), "got:\n{out}");
    assert!(out.contains("open(path, 'a')"), "got:\n{out}");
}

#[test]
fn string_builtins_map_to_str_methods() {
    let out = py(r#"(do (split "a,b" ",") (join (list "a") ",") (trim " x ")
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
            out.contains(&format!("def {helper}(")),
            "missing {helper}:\n{out}"
        );
    }
    // `contains` is Python's `in`.
    assert!(out.contains("return needle in hay"), "got:\n{out}");
}

#[test]
fn case_folding_is_ascii_only_not_python_str_upper() {
    // str.upper() is Unicode-aware, which the C runtime and the JS/Ruby
    // targets cannot match. The helper must fold the ASCII range only.
    let out = py(r#"(upcase "a")"#);
    assert!(!out.contains(".upper()"), "must not use str.upper:\n{out}");
    assert!(out.contains("'a' <= c <= 'z'"), "got:\n{out}");
    let out = py(r#"(downcase "A")"#);
    assert!(!out.contains(".lower()"), "must not use str.lower:\n{out}");
}

#[test]
fn trim_strips_the_ascii_set_not_str_strip() {
    // str.strip() also removes U+00A0 and other Unicode whitespace, which the
    // interpreter and the other targets do not.
    let out = py(r#"(trim " x ")"#);
    assert!(
        !out.contains(".strip()"),
        "must not use bare str.strip:\n{out}"
    );
    assert!(
        out.contains("\\x0b\\x0c"),
        "must name all six ASCII ws chars:\n{out}"
    );
}

#[test]
fn split_and_replace_reject_an_empty_target() {
    // Python raises on an empty split separator and *inserts* on an empty
    // replace target; AINL rejects both, so the helpers must too.
    let out = py(r#"(split "a" ",")"#);
    assert!(
        out.contains("split expects a non-empty separator"),
        "got:\n{out}"
    );
    let out = py(r#"(replace "a" "b" "c")"#);
    assert!(
        out.contains("replace expects a non-empty target"),
        "got:\n{out}"
    );
}

#[test]
fn env_exit_and_time_map_to_os_sys_time() {
    let out = py(r#"(do (env-get "X") (exit 0) (now) (sleep 0))"#);
    for helper in ["_env_get", "_exit", "_now", "_sleep"] {
        assert!(
            out.contains(&format!("def {helper}(")),
            "missing {helper}:\n{out}"
        );
    }
    // The host's own modules, imported lazily inside the helper.
    assert!(out.contains("os.environ.get"), "got:\n{out}");
    assert!(out.contains("raise SystemExit(code)"), "got:\n{out}");
    assert!(out.contains("int(time.time())"), "got:\n{out}");
    assert!(out.contains("time.sleep(secs)"), "got:\n{out}");
}

#[test]
fn math_builtins_map_to_math_module_with_ainl_rules() {
    let out = py(r#"(do (abs -1) (min 1 2) (max 1 2) (floor 1.5) (sqrt 2.25))"#);
    for helper in ["_abs", "_min", "_max", "_floor", "_sqrt", "_minmax"] {
        assert!(
            out.contains(&format!("def {helper}(")),
            "missing {helper}:\n{out}"
        );
    }
    assert!(out.contains("math.sqrt(n)"), "got:\n{out}");
    assert!(out.contains("math.floor(n)"), "got:\n{out}");
    // A negative sqrt is AINL's error, not Python's ValueError from math.sqrt.
    assert!(
        out.contains("sqrt expects a non-negative number"),
        "got:\n{out}"
    );
    // An int argument to floor passes through, so a large i64 is not
    // round-tripped through a float.
    assert!(
        out.contains("n if isinstance(n, int) else math.floor(n)"),
        "got:\n{out}"
    );
}

#[test]
fn min_max_share_one_fold_helper() {
    // `_min`/`_max` must not duplicate the fold — it is emitted once.
    let out = py("(min 1 2)");
    assert!(out.contains("def _minmax("), "got:\n{out}");
    assert!(out.contains("def _min("), "got:\n{out}");
    assert!(
        !out.contains("def _max("),
        "max unused, must not be emitted:\n{out}"
    );
}

#[test]
fn stdlib_runtime_is_omitted_when_unused() {
    // A program that touches no stdlib must not carry any of its helpers.
    let out = py("(+ 1 2)");
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
    ] {
        assert!(!out.contains(helper), "unused {helper} was emitted:\n{out}");
    }
}

#[test]
fn join_rejects_a_non_string_element() {
    // The interpreter errors; Python's str.join would coerce. The helper must
    // check, and must exclude a quoted symbol (a _Sym str subclass).
    let out = py(r#"(join (list "a") ",")"#);
    assert!(out.contains("join expects a list of str"), "got:\n{out}");
    assert!(
        out.contains("isinstance(x, _Sym)"),
        "must exclude _Sym:\n{out}"
    );
    // ...and that pulls in the _Sym class it tests against.
    assert!(out.contains("class _Sym(str)"), "got:\n{out}");
}
