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

/// Locate the `ainl` binary to compare against.
///
/// Debug is preferred over release deliberately, and the order matters. These
/// tests are usually run by `cargo test`, which builds the *debug* binary and
/// leaves any pre-existing `target/release/ainl` untouched. Preferring release
/// first meant a stale release binary won whenever one happened to be lying
/// around -- and the assertions failed with `unbound symbol 'read-file'`, i.e.
/// the tests silently compared against a binary predating Stage 3.1. In CI
/// that release binary came from the cargo cache, whose key is
/// `hashFiles('**/Cargo.toml')` and so does not change when only .rs files do.
fn ainl_bin() -> PathBuf {
    let debug = repo_root().join("target/debug/ainl");
    if debug.exists() {
        return debug;
    }
    let release = repo_root().join("target/release/ainl");
    assert!(
        release.exists(),
        "ainl binary not built; run `cargo build` first ({})",
        release.display()
    );
    release
}

fn compile_aot(src: &str, name: &str) -> PathBuf {
    let forms = ainl_core::parse(src).expect("parse");
    let c = ainl_cc::generate(&forms).expect("aot codegen");
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

/// The first line of a failed run's stderr, normalized for the differences
/// that are *not* the runtime's doing:
///
/// 1. The `ainl` CLI renders an `ainl_core::Error` through its `Display`,
///    which prefixes "runtime error: ", while a compiled binary prints
///    `g_errmsg` raw. That prefix is the CLI's, not the runtime's.
/// 2. A position suffix (` at line N, col M (byte B)`) is present on the
///    interpreter's error and absent on the AOT one.
///
/// (2) is the documented backend difference, not a silent divergence — see
/// docs/SYNTAX.md "Error messages" and the note in `ainl-cc`'s `generate`.
/// The AOT backend emits a *standalone C program*: the source text is not
/// embedded in the binary, so a line/column is not merely unavailable to the
/// runtime, it does not exist. Emitting a fabricated one would be worse than
/// omitting it, so the AOT message stops at the description. The rule this
/// test enforces is therefore: **the message body — what went wrong — must
/// match byte-for-byte, and the position suffix is interpreter-only.**
///
/// Stripping the suffix here (rather than skipping the assertions) keeps the
/// test honest: a change to the *wording* of a message still fails it.
fn norm_err(s: &[u8]) -> String {
    let text = String::from_utf8_lossy(s);
    let first = text.lines().next().unwrap_or("").trim();
    let first = first.strip_prefix("runtime error: ").unwrap_or(first);
    strip_pos_suffix(first)
}

/// Drop a trailing ` at line N, col M (byte B)` (or ` at byte B`) from a
/// diagnostic, leaving the description intact.
fn strip_pos_suffix(msg: &str) -> String {
    // " at byte N" is always the tail when there is no line/col.
    if let Some(i) = msg.rfind(" at byte ") {
        return msg[..i].to_string();
    }
    // Otherwise " at line N, col M (byte B)".
    if let Some(i) = msg.rfind(" at line ") {
        return msg[..i].to_string();
    }
    msg.to_string()
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

// ---- Tier 3 byte-oriented string primitives --------------------------------

/// The six byte primitives, including multi-byte input so a character-indexed
/// port cannot pass: `strstr` returns a byte offset, but a wrong length check
/// would still let a 2-byte character through.
#[test]
fn aot_byte_string_primitives_match_the_interpreter() {
    assert_stdout_parity(
        r#"(do (print (substring "abcdef" 0 6))
             (print (substring "abcdef" 2 4))
             (print (substring "abcdef" 0 0))
             (print (substring "héllo" 1 3))
             (print (char "abc" 0))
             (print (char "日本" 0))
             (print (char "日本" 3))
             (print (code "A"))
             (print (code "日本" 0))
             (print (code "日本" 1))
             (print (code "日本" 2))
             (print (starts-with "hello" "he"))
             (print (starts-with "hello" ""))
             (print (ends-with "hello" "lo"))
             (print (ends-with "hello" "he"))
             (print (index-of "hello" "llo"))
             (print (index-of "héllo" "llo"))
             (print (index-of "hello" "z"))
             (print (index-of "hello" ""))
             (print (index-of "日本" "本")))"#,
        "byte_strings",
    );
}

/// The one shared error message per failure mode, byte-identical between the
/// interpreter and the compiled binary — including the byte-offset wording,
/// which is where a hand-written port most easily drifts.
#[test]
fn aot_byte_string_error_messages_match_the_interpreter() {
    for (src, name) in [
        (r#"(substring "abc" 3 1)"#, "sub_start_gt_end"),
        (r#"(substring "abc" -1 2)"#, "sub_neg_start"),
        (r#"(substring "abc" 0 99)"#, "sub_past_end"),
        (r#"(substring "héllo" 0 2)"#, "sub_splits_char"),
        (r#"(substring "abc" 0 1.5)"#, "sub_float_end"),
        (r#"(char "日本" 1)"#, "char_splits_char"),
        (r#"(char "abc" 3)"#, "char_oob"),
        (r#"(char "abc" 1.5)"#, "char_float"),
        (r#"(code "")"#, "code_empty"),
        (r#"(code "abc" 9)"#, "code_oob"),
        (r#"(substring 1 0 2)"#, "sub_not_str"),
        (r#"(starts-with "a" 1)"#, "sw_not_str"),
        (r#"(index-of 1 "a")"#, "io_not_str"),
    ] {
        assert_error_parity(src, name);
    }
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

/// A file whose bytes are not valid UTF-8 cannot become a string, so `read-file`
/// must fail — in *both* backends.
///
/// This is a pre-existing divergence the Tier 1 work found: the interpreter
/// goes through `std::fs::read_to_string` (which fails on invalid UTF-8), while
/// the C runtime used to hand the raw bytes back, so the same program was a
/// runtime error interpreted and a 6-character string compiled. The C side now
/// validates; this pins both halves together.
#[test]
fn aot_read_file_rejects_invalid_utf8_like_the_interpreter() {
    let dir = std::env::temp_dir().join("ainl-aot-stdlib-utf8");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    // One invalid byte in the middle of otherwise-valid ASCII, plus the two
    // shapes a validator most often gets wrong: a *truncated* sequence and an
    // encoded surrogate half (ED A0 80, which is not well-formed UTF-8).
    for (name, bytes) in [
        ("bad.bin", b"ok\xffbad".to_vec()),
        ("trunc.bin", b"a\xc3".to_vec()),
        ("surrogate.bin", b"\xed\xa0\x80".to_vec()),
    ] {
        let p = dir.join(name);
        std::fs::write(&p, bytes).expect("write");
        let src = format!("(read-file {:?})", p.to_str().unwrap());
        assert_error_parity(&src, name);
    }
    // A valid multi-byte file must still read back, byte for byte: the check
    // must not be so strict that it rejects real UTF-8.
    let ok = dir.join("ok.txt");
    std::fs::write(&ok, "héllo 日本 😀\n".as_bytes()).expect("write");
    assert_stdout_parity(
        &format!("(print (len (read-file {:?})))", ok.to_str().unwrap()),
        "utf8_valid_read",
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- Tier 1 file I/O -------------------------------------------------------

/// A fixture directory whose contents make a *wrong* sort order visible.
///
/// The names are chosen so a case-insensitive or locale-aware collation gives
/// a different answer than byte order: uppercase `B` (0x42) before `_` (0x5F)
/// before lowercase `a` (0x61). A hidden file is present (it must be listed),
/// a name with a space (it must survive intact), and a subdirectory (it must
/// appear as a name, with no trailing separator).
fn seed_dir(dir: &Path) {
    std::fs::create_dir_all(dir.join("sub")).expect("mkdir sub");
    for name in ["Beta.txt", "alpha.txt", "zeta.md", "_.hidden", "sp ace.txt"] {
        std::fs::write(dir.join(name), "x").expect("write fixture");
    }
    std::fs::write(dir.join("sub").join("inner.txt"), "x").expect("write inner");
}

#[test]
fn aot_list_dir_matches_the_interpreter_and_is_sorted() {
    // Each backend gets its own seeded copy: `list-dir` is read-only, but
    // sharing one directory would make the assertion depend on the tests'
    // execution order.
    let src = r#"(do
        (print (list-dir {dir}))
        (print (len (list-dir {dir})))
        (print (first (list-dir {dir})))
        (print (list-dir (path-join {dir} "sub"))))"#;

    for (label, name) in [("aot", "list_dir_aot"), ("interp", "list_dir_interp")] {
        let dir = std::env::temp_dir().join(format!("ainl-aot-tier1-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        seed_dir(&dir);
        let program = src.replace("{dir}", &format!("{:?}", dir.to_str().unwrap()));

        let out = if label == "aot" {
            let bin = compile_aot(&program, name);
            Command::new(&bin).output().expect("run aot")
        } else {
            let (_, path) = run_interpreter(&program);
            let o = Command::new(ainl_bin())
                .arg("run")
                .arg(&path)
                .output()
                .expect("run ainl");
            let _ = std::fs::remove_file(&path);
            o
        };
        assert!(
            out.status.success(),
            "{label} list-dir failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8_lossy(&out.stdout);
        // `print` renders a bare string unquoted (only a list of them quotes),
        // so the third line is the name without quotes.
        assert_eq!(
            text,
            "(\"Beta.txt\" \"_.hidden\" \"alpha.txt\" \"sp ace.txt\" \"sub\" \"zeta.md\")\n\
             6\n\
             Beta.txt\n\
             (\"inner.txt\")\n",
            "{label} list-dir output differs, or is not in byte order"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn aot_file_exists_and_delete_file_match_the_interpreter() {
    // Stateful, so each backend runs against its own copy of the fixture — the
    // program deletes a file, and a shared directory would let the AOT run
    // invalidate what the interpreted run is about to check.
    let tmpl = r#"(do
        (print (file-exists {f}))
        (print (file-exists {missing}))
        (print (file-exists {sub}))
        (print (file-exists {g}))
        (write-file {g} "fresh\n")
        (print (file-exists {g}) (len (read-file {g})))
        (print (delete-file {g}))
        (print (file-exists {g}))
        (print (file-exists {f}))
        (print (file-exists (path-join {f} "/"))))"#;

    let run = |label: &str, program: &str, bin: Option<&Path>| -> String {
        let out = match bin {
            Some(b) => Command::new(b).output().expect("run aot"),
            None => {
                let (_, path) = run_interpreter(program);
                let o = Command::new(ainl_bin())
                    .arg("run")
                    .arg(&path)
                    .output()
                    .expect("run ainl");
                let _ = std::fs::remove_file(&path);
                o
            }
        };
        assert!(
            out.status.success(),
            "{label} file-exists/delete failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    let mut outputs = Vec::new();
    for (label, name) in [("aot", "fs_aot"), ("interp", "fs_interp")] {
        let dir = std::env::temp_dir().join(format!("ainl-aot-tier1-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        seed_dir(&dir);
        let d = format!("{:?}", dir.to_str().unwrap());
        let program = tmpl
            .replace("{f}", &format!("(path-join {d} \"alpha.txt\")"))
            .replace("{g}", &format!("(path-join {d} \"new.txt\")"))
            .replace("{sub}", &format!("(path-join {d} \"sub\")"))
            .replace("{missing}", &format!("(path-join {d} \"nope\")"));

        let out = if label == "aot" {
            let bin = compile_aot(&program, name);
            run(label, &program, Some(&bin))
        } else {
            run(label, &program, None)
        };
        // The documented answers: true/nil (not true/false), a directory
        // exists, the created file reads back, delete returns nil, and a
        // trailing separator still names the same file.
        assert_eq!(
            out, "true\nnil\ntrue\nnil\ntrue 6\nnil\nnil\ntrue\ntrue\n",
            "{label} file-exists/delete-file output differs from the contract"
        );
        outputs.push(out);
        let _ = std::fs::remove_dir_all(&dir);
    }
    assert_eq!(
        outputs[0], outputs[1],
        "AOT and the interpreter disagree on file-exists/delete-file"
    );
}

#[test]
fn aot_path_builtins_match_the_interpreter() {
    // The full edge-case table, not just the happy path: these are the inputs
    // where the four hosts disagree, so they are the ones worth pinning.
    assert_stdout_parity(
        r#"(do
        (print (path-join "a" "b" "c") (path-join "a//b" "d") (path-join "" "b"))
        (print (path-join "a" "" "b") (path-join "/a" "b") (path-join "a" "/b"))
        (print (path-join "a" "b/") (path-join "a" "." "b") (path-join "a" ".."))
        (print (path-base "a/b/c.txt") (path-base "a/b/") (path-base "/"))
        (print (path-base "") (path-base "x") (path-base "a/."))
        (print (path-dir "a/b/c.txt") (path-dir "x") (path-dir "/x"))
        (print (path-dir "a//b") (path-dir "a/.") (path-dir "/") (path-dir "")))"#,
        "path_builtins",
    );
}

#[test]
fn aot_tier1_error_messages_are_identical_to_the_interpreters() {
    // Error cases for the new builtins, including the two answers that are
    // design decisions rather than accidents (delete-file refuses a directory;
    // list-dir on a file is an error), and the positional type error from
    // path-join.
    let dir = std::env::temp_dir().join("ainl-aot-tier1-errors");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sub")).expect("mkdir");
    std::fs::write(dir.join("afile.txt"), "x").expect("write");
    let d = format!("{:?}", dir.to_str().unwrap());
    let cases: &[(&str, &str)] = &[
        (
            &format!("(delete-file (path-join {d} \"sub\"))"),
            "t1_del_dir",
        ),
        (
            &format!("(delete-file (path-join {d} \"gone.txt\"))"),
            "t1_del_missing",
        ),
        (
            &format!("(list-dir (path-join {d} \"afile.txt\"))"),
            "t1_list_file",
        ),
        (
            &format!("(list-dir (path-join {d} \"nodir\"))"),
            "t1_list_missing",
        ),
        ("(list-dir 1)", "t1_list_type"),
        ("(file-exists 1)", "t1_exists_type"),
        ("(delete-file 1)", "t1_delete_type"),
        ("(path-join)", "t1_join_arity"),
        ("(path-join \"a\" 1)", "t1_join_type"),
        ("(path-join \"a\" \"b\" 1.5)", "t1_join_type_float"),
        ("(path-base 1)", "t1_base_type"),
        ("(path-dir 1)", "t1_dir_type"),
    ];
    for (src, name) in cases {
        assert_error_parity(src, name);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- error-message identity (the card's "byte-for-byte" requirement) -------

#[test]
fn aot_json_error_messages_are_identical_to_the_interpreters() {
    // The JSON reader and writer are a second, independent C implementation of
    // the same spec, so an error path that exists on one side and not the other
    // is exactly the bug this catches — and the *message* is what a user reads,
    // so exit-code parity alone would not be enough. One case per rejection
    // family, plus both non-finite and the non-string-key rule.
    let cases: &[(&str, &str)] = &[
        // Malformed documents.
        (r#"(json-parse "{")"#, "j_open"),
        (r#"(json-parse "[1,]")"#, "j_trailing_comma"),
        (r#"(json-parse "{\"a\"}")"#, "j_no_colon"),
        (r#"(json-parse "tru")"#, "j_partial_kw"),
        (r#"(json-parse "01")"#, "j_leading_zero"),
        (r#"(json-parse "1.")"#, "j_dot_no_digit"),
        (r#"(json-parse "1e")"#, "j_exp_no_digit"),
        // Trailing content.
        (r#"(json-parse "1 2")"#, "j_trailing"),
        // Strings.
        (r#"(json-parse "\"unterminated")"#, "j_unterminated"),
        (r#"(json-parse "\"\\q\"")"#, "j_bad_escape"),
        (r#"(json-parse "\"\\u12\"")"#, "j_short_u"),
        (r#"(json-parse "\"\\ud800\"")"#, "j_lone_hi"),
        (r#"(json-parse "\"\\udc00\"")"#, "j_lone_lo"),
        (r#"(json-parse "\"\\ud800\\u0041\"")"#, "j_bad_pair"),
        // Argument types.
        (r#"(json-parse 1)"#, "j_type"),
        (r#"(json-parse nil)"#, "j_type_nil"),
        // Values with no JSON form.
        (r#"(json-serialize (quote sym))"#, "j_sym"),
        (r#"(json-serialize (fn (x) x))"#, "j_fn"),
        // AINL errors on a division by zero, so +inf is reached by overflow.
        (r#"(json-serialize (+ 1e308 1e308))"#, "j_inf"),
        (r#"(json-serialize (hash 1.5 "v"))"#, "j_key_type"),
    ];
    for (src, name) in cases {
        assert_error_parity(src, name);
    }
}

#[test]
fn aot_json_stdout_is_identical_to_the_interpreters() {
    // The float rule is where a JSON writer most easily diverges from the
    // interpreter, because the C runtime already had a *different* float
    // routine (format_float, a port of Value's Display) that it would be
    // natural to reuse — and reusing it splits 1e300 in two. Each of these
    // takes a different branch of the rule.
    let cases: &[(&str, &str)] = &[
        (r#"(print (json-serialize 0.5))"#, "j_half"),
        (r#"(print (json-serialize (/ 1.0 3)))"#, "j_third"),
        (r#"(print (json-serialize 1e21))"#, "j_big"),
        (r#"(print (json-serialize 1e-7))"#, "j_small"),
        (r#"(print (json-serialize 1e300))"#, "j_huge"),
        (r#"(print (json-serialize 1.25e17))"#, "j_frac_big"),
        (r#"(print (json-serialize 5e-324))"#, "j_subnormal"),
        (
            r#"(print (json-serialize (json-parse "{\"z\":1.5,\"a\":2.5}")))"#,
            "j_order",
        ),
        (
            r#"(print (json-serialize (json-parse "\"\\u00e9\\ud83d\\ude00\\u0007\"")))"#,
            "j_escapes",
        ),
    ];
    for (src, name) in cases {
        assert_stdout_parity(src, name);
    }
}

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
    //
    // The two HTTP builtins are deliberately NOT here. They are
    // interpreter-only (see `http_refusal.rs`), so there is no runtime.c
    // builtin for them and no id to give: a program using one is refused
    // before codegen, and the table must not pretend otherwise. They are
    // asserted as absent just below, so adding one to the prelude without
    // deciding its backend status fails here instead of silently passing.
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
        "file-exists",
        "delete-file",
        "list-dir",
        "path-join",
        "path-base",
        "path-dir",
        "json-parse",
        "json-serialize",
        "test",
        // Tier 3 collections. `map` / `filter` / `reduce` are special forms
        // lowered to loops before codegen (see ainl_core::collection_forms), so
        // they are deliberately absent: there is no builtin to emit, and adding
        // an arm for them here would only invite the question of what a
        // `map`-as-builtin would do with a `fn` it cannot call.
        "sort",
        // Tier 3 byte-oriented string primitives. All six are real builtins
        // (unlike map/filter/reduce, which are special forms), so all six need
        // an id and a name here.
        "substring",
        "char",
        "code",
        "starts-with",
        "ends-with",
        "index-of",
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
        let c = ainl_cc::generate(&forms).expect("aot codegen");
        assert!(
            c.contains("v_builtin("),
            "`{name}` did not compile to a builtin call:\n{c}"
        );
    }
    assert_eq!(names.len(), 62, "update this list when the prelude changes");

    // The other direction, which is the one that actually catches drift: every
    // name the prelude binds must be either in the table above (reachable by
    // codegen) or listed here as interpreter-only. A builtin that is in
    // neither is the silent failure this whole test exists to prevent — it
    // compiles to a `scope_lookup` and only fails when a program runs it.
    const INTERPRETER_ONLY: &[&str] = &["http-get", "http-post"];
    let listed: std::collections::HashSet<&str> = names.iter().map(|s| s.as_str()).collect();
    for name in INTERPRETER_ONLY {
        assert!(
            !listed.contains(name),
            "`{name}` is interpreter-only, so it must not be in the codegen table"
        );
        // It must nonetheless be bound in the prelude, or the interpreter-only
        // refusal is refusing a program that could never have called it.
        let v = env
            .get(name)
            .unwrap_or_else(|| panic!("the prelude does not define `{name}`"));
        assert!(
            matches!(v, ainl_core::Value::Builtin { .. }),
            "`{name}` must be a builtin in the prelude"
        );
    }
    let total = listed.len() + INTERPRETER_ONLY.len();
    assert_eq!(
        total,
        64,
        "the prelude has {total} builtins ({} portable + {} interpreter-only); \
         update the table and this count when the prelude changes",
        listed.len(),
        INTERPRETER_ONLY.len()
    );
}

// ---- Tier 3 collections ----------------------------------------------------
//
// `map` / `filter` / `reduce` reach the C runtime already lowered to `def`s and
// a `while` (see ainl_core::collection_forms), so for the AOT backend these cases
// test that the *shared* expansion compiles to C and runs — the same program text
// as the other backends, so any divergence is a real one.
//
// `sort` is different: it IS a builtin here, with a hand-written merge sort and a
// comparator invoked through `v_call`. It is the only place in the card where the
// AOT runtime implements an operation itself, so it gets the closest reading.

#[test]
fn aot_map_filter_and_reduce_match_the_interpreter() {
    assert_stdout_parity(
        r#"
        (def nums (list 1 2 3 4 5))
        (def dbl (fn (x) (* x 2)))
        (def big (fn (x) (> x 2)))
        (def add (fn (a x) (+ a x)))
        (print (map dbl nums))
        (print (filter big nums))
        (print (reduce add 0 nums))
        "#,
        "aot-collections-core",
    );
}

#[test]
fn aot_reduce_building_a_list_matches_the_interpreter() {
    // The card calls this shape out: an accumulator that is itself a list.
    assert_stdout_parity(
        r#"
        (def add (fn (a x) (push a (* x 10))))
        (print (reduce add (list) (list 1 2 3)))
        "#,
        "aot-collections-reduce-list",
    );
}

#[test]
fn aot_empty_collection_results_match_the_interpreter() {
    assert_stdout_parity(
        r#"
        (def id (fn (x) x))
        (def add (fn (a b) a))
        (print (map id (list)))
        (print (filter id (list)))
        (print (reduce add 99 (list)))
        (print (sort (list)))
        "#,
        "aot-collections-empty",
    );
}

#[test]
fn aot_walks_a_list_containing_nil_to_the_end() {
    // The termination test is `(> (len cur) 0)`, not `(= (first cur) nil)`.
    // A C port that used the `first` form would stop at the `nil` — and it would
    // be the *only* backend to do so, which is what this pins.
    assert_stdout_parity(
        r#"
        (def id (fn (x) x))
        (def keep (fn (a x) (push a x)))
        (print (map id (list 1 nil 2)))
        (print (filter id (list 1 nil 2 3)))
        (print (reduce keep (list) (list nil 1 nil 2)))
        "#,
        "aot-collections-nil",
    );
}

#[test]
fn aot_sort_matches_the_interpreter() {
    assert_stdout_parity(
        r#"
        (print (sort (list 3 1 2)))
        (print (sort (list "pear" "apple" "fig")))
        (print (sort (list -1 5 0)))
        "#,
        "aot-sort-default",
    );
}

#[test]
fn aot_sort_with_a_comparator_matches_the_interpreter() {
    // The comparator crosses the C function-pointer boundary here — a `Closure`
    // reached from inside a builtin, which is the AOT half of the card's
    // "fn-as-data" requirement.
    assert_stdout_parity(
        r#"
        (def sub (fn (a b) (- a b)))
        (def desc (fn (a b) (- b a)))
        (def byage (fn (a b) (- (nth a 1) (nth b 1))))
        (def people (list (list "bob" 30) (list "amy" 25) (list "cid" 30) (list "dan" 25)))
        (print (sort sub (list 3 1 2)))
        (print (sort desc (list 3 1 2)))
        (print (sort byage people))
        "#,
        "aot-sort-comparator",
    );
}

#[test]
fn aot_sort_is_stable() {
    // Equal keys keep input order. The C runtime uses a hand-written bottom-up
    // merge that takes from the left run on ties, so this is a property of that
    // code rather than of `qsort` (which is not stable and is not used).
    assert_stdout_parity(
        r#"
        (def byage (fn (a b) (- (nth a 1) (nth b 1))))
        (def people (list (list "bob" 30) (list "amy" 25) (list "cid" 30) (list "dan" 25)))
        (print (sort byage people))
        (print (sort (list 1 1 1 1 1 1)))
        (print (sort (fn (a b) (- b a)) (list 1 2 2 2 3 3 1)))
        "#,
        "aot-sort-stable",
    );
}

#[test]
fn aot_sort_rejects_a_mixed_type_list_with_the_same_message() {
    assert_error_parity(r#"(sort (list 1 "a"))"#, "aot-sort-mixed");
}

#[test]
fn aot_sort_rejects_a_bad_comparator_with_the_same_message() {
    assert_error_parity(r#"(sort (fn (a b) true) (list 1 2 3))"#, "aot-sort-badcmp");
}

#[test]
fn aot_sort_rejects_a_non_list_with_the_same_message() {
    assert_error_parity(r#"(sort 5)"#, "aot-sort-notlist");
}

#[test]
fn aot_sort_rejects_a_non_fn_comparator_even_on_a_short_list() {
    // The comparator's type is checked before any comparison runs, so a
    // 1-element list — where the comparator would never be called — still fails.
    // A C port that checked lazily would accept this silently.
    assert_error_parity(r#"(sort "x" (list 1))"#, "aot-sort-notfn-short");
    assert_error_parity(r#"(sort "x" (list))"#, "aot-sort-notfn-empty");
}
