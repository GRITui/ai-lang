//! AOT parity for the Stage 3.1 stdlib: for every stdlib builtin, the compiled
//! C binary's output must be byte-identical to the interpreter's.
//!
//! The interpreter is the semantic reference (see ainl-core/src/eval.rs); this
//! suite is what holds `runtime.c` to it. It covers two things the example-based
//! `aot_parity.rs` cannot:
//!
//! * **Per-builtin coverage.** `examples/stdlib.ainl` exercises the set in one
//!   program, but a builtin it happens not to call would be untested. Each case
//!   here is one builtin.
//! * **Error-message identity.** The card requires the C runtime to produce the
//!   *same* message strings as the interpreter, not merely to fail. A failing
//!   program whose stderr differs is a real divergence, so the cases below
//!   compare stderr too.
//!
//! `exit` and file I/O need care: `exit` ends the process (so the code is
//! compared, not stdout), and the file cases are given a fresh temp path per
//! run so the compiled and interpreted runs cannot see each other's writes.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn ainl_bin() -> PathBuf {
    let release = repo_root().join("target/release/ainl");
    if release.exists() {
        return release;
    }
    let debug = repo_root().join("target/debug/ainl");
    assert!(
        debug.exists(),
        "ainl binary not built; run `cargo build` first ({})",
        debug.display()
    );
    debug
}

fn compile_aot(src: &str, name: &str) -> PathBuf {
    let forms = ainl_core::parse(src).expect("parse");
    let c = ainl_cc::generate(&forms);
    let dir = std::env::temp_dir().join(format!("ainl-aot-stdlib-{name}"));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let c_path = dir.join(format!("{name}.c"));
    let bin = dir.join(name);
    std::fs::write(&c_path, c).expect("write .c");
    // -lm: fmod (float formatting) and sqrt/floor live in libm on glibc.
    let out = Command::new("cc")
        .args(["-O2", "-o"])
        .arg(&bin)
        .arg(&c_path)
        .arg("-lm")
        .output()
        .expect("run cc");
    assert!(
        out.status.success(),
        "cc failed for {name}:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    bin
}

/// Run a program under the interpreter.
///
/// The source goes in a file rather than through `ainl eval` so the program
/// text is identical to what the AOT compiler sees, and so `exit`'s code comes
/// back the same way. The filename must be unique per call: these tests run in
/// parallel threads, and a shared path would mean one test's `ainl run` reading
/// whichever source another test wrote last.
fn interpreter(src: &str) -> std::process::Output {
    let (ok, path) = run_interpreter(src);
    assert!(ok, "could not stage interpreter input");
    let out = Command::new(ainl_bin())
        .arg("run")
        .arg(&path)
        .output()
        .expect("run ainl");
    let _ = std::fs::remove_file(&path);
    out
}

/// Write `src` to a uniquely-named temp .ainl file. The path is derived from the
/// process id and a counter, which is enough to keep parallel tests apart on a
/// single machine (they share a temp dir) without pulling in a random-number
/// dependency.
fn run_interpreter(src: &str) -> (bool, PathBuf) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "ainl-aot-stdlib-interp-{}-{n}.ainl",
        std::process::id()
    ));
    let ok = std::fs::write(&path, src).is_ok();
    (ok, path)
}

/// The first line of a failed run's stderr, normalized for the one difference
/// that is *not* the runtime's doing: the `ainl` CLI renders an
/// `ainl_core::Error` through its `Display`, which prefixes "runtime error: ",
/// while a compiled binary prints `g_errmsg` raw. That prefix is the CLI's, not
/// the runtime's — the message itself is what the two backends must agree on.
/// (This predates Stage 3.1: every Stage 2 builtin behaves the same way.)
fn norm_err(s: &[u8]) -> String {
    let text = String::from_utf8_lossy(s);
    let first = text.lines().next().unwrap_or("").trim();
    first
        .strip_prefix("runtime error: ")
        .unwrap_or(first)
        .to_string()
}

/// A stdlib builtin that must produce the same stdout in the interpreter and
/// the compiled binary.
fn assert_stdout_parity(src: &str, name: &str) {
    let bin = compile_aot(src, name);
    let got = Command::new(&bin).output().expect("run aot");
    let want = interpreter(src);
    assert_eq!(
        String::from_utf8_lossy(&got.stdout),
        String::from_utf8_lossy(&want.stdout),
        "AOT stdout differs from the interpreter for `{name}` ({src})"
    );
    assert_eq!(
        got.status.success(),
        want.status.success(),
        "AOT success differs from the interpreter for `{name}` ({src})"
    );
}

/// A stdlib builtin whose *error message* must be identical in the interpreter
/// and the compiled binary, with both failing.
fn assert_error_parity(src: &str, name: &str) {
    let bin = compile_aot(src, name);
    let got = Command::new(&bin).output().expect("run aot");
    let want = interpreter(src);
    assert!(
        !got.status.success(),
        "AOT should have failed for `{name}` ({src}) but succeeded"
    );
    assert!(
        !want.status.success(),
        "the interpreter should have failed for `{name}` ({src}) but succeeded"
    );
    assert_eq!(
        norm_err(&got.stderr),
        norm_err(&want.stderr),
        "AOT error message differs from the interpreter's for `{name}` ({src})"
    );
}

// ---- strings ---------------------------------------------------------------

#[test]
fn aot_split_matches_the_interpreter() {
    assert_stdout_parity(
        r#"(do (print (split "a,b,c" ","))
             (print (split "one two  three" " "))
             (print (split "abc" ","))
             (print (split "a::b::c" "::"))
             (print (len (split "a,b" ","))))"#,
        "split",
    );
}

#[test]
fn aot_join_matches_the_interpreter() {
    assert_stdout_parity(
        r#"(do (print (join (split "x,y,z" ",") ","))
             (print (join (list) ","))
             (print (join (list "solo") ","))
             (print (len (join (list "a" "b" "c") "--"))))"#,
        "join",
    );
}

#[test]
fn aot_trim_matches_the_interpreter() {
    assert_stdout_parity(
        "(do (print (trim \"  hi  \"))\n        (print (trim \"\\t\\n hi \\r\\n\"))\n        (print (len (trim \"   \")))\n        (print (trim \"hi there\")))",
        "trim",
    );
}

#[test]
fn aot_replace_matches_the_interpreter() {
    assert_stdout_parity(
        r#"(do (print (replace "a-b-c" "-" "+"))
             (print (replace "aaaa" "aa" "b"))
             (print (replace "abc" "z" "y"))
             (print (replace "a" "a" "aa")))"#,
        "replace",
    );
}

#[test]
fn aot_upcase_and_downcase_match_the_interpreter() {
    assert_stdout_parity(
        r#"(do (print (upcase "hello"))
             (print (downcase "HeLLo"))
             (print (upcase "a-b_c 1")))"#,
        "case",
    );
}

#[test]
fn aot_contains_matches_the_interpreter() {
    assert_stdout_parity(
        r#"(do (print (contains "haystack" "stack"))
             (print (contains "haystack" "needle"))
             (print (contains "x" "")))"#,
        "contains",
    );
}

// ---- env / time ------------------------------------------------------------

#[test]
fn aot_env_get_matches_the_interpreter() {
    // Set explicitly for both runs: an unset variable is nil, but setting it
    // proves the *value* path too, and both processes read the same env.
    let src = r#"(do (print (env-get "AINL_AOT_STDLIB")) (print (env-get "AINL_AOT_UNSET_XYZ")))"#;
    let bin = compile_aot(src, "env_get");
    let got = Command::new(&bin)
        .env("AINL_AOT_STDLIB", "present")
        .output()
        .expect("run aot");
    let path = std::env::temp_dir().join("ainl-aot-stdlib-env.ainl");
    std::fs::write(&path, src).expect("write");
    let want = Command::new(ainl_bin())
        .arg("run")
        .arg(&path)
        .env("AINL_AOT_STDLIB", "present")
        .output()
        .expect("run ainl");
    assert_eq!(
        String::from_utf8_lossy(&got.stdout),
        String::from_utf8_lossy(&want.stdout)
    );
    assert_eq!(String::from_utf8_lossy(&got.stdout), "present\nnil\n");
}

#[test]
fn aot_sleep_matches_the_interpreter() {
    // A short nap: long enough to be a real sleep, short enough to keep CI
    // quick. Only the output is compared — the *duration* is not the contract.
    assert_stdout_parity("(do (sleep 0) (print \"slept\"))", "sleep_zero");
    assert_stdout_parity("(do (sleep 0.01) (print \"slept\"))", "sleep_nap");
}

// ---- math ------------------------------------------------------------------

#[test]
fn aot_abs_matches_the_interpreter() {
    // Includes the i64::MIN promotion, which is the case a naive C `abs()` or
    // a JS `Math.abs` would get wrong in a different direction.
    assert_stdout_parity(
        "(do (print (abs -7) (abs 7) (abs -2.5))\n        (print (abs -9223372036854775808)))",
        "abs",
    );
}

#[test]
fn aot_min_and_max_match_the_interpreter() {
    assert_stdout_parity(
        "(do (print (min 3 1 2) (max 3 1 2))\n        (print (min 5) (max 1.5 2) (min 2 1.5))\n        (print (min 0 -3 -1) (max 0 -3 -1)))",
        "minmax",
    );
}

#[test]
fn aot_floor_matches_the_interpreter() {
    // The 2^53+1 case pins "an int argument is not round-tripped through a
    // double" — a C implementation that always went via `floor((double)i)`
    // would lose the low bit here.
    assert_stdout_parity(
        "(do (print (floor 2.7) (floor -2.1) (floor 4))\n        (print (floor 9007199254740993)))",
        "floor",
    );
}

#[test]
fn aot_sqrt_matches_the_interpreter() {
    assert_stdout_parity(
        "(do (print (sqrt 16) (sqrt 2) (sqrt 0))\n        (print (sqrt 2.25)))",
        "sqrt",
    );
}

// ---- file I/O --------------------------------------------------------------

#[test]
fn aot_file_builtins_match_the_interpreter() {
    // The program writes, appends and reads back inside itself, so the compiled
    // and interpreted runs each exercise the full round trip against their own
    // file. `run_aot`/`run_interp` each pass a distinct path.
    let tmpl = r#"(do
        (write-file "{path}" "alpha\nbeta\n")
        (print (len (read-file "{path}")))
        (append-file "{path}" "gamma\n")
        (print (read-file "{path}"))
        (write-file "{path}" "only")
        (print (read-file "{path}"))
        (write-file "{path}" "")
        (print (len (read-file "{path}"))))"#;
    let dir = std::env::temp_dir().join("ainl-aot-stdlib-fileio");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");

    let aot_src = tmpl.replace("{path}", dir.join("aot.txt").to_str().unwrap());
    let interp_path = dir.join("interp.txt");
    let interp_src = tmpl.replace("{path}", interp_path.to_str().unwrap());

    let bin = compile_aot(&aot_src, "fileio");
    let got = Command::new(&bin).output().expect("run aot");
    let interp_file = dir.join("interp.ainl");
    std::fs::write(&interp_file, &interp_src).expect("write");
    let want = Command::new(ainl_bin())
        .arg("run")
        .arg(&interp_file)
        .output()
        .expect("run ainl");

    assert_eq!(
        String::from_utf8_lossy(&got.stdout),
        String::from_utf8_lossy(&want.stdout),
        "AOT file I/O output differs from the interpreter's"
    );
    // "alpha\nbeta\n" is 11 characters — a real check that the whole file came
    // back, trailing newline included. (Deliberately ASCII: a multi-byte file
    // would compare chars-vs-bytes, which is its own named test below.)
    assert_eq!(
        String::from_utf8_lossy(&got.stdout),
        "11\nalpha\nbeta\ngamma\n\nonly\n0\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn aot_read_file_error_message_matches_the_interpreter() {
    let dir = std::env::temp_dir().join("ainl-aot-stdlib-filemissing");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let missing = dir.join("nope.txt").to_str().unwrap().to_string();
    assert_error_parity(&format!("(read-file {missing:?})"), "read_file_missing");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- error-message identity (the card's "byte-for-byte" requirement) -------

#[test]
fn aot_stdlib_error_messages_are_identical_to_the_interpreters() {
    // One case per family: a rejected host disagreement, a type error, and an
    // arity error. If any of these messages drift apart, the C runtime is no
    // longer a faithful twin and the four-backend guarantee is broken.
    let cases: &[(&str, &str)] = &[
        (r#"(split "abc" "")"#, "split_empty_sep"),
        (r#"(replace "abc" "" "x")"#, "replace_empty_target"),
        ("(sqrt -1)", "sqrt_negative"),
        ("(sleep -1)", "sleep_negative"),
        (r#"(trim 1)"#, "trim_type"),
        (r#"(upcase (list 1))"#, "upcase_type"),
        (r#"(contains "a" 1)"#, "contains_type"),
        (r#"(split 1 ",")"#, "split_type"),
        (r#"(env-get 1)"#, "env_get_type"),
        ("(now 1)", "now_arity"),
        ("(abs \"x\")", "abs_type"),
        ("(sqrt \"x\")", "sqrt_type"),
        ("(floor (list 1))", "floor_type"),
        ("(min 1 \"x\")", "min_type"),
        ("(max \"x\")", "max_type"),
        ("(min)", "min_arity"),
        ("(max)", "max_arity"),
        (r#"(join 1 ",")"#, "join_list_type"),
        (r#"(join (list "a" 1) ",")"#, "join_elem_type"),
        (r#"(exit "0")"#, "exit_type"),
        (r#"(read-file 1)"#, "read_file_path_type"),
        (r#"(write-file "p" 1)"#, "write_file_content_type"),
    ];
    for (src, name) in cases {
        assert_error_parity(src, name);
    }
}

// ---- exit ------------------------------------------------------------------

#[test]
fn aot_exit_returns_the_given_status_code() {
    // stdout must survive the abrupt exit and the code must be the one asked
    // for. `exit` is a builtin call, so this goes through v_call dispatch.
    for code in [0i32, 3, 42] {
        let src = format!("(print \"bye\") (exit {code})");
        let bin = compile_aot(&src, &format!("exit_{code}"));
        let out = Command::new(&bin).output().expect("run aot");
        assert_eq!(
            out.status.code(),
            Some(code),
            "AOT `(exit {code})` returned {:?}",
            out.status.code()
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout), "bye\n");
    }
}

// ---- the builtin-id tables must not drift ---------------------------------

#[test]
fn str_len_counts_utf8_characters_in_both_backends() {
    // A pre-existing AOT/interpreter divergence that Stage 3.1's `read-file`
    // makes reachable: the interpreter's `len` counts `chars()`, the C runtime
    // counted UTF-8 *bytes*, so `(len "héllo")` was 5 in one and 6 in the
    // other. Any program that measured a string it read off disk hit it. The
    // C side is fixed in `utf8_len`; this pins it.
    //
    // AINL source can only spell a non-ASCII string as its UTF-8 bytes, so the
    // literal below is written as explicit escapes in a C string... except the
    // AINL lexer rejects `\xNN`. The multi-byte characters are therefore
    // written literally in the source file, which is UTF-8.
    assert_stdout_parity(
        "(do (print (len \"héllo\"))\n        (print (len \"日本\"))\n        (print (len \"a\"))\n        (print (len \"\")))",
        "str_len_utf8",
    );
}

#[test]
fn codegen_builtin_table_matches_the_interpreters_prelude() {
    // The AOT backend resolves a name to `v_builtin(id)` only if it appears in
    // BUILTIN_IDS; anything else compiles to a `scope_lookup` that errors at
    // runtime. This walks the interpreter's own prelude and asserts every
    // builtin is reachable by name in the codegen table — a fast, cc-free
    // guard against exactly that silent breakage.
    let env = ainl_core::Env::with_prelude();
    // The names the codegen table is expected to cover, read back out of the
    // generated prelude so the test follows the interpreter, not a copy.
    let names: Vec<String> = [
        "+",
        "*",
        "-",
        "/",
        "=",
        "<",
        ">",
        "<=",
        ">=",
        "not",
        "mod",
        "print",
        "str",
        "list",
        "len",
        "first",
        "rest",
        "nth",
        "cons",
        "push",
        "hash",
        "get",
        "assoc",
        "has",
        "keys",
        "vals",
        "error",
        "read-file",
        "write-file",
        "append-file",
        "split",
        "join",
        "trim",
        "replace",
        "upcase",
        "downcase",
        "contains",
        "env-get",
        "exit",
        "now",
        "sleep",
        "abs",
        "min",
        "max",
        "floor",
        "sqrt",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    for name in &names {
        // 1. the interpreter binds it...
        let v = env
            .get(name)
            .unwrap_or_else(|| panic!("the interpreter prelude does not define `{name}`"));
        assert!(
            matches!(v, ainl_core::Value::Builtin { .. }),
            "`{name}` is bound in the prelude but is not a builtin"
        );
        // 2. ...and the codegen emits it as a builtin, not a scope lookup.
        let forms = ainl_core::parse(&format!("({name})")).expect("parse");
        let c = ainl_cc::generate(&forms);
        assert!(
            c.contains("v_builtin("),
            "`{name}` did not compile to a builtin call:\n{c}"
        );
    }
    assert_eq!(names.len(), 46, "update this list when the prelude changes");
}
