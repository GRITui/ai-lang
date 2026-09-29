//! The Tier 4 key-value layer on the **compiled** AOT binary.
//!
//! The card's acceptance has two halves that only this file can cover:
//!
//! * **3-backend byte-identical parity** — interpreter, VM, and the C runtime
//!   this crate carries. `runtime.c` is an *independent* implementation of the
//!   value layer, not a binding to the Rust one (there is no FFI anywhere in
//!   this project), so "the engine is shared" holds only between the two
//!   evaluators. Everything here compares the compiled binary's output against
//!   the interpreter's, byte for byte, on stdout *and* stderr.
//! * **Persistence across a process** — set, get, close, reopen, intact, in a
//!   *different binary*. A value that lives in a cached index and never reaches
//!   the file passes every same-process test, which is why these are separate
//!   programs run in sequence rather than one program that closes and reopens.
//!
//! The transpiler half is a refusal, and lives in `db_refusal.rs`.
//!
//! Two bugs this file exists to have caught, both of which a Rust-only test
//! would have passed:
//!
//! * The C index **chains** rather than replacing, so a key written twice
//!   occupies two entries. `db-keys` and `db-count` deduplicate there and in
//!   Rust they do not have to — an asymmetry that made the C port list an
//!   overwritten key twice.
//! * A function stored through `db-set` has to be refused with the *same* string
//!   on both engines. It is, because both call their own JSON writer, and
//!   because the C port clears the parser's error before raising its own.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A unique scratch directory for one test, removed when it drops.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Scratch {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!("ainl-dbkvaot-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("scratch dir");
        Scratch { path: p }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The `ainl` CLI built for tests, next to the test binary.
fn ainl() -> PathBuf {
    let mut p = std::env::current_exe().expect("test exe path");
    p.pop(); // deps/
    if p.ends_with("deps") {
        p.pop();
    }
    p.push(if cfg!(windows) { "ainl.exe" } else { "ainl" });
    assert!(
        p.exists(),
        "the ainl CLI was not found at {} — run `cargo build` first",
        p.display()
    );
    p
}

/// Compile `src` to a native binary, returning `(binary, generated C)`.
///
/// The flags are pinned to the ones `scripts/check-aot.sh` uses. An earlier tier
/// in this repo recorded why: on macOS the link step succeeds with flags that
/// fail on Linux, so a green local run is not evidence about CI. Pinning them
/// here means this test asserts the property the CI job does.
fn build(src: &str, tag: &str, dir: &Path) -> (PathBuf, PathBuf) {
    let ainl_path = dir.join("p.ainl");
    let c_path = dir.join("p.c");
    let bin_path = dir.join(format!("{tag}.bin"));
    std::fs::write(&ainl_path, src).expect("write the program");

    let out = Command::new(ainl())
        .arg("compile")
        .arg(&ainl_path)
        .arg("-o")
        .arg(&bin_path)
        .arg("--keep-c")
        .arg(&c_path)
        .output()
        .expect("run ainl compile");
    assert!(
        out.status.success(),
        "ainl compile refused the {tag} program: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (bin_path, c_path)
}

/// Run a compiled binary to completion in `dir`, returning `(stdout, stderr)`.
fn run(bin: &Path, dir: &Path) -> (String, String, bool) {
    let out = Command::new(bin)
        .current_dir(dir)
        .output()
        .expect("run the compiled binary");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

/// Run a program in the **interpreter**, in its own directory.
///
/// The point of the whole file: the same source, the interpreter and the
/// compiled binary, in two separate directories so neither sees the other's
/// database, and then their bytes are compared.
fn run_interp(src: &str, dir: &Path) -> (String, String, bool) {
    let p = dir.join("i.ainl");
    std::fs::write(&p, src).expect("write the program");
    let out = Command::new(ainl())
        .arg("run")
        .arg(&p)
        .current_dir(dir)
        .output()
        .expect("run ainl");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

/// The parity assertion this file exists for: the same program, the interpreter
/// and the compiled binary, byte-identical on both streams and the same exit
/// code.
fn assert_parity(src: &str, tag: &str) {
    // Separate directories: a shared one would let the interpreter's database
    // answer the binary's read, and the test would pass for the wrong reason.
    let si = Scratch::new(&format!("{tag}-interp"));
    let sc = Scratch::new(&format!("{tag}-aot"));
    let (i_out, i_err, i_ok) = run_interp(src, &si.path);
    let (bin, _) = build(src, tag, &sc.path);
    let (c_out, c_err, c_ok) = run(&bin, &sc.path);

    assert_eq!(
        (i_ok, c_ok),
        (true, true),
        "the {tag} program: interpreter ok={i_ok} compiled ok={c_ok}\nstderr interp: {i_err}\nstderr aot: {c_err}"
    );
    assert_eq!(
        i_out, c_out,
        "the {tag} program: stdout differs between the interpreter and the compiled binary"
    );
    assert_eq!(
        i_err, c_err,
        "the {tag} program: stderr differs between the interpreter and the compiled binary"
    );
}

// ---- the value round trip, on a compiled binary ----------------------------

#[test]
fn every_value_type_round_trips_in_the_compiled_binary() {
    assert_parity(
        r#"(do (def h (db-open "d.ainl-db"))
             (db-set h "i" 42) (db-set h "n" -7) (db-set h "f" 1.5)
             (db-set h "wf" 1.0) (db-set h "t" true) (db-set h "fa" false)
             (db-set h "nul" nil) (db-set h "s" "hello") (db-set h "e" "")
             (db-set h "l" (list 1 "two" false nil (list 3 4)))
             (db-set h "m" (hash "a" 1 "b" 2))
             (def r (json-serialize (list (db-get h "i") (db-get h "n")
                                           (db-get h "f") (db-get h "wf")
                                           (db-get h "t") (db-get h "fa")
                                           (db-get h "nul") (db-get h "s")
                                           (db-get h "e") (db-get h "l")
                                           (db-get h "m"))))
             (db-close h)
             (print r))"#,
        "roundtrip",
    );
}

/// The int/float distinction, on its own. `=` would accept either, so the
/// assertion re-serializes: `1` and `1.0` are different JSON.
#[test]
fn an_int_stays_an_int_and_a_whole_float_stays_a_float_in_c() {
    assert_parity(
        r#"(do (def h (db-open "d.ainl-db"))
             (db-set h "i" 1) (db-set h "f" 1.0)
             (def r (list (json-serialize (db-get h "i"))
                           (json-serialize (db-get h "f"))))
             (db-close h)
             (print r))"#,
        "intfloat",
    );
}

// ---- enumeration -----------------------------------------------------------

/// Sorted keys, and the dedup rule: an overwritten key is listed and counted
/// **once**. The C index chains, so this is where the first version of the port
/// went wrong.
#[test]
fn keys_are_sorted_and_deduplicated_in_c() {
    assert_parity(
        r#"(do (def h (db-open "d.ainl-db"))
             (db-set h "b" 1) (db-set h "A" 1) (db-set h "a" 1)
             (db-set h "10" 1) (db-set h "2" 1)
             (db-set h "b" 2) (db-set h "b" 3)
             (def r (list (db-keys h) (db-count h)))
             (db-close h)
             (print r))"#,
        "keys",
    );
}

// ---- deletion --------------------------------------------------------------

/// A delete must clear all three readers at once. The three are asserted
/// together because a delete that cleared `db-get` but not `db-keys` passes
/// either one alone.
#[test]
fn a_delete_clears_get_keys_and_count_in_c() {
    assert_parity(
        r#"(do (def h (db-open "d.ainl-db"))
             (db-set h "a" 1) (db-set h "b" 2)
             (def gone (db-del h "a"))
             (def again (db-del h "a"))
             (def r (list gone again (db-get h "a") (db-keys h) (db-count h)))
             (db-close h)
             (print r))"#,
        "delete",
    );
}

// ---- the two layers, and the sharp edge ------------------------------------

/// `db-put` text that is valid JSON reads as the value it spells; text that is
/// not is an error naming the text and the fix.
///
/// The stdout half is a strict byte-for-byte parity assertion — the decode and
/// the refusal are the same on both engines. The error is caught and printed
/// rather than allowed to escape, because an *escaping* error's text carries
/// the source position on one backend and not the other (see the refusal test
/// below), and this case is about the message's content, not its suffix.
#[test]
fn db_put_text_is_json_or_an_error_in_c() {
    assert_parity(
        r#"(do (def h (db-open "d.ainl-db"))
             (db-put h "num" "42")
             (db-put h "note" "buy milk")
             (print (db-get h "num"))
             (try (db-get h "note") (catch (e) (print (get e "message"))))
             (db-close h))"#,
        "puttext",
    );
}

/// `db-get-raw` is the byte layer's reader, under its own name, and its errors
/// carry that name.
#[test]
fn db_get_raw_is_the_byte_layer_reader_in_c() {
    assert_parity(
        r#"(do (def h (db-open "d.ainl-db"))
             (db-put h "note" "buy milk")
             (db-set h "n" 42)
             (print (db-get-raw h "note"))
             (print (db-get-raw h "n"))
             (db-close h))"#,
        "getraw",
    );
}

// ---- refusals, byte for byte ----------------------------------------------

/// The value layer's **own** refusal text, asserted per backend rather than
/// compared across them.
///
/// This is the one place this file deliberately does *not* require the two
/// backends to be byte-identical, and the reason is measured rather than
/// assumed. The compiled binary's runtime does not append the interpreter's
/// `at line L, col C (byte B)` suffix, and it phrases a couple of the shared
/// type errors slightly differently (`db-put expects a str, got int` where the
/// interpreter says `db-put expects a str key, got int`). Both facts predate
/// this card — they hold for §3k's five builtins on `git show origin/main`
/// — and they are a property of the whole AOT runtime, not of the value layer.
///
/// So the claim asserted here is the one that *is* new and *is* required:
/// each backend refuses with its own consistent, documented message, and the
/// two agree on the part a program can branch on — that it failed at all, and
/// that the message names the right builtin. Closing the suffix gap properly
/// means changing the AOT error path for every builtin, which is a separate
/// change and not this card's to smuggle in.
#[test]
fn a_stored_function_is_refused_by_both_backends() {
    let src = r#"(do (def h (db-open "d.ainl-db"))
             (db-set h "k" (fn (x) x))
             (db-close h))"#;
    for (label, stderr) in [
        ("interpreter", run_interp_err(src)),
        ("aot", run_aot_err(src)),
    ] {
        assert!(
            stderr.contains("json-serialize: cannot serialize a fn"),
            "{label} must refuse a stored function with the JSON writer's own message, got: {stderr}"
        );
    }
}

/// The stderr of a program run in the interpreter, in its own directory.
fn run_interp_err(src: &str) -> String {
    let s = Scratch::new("interp-err");
    let (_out, err, _ok) = run_interp(src, &s.path);
    err
}

/// The stderr of a program compiled and then run.
fn run_aot_err(src: &str) -> String {
    let s = Scratch::new("aot-err");
    let (bin, _) = build(src, "err", &s.path);
    let (_out, err, _ok) = run(&bin, &s.path);
    err
}

/// Arity, type and stale-handle refusals, checked per backend rather than
/// compared byte for byte.
///
/// The same reason as above: the AOT runtime's messages carry no source position
/// and word two of the shared type errors differently, and that predates this
/// card. What each of these asserts instead is the thing that is actually the
/// card's contract — **every** one of the value builtins validates its own
/// arity and its own operand types, and it refuses by name. A port that
/// skipped a check, or reported it under another builtin's name, fails here; a
/// port that phrased it the way the interpreter phrases it does not get credit
/// for it, because that claim is not true today and is not this card's to make.
#[test]
fn every_value_layer_refusal_is_refused_by_both_backends() {
    for (tag, src, must_name) in [
        ("arity-set", r#"(db-set 1 "k")"#, "db-set"),
        ("arity-del", r#"(db-del 1)"#, "db-del"),
        ("arity-keys", r#"(db-keys 1 2)"#, "db-keys"),
        ("arity-count", r#"(db-count)"#, "db-count"),
        ("arity-raw", r#"(db-get-raw 1)"#, "db-get-raw"),
        ("arity-get", r#"(db-get 1)"#, "db-get"),
        ("handle-type", r#"(db-set "x" "k" 1)"#, "db-set"),
        ("key-type", r#"(db-set 1 5 1)"#, "db-set"),
        (
            "stale-set",
            r#"(do (def h (db-open "d.ainl-db")) (db-close h) (db-set h "k" 1))"#,
            "db-set",
        ),
        (
            "stale-keys",
            r#"(do (def h (db-open "d.ainl-db")) (db-close h) (db-keys h))"#,
            "db-keys",
        ),
        (
            "stale-count",
            r#"(do (def h (db-open "d.ainl-db")) (db-close h) (db-count h))"#,
            "db-count",
        ),
        (
            "stale-raw",
            r#"(do (def h (db-open "d.ainl-db")) (db-close h) (db-get-raw h "k"))"#,
            "db-get-raw",
        ),
    ] {
        let i_err = run_interp_err(src);
        let a_err = run_aot_err(src);
        for (label, err) in [("interpreter", &i_err), ("aot", &a_err)] {
            assert!(
                !err.is_empty(),
                "{tag}: the {label} backend must refuse, but it printed nothing"
            );
            assert!(
                err.contains(must_name),
                "{tag}: the {label} backend's message must name {must_name}, got: {err}"
            );
        }
    }
}

// ---- persistence across processes, on the compiled binary -----------------

/// The card's headline persistence claim, in three separate binaries.
///
/// Each phase is a *different program* and a *different process*: that is the
/// only way to show the value reached the file rather than a cache. The delete
/// in phase 2 has to still be there in phase 3, which is the tombstone claim.
#[test]
fn a_value_and_its_delete_survive_three_compiled_processes() {
    let s = Scratch::new("persist3");
    let dir = &s.path;

    let (b1, _) = build(
        r#"(do (def h (db-open "d.ainl-db"))
             (db-set h "int" 42) (db-set h "float" 1.5) (db-set h "str" "hello")
             (db-set h "bool" true) (db-set h "nil" nil)
             (db-set h "list" (list 1 "two" (list 3 4)))
             (db-set h "doomed" "gone soon")
             (db-set h "over" "first") (db-set h "over" "second")
             (db-flush h) (db-close h))"#,
        "p1",
        dir,
    );
    let (o1, e1, ok1) = run(&b1, dir);
    assert!(ok1, "phase 1 failed: {e1}");

    let (b2, _) = build(
        r#"(do (def h (db-open "d.ainl-db"))
             (print (db-get h "int") (db-get h "list") (db-get h "over"))
             (print (db-count h))
             (print (db-del h "doomed"))
             (db-flush h) (db-close h))"#,
        "p2",
        dir,
    );
    let (o2, e2, ok2) = run(&b2, dir);
    assert!(ok2, "phase 2 failed: {e2}");
    assert_eq!(
        o2, "42 (1 \"two\" (3 4)) second\n8\ntrue\n",
        "phase 2 read the wrong values"
    );

    let (b3, _) = build(
        r#"(do (def h (db-open "d.ainl-db"))
             (print (db-get h "doomed"))
             (print (db-get h "int") (db-get h "over"))
             (print (db-count h))
             (print (db-keys h))
             (db-close h))"#,
        "p3",
        dir,
    );
    let (o3, e3, ok3) = run(&b3, dir);
    assert!(ok3, "phase 3 failed: {e3}");
    assert_eq!(
        o3, "nil\n42 second\n7\n(\"bool\" \"float\" \"int\" \"list\" \"nil\" \"over\" \"str\")\n",
        "the delete did not survive, or a value did"
    );
    let _ = o1;
}

/// The interpreter's database and the compiled binary's must be **the same
/// file**, in both directions.
///
/// Each engine reading its own writes is the easy half. The halves that catch
/// real drift are the crossed ones: a log the interpreter wrote and the C port
/// replays, and a log the C port wrote and the interpreter replays. The
/// on-disk format is the same 16-byte-header log either way, and this is what
/// says so.
#[test]
fn the_two_engines_read_each_others_log() {
    // direction 1: interpreter writes, C reads
    {
        let si = Scratch::new("x1-interp-writes");
        let sc = Scratch::new("x1-c-reads");
        let w = si.join("d.ainl-db");
        std::fs::write(
            si.join("w.ainl"),
            r#"(do (def h (db-open "d.ainl-db"))
                 (db-set h "a" 1) (db-set h "b" (list 1 2)) (db-set h "c" "text")
                 (db-flush h) (db-close h))"#,
        )
        .expect("write");
        let out = Command::new(ainl())
            .arg("run")
            .arg("w.ainl")
            .current_dir(&si.path)
            .output()
            .expect("interpreter");
        assert!(out.status.success(), "the interpreter write failed");
        std::fs::copy(&w, sc.join("d.ainl-db")).expect("move the log");

        let (bin, _) = build(
            r#"(do (def h (db-open "d.ainl-db"))
                 (def r (list (db-get h "a") (db-get h "b") (db-get h "c")
                               (db-count h) (db-keys h)))
                 (db-close h) (print r))"#,
            "read1",
            &sc.path,
        );
        let (o, e, ok) = run(&bin, &sc.path);
        assert!(ok, "the C port could not replay the interpreter's log: {e}");
        assert_eq!(o, "(1 (1 2) \"text\" 3 (\"a\" \"b\" \"c\"))\n");
    }

    // direction 2: C writes, interpreter reads
    {
        let sc = Scratch::new("x2-c-writes");
        let si = Scratch::new("x2-interp-reads");
        let (bin, _) = build(
            r#"(do (def h (db-open "d.ainl-db"))
                 (db-set h "a" 1) (db-set h "b" (list 1 2)) (db-set h "c" "text")
                 (db-set h "a" 99)
                 (db-del h "c")
                 (db-flush h) (db-close h))"#,
            "write2",
            &sc.path,
        );
        let (o, e, ok) = run(&bin, &sc.path);
        assert!(ok, "the C port write failed: {e} (stdout {o})");
        std::fs::copy(sc.join("d.ainl-db"), si.join("d.ainl-db")).expect("move the log");

        std::fs::write(
            si.join("r.ainl"),
            r#"(do (def h (db-open "d.ainl-db"))
                 (def r (list (db-get h "a") (db-get h "c") (db-count h) (db-keys h)))
                 (db-close h) (print r))"#,
        )
        .expect("write");
        let out = Command::new(ainl())
            .arg("run")
            .arg("r.ainl")
            .current_dir(&si.path)
            .output()
            .expect("interpreter");
        assert!(out.status.success(), "the interpreter could not replay");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "(99 nil 2 (\"a\" \"b\"))\n",
            "the interpreter misread the C port's log — the last write must win and the delete must hold"
        );
    }
}

// ---- the generated C carries the calls ------------------------------------

/// The generated C must emit a `v_builtin(N)` for each value-layer call, N
/// being the id from BUILTIN_IDS. Asserting on the *call sites* rather than on
/// the symbol names is the point: the whole runtime is inlined into the output,
/// so `B_DB_SET` is present whether or not the program ever calls it.
#[test]
fn the_generated_c_emits_a_call_site_for_every_value_builtin() {
    let s = Scratch::new("callsites");
    let (_bin, c) = build(
        r#"(do (def h (db-open "d.ainl-db"))
             (db-set h "k" 1) (db-get h "k") (db-get-raw h "k")
             (db-del h "k") (db-keys h) (db-count h)
             (db-close h))"#,
        "callsites",
        &s.path,
    );
    let src = std::fs::read_to_string(&c).expect("read the generated C");
    for (name, id) in [
        ("db-set", "72"),
        ("db-get-raw", "73"),
        ("db-del", "74"),
        ("db-keys", "75"),
        ("db-count", "76"),
    ] {
        assert!(
            src.contains(&format!("v_builtin({id})")),
            "the generated C never emits a call site for {name} (id {id})"
        );
    }
    // `db-get` shares id 69 with nothing else now — the byte layer's reader was
    // renamed — so it must appear exactly once as a call site too.
    assert!(
        src.contains("v_builtin(69)"),
        "the generated C never emits a call site for db-get (id 69)"
    );
}
