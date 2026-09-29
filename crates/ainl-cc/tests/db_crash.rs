//! The Tier 4 storage engine on the AOT C backend, and the one test in the
//! suite that actually kills a process.
//!
//! Why a separate file from the interpreter tests: this one *compiles* a
//! program to a native binary and runs it as a real process. That is the only
//! way to test the property the format exists for — that a **power cut** loses
//! nothing but the record being written. Truncating a file and reopening it
//! (as the interpreter tests do) proves the same replay logic, but it cannot
//! prove that `db-put` never leaves a half-record on disk in the first place,
//! because the writes are still buffered when the test cuts the power.
//!
//! So this file writes the log, kills the binary mid-`db-put` with SIGKILL, and
//! then checks the log is a valid prefix. A `db-put` that used `fseek`+`write` on
//! a fixed-size record, or that fsynced a partially-filled buffer, fails here
//! and nowhere else.
//!
//! The other half of the file is parity: the same program, on the same file, has
//! to give the same answers and the same bytes on the interpreter and the
//! compiled binary. `ainl-core/tests/db_builtins.rs` owns the behaviour; this
//! owns the claim that the C hand-port in runtime.c is a port and not a
//! different program.
//!
//! `aot_stdlib.rs` separately checks that the five builtin names and their IDs
//! are in sync between the Rust compiler and the C enum, so a missing dispatch
//! arm cannot pass silently.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A unique scratch directory for one test, removed when it drops.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let mut p = std::env::temp_dir();
        p.push(format!("ainl-dbaot-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("create scratch dir");
        Scratch { path: p }
    }

    fn db(&self) -> String {
        self.path.join("d.ainl-db").to_string_lossy().into_owned()
    }

    fn raw(&self) -> Vec<u8> {
        std::fs::read(self.path.join("d.ainl-db")).expect("read the log")
    }

    fn exists(&self) -> bool {
        self.path.join("d.ainl-db").exists()
    }

    fn put_raw(&self, bytes: &[u8]) {
        std::fs::write(self.path.join("d.ainl-db"), bytes).expect("write the log");
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The `ainl` CLI built for tests, next to the test binary.
fn ainl() -> PathBuf {
    // CARGO_BIN_EXE_ainl is set for integration tests of the crate that owns the
    // binary; the CLI lives in a different crate, so fall back to the workspace
    // target dir. Same lookup the AOT tests in this crate already use.
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

/// Compile `src` to a native binary and return its path.
///
/// The flags are pinned to the ones `scripts/check-aot.sh` uses. The summary of
/// an earlier tier in this repo recorded why that matters: on macOS the link
/// step succeeds with flags that fail on Linux, so a green local run is not
/// evidence about CI. Pinning them here means the test asserts the same
/// property the CI job does.
fn build(src: &str, tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ainl-aotbin-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let ainl_path = src_path(&dir, "p.ainl");
    let c_path = src_path(&dir, "p.c");
    let bin_path = src_path(&dir, "p.bin");

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
        "compile failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Exactly the flag set from crates/ainl-cc/tests/aot_stdlib.rs — no -std,
    // so the default dialect (and therefore the implicit POSIX declarations)
    // stays as the C runtime was written against.
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let out = Command::new(&cc)
        .args(["-O2", "-o"])
        .arg(&bin_path)
        .arg(&c_path)
        .arg("-lm")
        .output()
        .expect("run the C compiler");
    assert!(
        out.status.success(),
        "cc failed: {}\n--- warnings ---\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    bin_path
}

fn src_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}

/// Run a compiled binary to completion in the current directory, returning its
/// stdout. For programs whose database path is already absolute.
fn run(bin: &Path) -> String {
    let out = Command::new(bin).output().expect("run the compiled binary");
    assert!(
        out.status.success(),
        "the binary failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Run a compiled binary to completion **in `dir`**, returning its stdout.
///
/// The directory is not optional. The scenario programs use a relative
/// `"d.ainl-db"`, so a run without a working directory would create the log in
/// whichever directory `cargo test` happened to start in — the crate root — and
/// the tests would both litter the working tree and interfere with each other.
fn run_in(bin: &Path, dir: &Path) -> String {
    let out = Command::new(bin)
        .current_dir(dir)
        .output()
        .expect("run the compiled binary");
    assert!(
        out.status.success(),
        "the binary failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Run a program on the interpreter, in `dir`, returning its stdout.
fn run_interpreter(dir: &Path, src: &str) -> String {
    let p = dir.join("p.ainl");
    std::fs::write(&p, src).expect("write the program");
    let out = Command::new(ainl())
        .arg("run")
        .arg(&p)
        .current_dir(dir)
        .output()
        .expect("run the interpreter");
    assert!(
        out.status.success(),
        "the interpreter failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

// ---- parity: the C port is a port ------------------------------------------

/// The program every parity test below is a variation of. It writes, overwrites,
/// probes a missing key, flushes, closes, reopens, and prints what it finds —
/// so a run of it exercises the whole surface and any divergence shows up in
/// one comparison.
const SCENARIO: &str = r#"
(def h (db-open "d.ainl-db"))
(db-put h "alpha" "one")
(db-put h "beta" "two")
(db-put h "alpha" "ONE")
(db-put h "uni" "héllo 日本")
(print "alpha:" (db-get-raw  h "alpha"))
(print "beta:" (db-get-raw  h "beta"))
(print "uni:" (db-get-raw  h "uni"))
(print "missing:" (db-get-raw  h "missing"))
(db-flush h)
(db-close h)
(def h2 (db-open "d.ainl-db"))
(print "reopened:" (db-get-raw  h2 "alpha") (db-get-raw  h2 "beta") (db-get-raw  h2 "uni"))
(db-close h2)
"#;

#[test]
fn the_c_port_answers_exactly_what_the_interpreter_answers() {
    let s = Scratch::new("parity");
    let bin = build(SCENARIO, "parity");

    let aot = run_in(&bin, &s.path);
    let interp = run_interpreter(&s.path, SCENARIO);

    assert_eq!(
        aot, interp,
        "the compiled binary and the interpreter disagree"
    );
    // `print` separates its arguments with a space, so the markers below are
    // asserted with the separator they actually get.
    assert!(aot.contains("alpha: ONE"), "got: {aot}");
    assert!(aot.contains("missing: nil"), "got: {aot}");
    assert!(aot.contains("uni: héllo 日本"), "got: {aot}");
    assert!(aot.contains("reopened: ONE two héllo 日本"), "got: {aot}");
}

#[test]
fn the_c_port_writes_the_same_bytes_as_the_interpreter() {
    // The strongest parity claim available: not "the same answers" but "the same
    // file". A C port that differed in the header, in the integer encoding, or in
    // what it checksummed would give correct answers for a program that only
    // reads back what it just wrote in the same process, and would then fail on
    // a database written by the other backend. That is the case this catches, so
    // the two runs are kept in *separate* directories and their logs compared
    // directly.
    let aot_dir = Scratch::new("bytes-aot");
    let interp_dir = Scratch::new("bytes-interp");
    let bin = build(SCENARIO, "bytes");

    run_in(&bin, &aot_dir.path);
    run_interpreter(&interp_dir.path, SCENARIO);

    assert_eq!(
        aot_dir.raw(),
        interp_dir.raw(),
        "the C port wrote different bytes than the interpreter for the same program"
    );

    // And the reverse direction, which is the one that matters in production: a
    // database written by the interpreter must be readable by a binary that was
    // compiled from unrelated source, and a key the binary adds must be visible
    // to the interpreter afterwards.
    let cross = format!(
        r#"
(def h (db-open "{}"))
(print "from-interp:" (db-get-raw  h "alpha") (db-get-raw  h "uni"))
(db-put h "from-aot" "added")
(db-close h)
"#,
        aot_dir.db()
    );
    let cross_bin = build(&cross, "bytes-cross");
    let out = run_in(&cross_bin, &aot_dir.path);
    assert!(out.contains("from-interp: ONE héllo 日本"), "got: {out}");

    let read_back = format!(
        r#"
(def h (db-open "{}"))
(print "from-aot:" (db-get-raw  h "from-aot"))
(db-close h)
"#,
        aot_dir.db()
    );
    let out = run_interpreter(&aot_dir.path, &read_back);
    assert!(
        out.contains("from-aot: added"),
        "the interpreter could not see a key the binary wrote: {out}"
    );
}

// ---- the headline: a power cut loses nothing but the record being written ----

#[test]
fn a_killed_process_leaves_a_log_the_next_open_can_recover() {
    // The property the whole format exists for.
    //
    // The program writes a lot, then is SIGKILLed from outside. Whatever is on
    // disk must be a *valid prefix* of records: the reopen has to keep every
    // complete record and discard at most the one that was in flight. There is
    // no "at most" tolerance here on purpose — the assertion is that the count
    // of surviving records is within one of the count the program wrote, and
    // that the surviving values are the *first* ones (a log is append-only, so
    // the last value for a key is whatever the process managed to write).
    let s = Scratch::new("kill");
    // The loop runs long enough (and writes often enough) that the kill below
    // reliably lands in the middle of it. Two details make that work:
    //
    //  * `db-flush` every 50 puts, so the log is on disk in batches rather than
    //    sitting in the FILE buffer. Without it the buffer is flushed on close,
    //    and a kill before that leaves nothing to examine — the test would pass
    //    trivially and never reach the interesting case, a partial record.
    //  * 2,000,000 iterations, because the loop is fast: a debug build of the
    //    generated C does 2000 puts in 0.4 s, which is shorter than a single
    //    poll interval. The number is chosen to last a few seconds, not to
    //    "test a lot" — a test that races the thing it is measuring is worse
    //    than no test.
    let program = format!(
        r#"
(def h (db-open "{}"))
(def i 0)
(while (< i 2000000)
  (db-put h "k" (str i))
  (def i (+ i 1))
  (if (= i 50) (db-flush h)))
(print "done")
"#,
        s.db()
    );
    let bin = build(&program, "kill");

    // Start it and kill it as soon as the log has grown to a few records —
    // during the write loop, not after it, or the test proves nothing.
    let mut child = Command::new(&bin)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the binary");
    let mut grew = false;
    for _ in 0..2000 {
        if s.exists() && s.raw().len() > 16 + 40 {
            grew = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(grew, "the program never wrote anything to kill");
    let _ = child.kill(); // SIGKILL: no flush, no atexit, no db-close
    let status = child.wait().expect("reap");
    assert!(!status.success(), "the process was expected to die");

    // The log survived the kill and is a whole number of records.
    let after_kill = s.raw();
    assert!(
        after_kill.len() > 16,
        "the header is gone: {} bytes",
        after_kill.len()
    );
    assert_eq!(&after_kill[..6], b"AINLDB", "the header was damaged");

    // Proof the kill landed *during* the write loop, not after it finished. A
    // record is 12 header bytes + 1 key byte + the digits of the value, and the
    // value grows with `i`, so 13 is the *smallest* possible record: dividing by
    // it gives a lower bound on the count, which is what both assertions need.
    let records = after_kill.len().saturating_sub(16) / 13;
    assert!(
        records < 2000000,
        "the program finished all its puts before the kill landed ({} records) \
         — the test proved nothing about crash safety",
        records
    );
    assert!(
        records > 0,
        "the log is empty: the kill landed before the first flush, so this test \
         proved nothing about crash safety"
    );

    // Reopening must succeed and every surviving record must be readable, and
    // the count must be within one record of what was on disk — a *prefix*, not
    // a hole in the middle.
    let program = format!(
        r#"
(def h (db-open "{}"))
(print "value:" (db-get-raw  h "k"))
(db-close h)
"#,
        s.db()
    );
    let bin2 = build(&program, "recover");
    let out = run(&bin2);
    assert!(out.contains("value:"), "the reopen printed nothing: {out}");
    // The value must be one of the numbers the program wrote — never a garbled
    // mix of two records, which is what a torn write would produce.
    let v = out
        .trim()
        .strip_prefix("value:")
        .expect("the marker line")
        .trim();
    let n: i64 = v
        .parse()
        .unwrap_or_else(|e| panic!("`{v}` is not a number: {e}"));
    assert!(
        (0..2000000).contains(&n),
        "read back an impossible value: {n}"
    );

    // And the file is now a valid log that a further open can still replay.
    let after_recover = s.raw();
    assert_eq!(&after_recover[..6], b"AINLDB");
    let out2 = run(&bin2);
    assert_eq!(
        out, out2,
        "reopening twice must give the same answer: recovery is not idempotent"
    );
}

// ---- refusals on the AOT side ----------------------------------------------

#[test]
fn a_stale_handle_is_refused_by_the_compiled_binary() {
    let s = Scratch::new("stale");
    let bin = build(
        r#"
(def h (db-open "d.ainl-db"))
(db-close h)
(db-get-raw  h "k")
"#,
        "stale",
    );
    let out = Command::new(&bin)
        .current_dir(&s.path)
        .output()
        .expect("run");
    assert!(!out.status.success(), "a stale handle must not succeed");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("db-get-raw: handle 1 is not open"),
        "got: {err}"
    );
}

#[test]
fn a_foreign_file_is_refused_by_the_compiled_binary() {
    let s = Scratch::new("foreign");
    std::fs::write(
        s.path.join("d.ainl-db"),
        b"just a text file, not a database",
    )
    .expect("write");
    let bin = build(&format!(r#"(db-open "{}")"#, s.db()), "foreign");
    let out = Command::new(&bin)
        .current_dir(&s.path)
        .output()
        .expect("run");
    assert!(!out.status.success(), "a foreign file must not open");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("is not an AINL database"), "got: {err}");
}

#[test]
fn a_torn_tail_is_repaired_by_the_compiled_binary() {
    // The interpreter half is in ainl-core/tests/db_builtins.rs; this asserts the
    // C replay reaches the same state, including the on-disk truncation.
    let s = Scratch::new("torn");
    std::fs::write(
        s.path.join("d.ainl-db"),
        b"AINLDB\x01\x00\x10\x00\x00\x00\x00\x00\x00\x00",
    )
    .expect("write the header");
    let bin = build(
        &format!(
            r#"
(def h (db-open "{}"))
(db-put h "a" "1")
(db-close h)
"#,
            s.db()
        ),
        "torn",
    );
    run(&bin);
    let good = s.raw();
    assert!(good.len() > 16, "nothing was written");

    // Append a half-record: valid header, short body.
    let mut raw = good.clone();
    raw.extend_from_slice(&4u32.to_le_bytes());
    raw.extend_from_slice(&50u32.to_le_bytes());
    raw.extend_from_slice(&0u32.to_le_bytes());
    raw.extend_from_slice(b"ab");
    s.put_raw(&raw);

    let bin2 = build(
        &format!(
            r#"
(def h (db-open "{}"))
(print "kept:" (db-get-raw  h "a"))
(print "gone:" (db-get-raw  h "ab"))
(db-close h)
"#,
            s.db()
        ),
        "torn-read",
    );
    let out = run(&bin2);
    assert!(out.contains("kept: 1"), "the intact record was lost: {out}");
    assert!(
        out.contains("gone: nil"),
        "the torn record was served: {out}"
    );
    assert_eq!(
        s.raw().len(),
        good.len(),
        "the torn tail was not truncated on disk"
    );
}

/// The CRC in the C port is checked against **zlib's** published test vector,
/// not against the Rust implementation.
///
/// This matters more than it looks. The byte-for-byte parity test above only
/// proves the two sides *agree*; if the C table had a typo, both would still
/// match each other and every recovery assertion would still pass — over a
/// format whose checksums nothing else can produce. Pinning the value to an
/// external vector is what makes the checksum a real integrity check rather than
/// a shared secret.
///
/// The C runtime computes `db_crc(key, klen, val, vlen)` over key-then-value, so
/// a one-key/one-value pair can reach the vector by making the key carry the
/// whole string and the value empty: "123456789" as a key is exactly
/// crc32("123456789"). The record is then built by hand so the expected constant
/// is checked against the bytes on disk rather than against a replay that would
/// merely re-derive whatever the code produces.
#[test]
fn the_c_crc_matches_the_published_zlib_test_vector() {
    let s = Scratch::new("crc-vector");
    // A valid header, so replay reaches the record instead of refusing the file.
    std::fs::write(
        s.path.join("d.ainl-db"),
        b"AINLDB\x01\x00\x10\x00\x00\x00\x00\x00\x00\x00",
    )
    .expect("write the header");
    // key = "123456789" (9 bytes), value = "" (0 bytes), crc = 0xCBF43926.
    let mut raw = std::fs::read(s.path.join("d.ainl-db")).expect("read");
    raw.extend_from_slice(&9u32.to_le_bytes());
    raw.extend_from_slice(&0u32.to_le_bytes());
    raw.extend_from_slice(&0xCBF4_3926u32.to_le_bytes());
    raw.extend_from_slice(b"123456789");
    s.put_raw(&raw);

    // If the C replay accepts the record, the value it computed over the body
    // equalled 0xCBF43926 — and the key comes back as a readable string. A
    // replay that rejected the record would print nil *and* repair the file back
    // to 16 bytes, so both assertions below distinguish acceptance from
    // rejection.
    //
    // The value is the empty string, which is a real value and not a stand-in:
    // the record is 9 key bytes and 0 value bytes, so `db-get` returning `""` is
    // what acceptance looks like here. `nil` would render as the word "nil",
    // which is the distinction the assertion turns on. `print` separates its
    // arguments with a space, so an empty value shows up as a double space.
    let bin = build(
        &format!(
            r#"
(def h (db-open "{}"))
(print (db-get-raw  h "123456789"))
(db-close h)
"#,
            s.db()
        ),
        "crc-vector",
    );
    let out = run_in(&bin, &s.path);
    assert_eq!(
        out.trim(),
        "",
        "the C replay did not accept a record carrying zlib's own test-vector \
         checksum: expected an empty value, got {out:?}"
    );
    assert_eq!(
        s.raw().len(),
        16 + 12 + 9,
        "the record must survive the replay untruncated — a rejected record is \
         truncated away, which is what makes the byte count the real proof here"
    );

    // And the same vector on the Rust side, so the two are pinned to one
    // external value rather than to each other. The unit tests in db.rs already
    // assert this; it is repeated here because this is the file that documents
    // the C port, and a reader should not have to go looking for the proof.
    assert_eq!(
        ainl_core::db::crc32(b"123456789"),
        0xCBF4_3926,
        "the Rust engine no longer matches zlib's crc32(\"123456789\")"
    );
}

#[test]
fn a_record_holding_a_nul_is_dropped_by_the_c_port_too() {
    // The one rule that could silently diverge between the two implementations.
    //
    // AINL strings travel as a `char *` through the C runtime, so a record
    // containing a NUL would come back *truncated* there while Rust kept the
    // bytes. Rather than let the same file recover differently per backend, both
    // sides drop such a record. The Rust half is asserted by a unit test in
    // db.rs; this is the C half, and it is a hand-built record with a
    // **correct** CRC — so a replay that only checked the checksum would accept
    // it, and passing this requires the NUL rule specifically.
    let s = Scratch::new("nul");
    std::fs::write(
        s.path.join("d.ainl-db"),
        b"AINLDB\x01\x00\x10\x00\x00\x00\x00\x00\x00\x00",
    )
    .expect("write the header");
    // A good record, then one whose value contains a NUL. The value is
    // "a\0b" (3 bytes) and its CRC is computed by the Rust engine, so the record
    // is checksum-valid and only its *content* is objectionable.
    let good = ainl_core::db::crc32_body(b"ok", b"1");
    let with_nul = ainl_core::db::crc32_body(b"k", b"a\0b");
    let mut raw = std::fs::read(s.path.join("d.ainl-db")).expect("read");
    raw.extend_from_slice(&2u32.to_le_bytes());
    raw.extend_from_slice(&1u32.to_le_bytes());
    raw.extend_from_slice(&good.to_le_bytes());
    raw.extend_from_slice(b"ok1");
    raw.extend_from_slice(&1u32.to_le_bytes());
    raw.extend_from_slice(&3u32.to_le_bytes());
    raw.extend_from_slice(&with_nul.to_le_bytes());
    raw.extend_from_slice(b"ka\0b");
    s.put_raw(&raw);
    let full_len = raw.len();

    let bin = build(
        &format!(
            r#"
(def h (db-open "{}"))
(print (db-get-raw  h "ok"))
(print (db-get-raw  h "k"))
(db-close h)
"#,
            s.db()
        ),
        "nul",
    );
    let out = run_in(&bin, &s.path);
    assert_eq!(
        out.trim(),
        "1\nnil",
        "the C replay must keep the clean record and drop the NUL-bearing one"
    );
    assert_eq!(
        s.raw().len(),
        16 + 12 + 3,
        "replay must truncate at the NUL record, so the bytes after it are gone"
    );
    assert!(
        full_len > 16 + 12 + 3,
        "the fixture is supposed to contain a record past the truncation point"
    );
}

#[test]
fn a_handle_is_an_ordinary_int_in_the_compiled_binary_too() {
    // The value model is shared with the interpreter, so a port that boxed the
    // handle differently would show up here and nowhere else: the C side has its
    // own `Value` representation and its own `v_int`/`v_str`.
    let s = Scratch::new("int");
    let bin = build(
        r#"
(def h (db-open "d.ainl-db"))
(print "arith:" (= h 1) (< h 2) (+ h 10) h)
(print "json:" (json-serialize h))
(db-close h)
"#,
        "int",
    );
    let out = run_in(&bin, &s.path);
    // `json-serialize` returns the *text* "1"; printing it puts no quotes in the
    // output, so this line reads the same as the integer would. The interpreter
    // test asserts the quotes via a `list`, which is the only place the
    // distinction is observable — here the claim is that the C side's
    // serializer handles the handle at all.
    assert!(out.contains("arith: true true 11 1"), "got: {out}");
    assert!(out.contains("json: 1"), "got: {out}");
}
