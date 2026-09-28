//! Ruby projection of the Stage 3.1 stdlib.
//!
//! `scripts/check-transpile.sh` verifies the behavior end-to-end on
//! `examples/stdlib.ainl`. These tests cover the mapping itself, with emphasis
//! on the two places Ruby's defaults diverge from AINL's: `String#split` drops
//! trailing empty fields (it needs an explicit -1 limit), and `Comparable`
//! would let a String take part in `<` against an Integer, which AINL forbids.

use ainl_transpile::transpile_ruby_src;

fn rb(src: &str) -> String {
    transpile_ruby_src(src).unwrap_or_else(|e| panic!("transpile failed for `{src}`: {e}"))
}

#[test]
fn file_builtins_map_to_file_read_and_write() {
    let out = rb(r#"(do (write-file "p" "a") (append-file "p" "b") (read-file "p"))"#);
    assert!(out.contains("_write_file(\"p\", \"a\")"), "got:\n{out}");
    assert!(out.contains("_append_file(\"p\", \"b\")"), "got:\n{out}");
    assert!(out.contains("_read_file(\"p\")"), "got:\n{out}");
    for helper in ["_read_file", "_write_file", "_append_file"] {
        assert!(
            out.contains(&format!("def {helper}(")),
            "missing {helper}:\n{out}"
        );
    }
    assert!(out.contains("File.read(path)"), "got:\n{out}");
    // 'w' truncates, 'a' appends — the difference between the two builtins.
    assert!(out.contains("File.write(path, content)"), "got:\n{out}");
    assert!(out.contains("File.open(path, 'a')"), "got:\n{out}");
}

#[test]
fn json_builtins_do_not_reach_for_the_host_json_library() {
    // JSON.generate / JSON.parse are the obvious mapping and are wrong here on
    // three counts: a Hash is not an AHash, so AINL's insertion order and
    // first-position duplicate-key rule are lost; JSON.parse accepts NaN and
    // Infinity, which AINL must reject; and JSON.generate prints floats its
    // own way ("1.0e+300") and \u-escapes every non-ASCII character, so any
    // program with a non-ASCII string in it would disagree with the others.
    // Ruby's own Float#to_s is equally unusable for the same reason.
    let out = rb(r#"(do (json-parse "[1]") (json-serialize (list 1 2)))"#);
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
            out.contains(&format!("def {helper}(")),
            "missing {helper}:\n{out}"
        );
    }
    for forbidden in [
        "JSON.generate",
        "JSON.parse",
        "require 'json'",
        "require \"json\"",
    ] {
        assert!(
            !out.contains(forbidden),
            "the host JSON library must not be used ({forbidden}):\n{out}"
        );
    }
    // AHash is an Array of [k, v] pairs, so the map branch must come before the
    // plain-Array branch and must iterate pairs, not destructure them as
    // k/v — `each { |k, v| }` on an AHash of pairs silently yields the pair
    // arrays themselves.
    let ser = out
        .split("def _json_ser(")
        .nth(1)
        .expect("_json_ser is missing")
        .split("\nend")
        .next()
        .expect("unterminated _json_ser");
    assert!(
        ser.find("when AHash").unwrap() < ser.find("when Array").unwrap(),
        "AHash must be matched before Array (it is an Array subclass):\n{ser}"
    );
    assert!(
        ser.contains("v.each do |p|"),
        "_json_ser must iterate AHash pairs, not destructure them:\n{ser}"
    );
}

#[test]
fn string_builtins_map_to_ruby_string_methods() {
    let out = rb(r#"(do (split "a,b" ",") (join (list "a") ",") (trim " x ")
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
    assert!(out.contains("hay.include?(needle)"), "got:\n{out}");
}

#[test]
fn split_uses_a_minus_one_limit_to_keep_trailing_empty_fields() {
    // Ruby's String#split drops trailing empty fields by default:
    // "a,b,".split(",") => ["a", "b"], but AINL (and the C runtime, Python and
    // JS) keep them => ["a" "b" ""]. The -1 limit is the whole fix, and
    // getting it wrong is invisible until a program splits a file's lines.
    let out = rb(r#"(split "a,b," ",")"#);
    assert!(out.contains("s.split(sep, -1)"), "got:\n{out}");
    assert!(
        !out.contains("s.split(sep)\n"),
        "bare split would drop fields:\n{out}"
    );
}

#[test]
fn case_folding_is_ascii_only_not_native_upcase() {
    // String#upcase is Unicode-aware, which the C runtime and the Python/JS
    // targets cannot match.
    let out = rb(r#"(upcase "a")"#);
    assert!(
        !out.contains(".upcase"),
        "must not use String#upcase:\n{out}"
    );
    assert!(out.contains("/[a-z]/"), "got:\n{out}");
    let out = rb(r#"(downcase "A")"#);
    assert!(
        !out.contains(".downcase"),
        "must not use String#downcase:\n{out}"
    );
}

#[test]
fn trim_strips_the_ascii_set_not_native_strip() {
    // String#strip removes U+00A0 and other Unicode whitespace too.
    let out = rb(r#"(trim " x ")"#);
    assert!(!out.contains(".strip"), "must not use String#strip:\n{out}");
    assert!(
        out.contains("\\x0b\\x0c"),
        "must name all six ASCII ws chars:\n{out}"
    );
}

#[test]
fn split_and_replace_reject_an_empty_target() {
    // Ruby raises ArgumentError on an empty split separator and returns the
    // input unchanged for an empty replace target; AINL has one message for
    // both, and it must be AINL's.
    let out = rb(r#"(split "a" ",")"#);
    assert!(
        out.contains("split expects a non-empty separator"),
        "got:\n{out}"
    );
    let out = rb(r#"(replace "a" "b" "c")"#);
    assert!(
        out.contains("replace expects a non-empty target"),
        "got:\n{out}"
    );
}

#[test]
fn math_builtins_restrict_min_max_to_numeric() {
    let out = rb(r#"(do (abs -1) (min 1 2) (max 1 2) (floor 1.5) (sqrt 2.25))"#);
    for helper in ["_abs", "_min", "_max", "_minmax", "_floor", "_sqrt"] {
        assert!(
            out.contains(&format!("def {helper}(")),
            "missing {helper}:\n{out}"
        );
    }
    // Ruby's Comparable would let a String sort against an Integer in some
    // paths; AINL's min/max are numeric-only, so the check must be explicit.
    assert!(out.contains("x.is_a?(Numeric)"), "got:\n{out}");
    assert!(out.contains("Math.sqrt(n)"), "got:\n{out}");
    // A negative sqrt is AINL's message, not Math::DomainError's.
    assert!(
        out.contains("sqrt expects a non-negative number"),
        "got:\n{out}"
    );
    // floor returns an Integer and passes one straight through.
    assert!(
        out.contains("n.is_a?(Integer) ? n : n.floor"),
        "got:\n{out}"
    );
}

#[test]
fn min_max_share_one_fold_helper() {
    let out = rb("(min 1 2)");
    assert!(out.contains("def _minmax("), "got:\n{out}");
    assert!(out.contains("def _min("), "got:\n{out}");
    assert!(
        !out.contains("def _max("),
        "max unused, must not be emitted:\n{out}"
    );
}

#[test]
fn env_exit_and_time_map_to_ruby_core() {
    let out = rb(r#"(do (env-get "X") (exit 0) (now) (sleep 0))"#);
    // `_now` takes no arguments, so Ruby spells it without parens.
    for helper in ["_env_get", "_exit", "_sleep"] {
        assert!(
            out.contains(&format!("def {helper}(")),
            "missing {helper}:\n{out}"
        );
    }
    assert!(out.contains("def _now\n"), "got:\n{out}");
    // ENV[] already returns nil for an unset variable, matching AINL exactly —
    // no normalization needed, unlike the JS target.
    assert!(out.contains("ENV[name]"), "got:\n{out}");
    assert!(out.contains("exit(code)"), "got:\n{out}");
    assert!(out.contains("Time.now.to_i"), "got:\n{out}");
    assert!(out.contains("sleep(secs)"), "got:\n{out}");
}

#[test]
fn tier1_file_builtins_map_to_ruby_fs_and_our_own_path_rules() {
    let out = rb(r#"(do (file-exists "p") (delete-file "p") (list-dir "d")
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
            out.contains(&format!("def {helper}(")),
            "missing {helper}:\n{out}"
        );
    }
    // The filesystem ones use ruby's own File/Dir, all in the lstat form so a
    // broken symlink reads as present.
    assert!(out.contains("File.lstat"), "got:\n{out}");
    assert!(out.contains("Dir.children"), "got:\n{out}");
    assert!(out.contains("File.unlink"), "got:\n{out}");
    // ...but the path ones must NOT use File.join / File.dirname: File.join
    // inserts a separator for an empty first part (File.join("", "b") is "/b"
    // where os.path gives "b"), and File.dirname("x") is "." by a different
    // route than POSIX. The rules are pinned in eval.rs instead.
    assert!(
        !out.contains("File.join") && !out.contains("File.dirname"),
        "must not delegate to File's path helpers:\n{out}"
    );
    // list-dir sorts by UTF-8 bytes (String#b), not String#<=>.
    assert!(out.contains("sort_by { |n| n.b }"), "got:\n{out}");
}

#[test]
fn path_base_does_not_use_rubys_split_which_drops_trailing_empties() {
    // A real trap this work hit: "/".split("/") is [] in Ruby (it drops
    // trailing empty fields), so `.last` is nil where the interpreter's
    // rsplit gives "". The helper must use rindex/slice instead.
    let out = rb(r#"(path-base "/")"#);
    assert!(
        !out.contains("c.split('/').last"),
        "must not use split('/').last:\n{out}"
    );
    assert!(out.contains("c.rindex('/')"), "got:\n{out}");
}

#[test]
fn stdlib_runtime_is_omitted_when_unused() {
    let out = rb("(+ 1 2)");
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
