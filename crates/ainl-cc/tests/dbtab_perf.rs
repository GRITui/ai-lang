//! The 10k-row scale test, reported **separately** for the interpreter and for
//! the compiled AOT binary, as the card's PO decision requires.
//!
//! # Why two numbers and not one
//!
//! The two backends run two different B-trees: `ainl-core/src/btree.rs` and the
//! `dbt_*` hand-port in `ainl-cc/src/runtime.c`, with no FFI between them. They
//! do not need identical *constants* — one allocates a `Vec<String>` per node
//! and the other a malloc'd `char *`, and they were never going to match to the
//! nanosecond. What they have to agree on is the *results*, and
//! `dbtab_aot.rs` asserts that. This file measures the constants, separately,
//! because one averaged number would hide the thing worth seeing: the C port's
//! per-lookup cost against the interpreter's.
//!
//! # Why the probe loop is written *in* AINL
//!
//! The first version of this test emitted one source line per probe, and both
//! of its numbers were wrong for the same reason. A 4000-probe program is 4000
//! lines of source, so the run with probes pays for 4000 lines of **parsing**
//! that the run without probes does not, and the difference — which is the only
//! thing being timed — is parse time, not lookup time. It reported 2222
//! us/lookup for the interpreter and produced a 5.60x "regression to O(n)" for
//! the C port that was pure noise: 4000 probes is about 5ms of real work,
//! differentiated out of two process runs that each replayed a whole 10k-row
//! log. The tree was never the slow part.
//!
//! So the loop is a `while` in the program, with the probe key computed from
//! the loop counter: the program text is a fixed few lines whatever the probe
//! count, and the only thing that grows between the two runs is the number of
//! lookups. Measured on this machine with that shape, per-lookup cost is flat
//! across a 16x increase in rows (C port: 1.40 us at 10k, 1.47 at 40k, 1.48 at
//! 160k), which is what O(log n) looks like and is the claim under test.
//!
//! # Why the assertion is a ratio and not a wall-clock number
//!
//! A wall-clock threshold is a flaky test. The machine under CI is not this
//! machine, and a threshold tight enough to catch a regression to O(n) is a
//! threshold that fails on a loaded runner. So the *assertion* is the shape of
//! the curve — 4x the rows must not cost 4x the time — and the absolute numbers
//! are **printed** for the report and never asserted on. A regression to a
//! linear scan multiplies the per-lookup time by the row ratio, which no amount
//! of runner noise can hide.
//!
//! # Why the probe count is 50_000 and not more
//!
//! Two independent ceilings, and the smaller one wins. The interpreter has a
//! 2M eval-step budget (`MAX_STEPS` in `eval.rs`) and the probe loop costs
//! roughly 18 steps per iteration, so ~100_000 probes is where it stops with
//! "step limit exceeded" — measured, not guessed. Fifty thousand leaves headroom
//! and still puts ~350ms of lookup work against a ~60ms base, which is a signal
//! large enough to survive a loaded CI runner.
//!
//! # If you mutate `runtime.c` to check this test can fail
//!
//! `touch crates/ainl-cc/src/lib.rs` first. The runtime is pulled in with
//! `include_str!("runtime.c")` in `ainl-cc/src/lib.rs:260`, which changes what
//! the compiler *embeds* but is not by itself a reason to recompile, so an
//! edited `.c` can leave a stale CLI in `target/debug` — and a perf test then
//! measures the old tree and passes. That happened here: breaking `dbt_get` into
//! a linear scan reported 1.4 us/lookup and a passing test, and only after the
//! forced rebuild did the same mutation report 19.8 us/lookup at 10k and 73.2
//! at 40k, a 3.71x ratio, and fail. Card 4.1 documented this trap; it is
//! repeated here because it will otherwise cost the next person an hour of
//! "why is my mutation not failing".
//!
//! With the rebuild done, the mutation is caught: the assertion below fires on
//! the ratio, and the checksum still matches — a linear scan returns the right
//! *answers*, which is exactly why only a timing assertion catches it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

/// Lookups per measured run. Sized by the interpreter's step budget; see the
/// module note.
const PROBES: usize = 50_000;

/// The row counts. 10k is the figure the card asks for; 40k is 4x it, which for
/// an order-16 tree adds well under one level (4^(1/15) = 1.09) — so a correct
/// tree's per-lookup time barely moves, while a linear scan would multiply by
/// four. That gap is what the assertion below uses.
const SMALL_N: usize = 10_000;
const LARGE_N: usize = 40_000;

/// A scratch directory, unique per tag, removed on drop.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "ainl-dbtab-perf-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("make the scratch dir");
        Scratch { path: p }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn ainl() -> PathBuf {
    let mut p = std::env::current_exe().expect("test exe path");
    p.pop();
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

/// A program that bulk-loads `n` rows into `db` and closes.
fn load_src(n: usize, db: &str) -> String {
    format!(
        r#"(do
  (def h (db-open "{db}"))
  (def t (db-create-table h "t"))
  (def i 0)
  (while (< i {n})
    (db-insert h t (list (str i "-" "-" "-" "-" "-" "-") i))
    (def i (+ i 1)))
  (db-flush h)
  (db-close h))"#
    )
}

/// A program that reopens `db` and does `probes` lookups, printing a checksum.
///
/// **The loop is in the program**, which is the whole point of this file's
/// module note. `probes` is a loop bound, not a line count, so the base run
/// (0 probes) and the measured run differ only in how many lookups happen.
///
/// The key is `(mod i n)`, so the probes walk the key space repeatedly and
/// evenly rather than once each. That matters for two reasons: a stride that
/// only ever landed in one subtree would flatter a deep tree, and
/// `(mod i n)` sums to a value the test can predict exactly, so a missed
/// lookup is caught rather than averaged away.
///
/// AINL has no padding format, so `k0000042` cannot be built in-program; the
/// key is the decimal number followed by six dashes, which is unique per row
/// and sorts in **insertion** order. Left-to-right filling splits most often,
/// so it is the case worth measuring.
fn probe_src(n: usize, probes: usize, db: &str) -> String {
    format!(
        r#"(do
  (def h (db-open "{db}"))
  (def t (db-create-table h "t"))
  (def acc 0)
  (def i 0)
  (while (< i {probes})
    (def acc (+ acc (nth (db-select h t (str (mod i {n}) "-" "-" "-" "-" "-" "-")) 1)))
    (def i (+ i 1)))
  (print acc)
  (db-close h))"#
    )
}

/// The checksum `(mod i n)` produces over `probes` iterations.
///
/// Computed here rather than only compared between the two engines, because
/// agreement between two engines is not evidence that either found a row: a
/// key rule that missed on both sides would sum zero on both sides and look
/// perfect. Against the expected series, a miss shows up as a smaller number.
fn expected_sum(n: usize, probes: usize) -> u64 {
    let n = n as u64;
    let probes = probes as u64;
    let full = probes / n;
    let rem = probes % n;
    // `rem - 1` is written as a saturating subtraction because an exact multiple
    // of `n` leaves `rem == 0`, and `0u64 - 1` is a panic in a debug build
    // rather than the zero the second term obviously is.
    full * (n * (n - 1) / 2) + rem.saturating_sub(1) * rem / 2
}

fn best_of<T: Clone>(runs: usize, mut f: impl FnMut() -> (f64, T)) -> (f64, T) {
    let mut best = f64::MAX;
    let mut keep: Option<T> = None;
    for _ in 0..runs {
        let (t, v) = f();
        if t < best {
            best = t;
            keep = Some(v);
        }
    }
    (best, keep.expect("at least one run"))
}

/// Per-lookup microseconds from two whole-process timings.
///
/// A raw `probed - base` is the right idea and a fragile one: the two runs
/// differ in the lookup loop *only*, so the difference should be the loop's
/// cost, but on a shared or throttled runner the base run can come out slightly
/// slower than the measured one, and the subtraction then goes negative. A
/// negative per-lookup cost is not a measurement — it is the noise floor being
/// larger than the signal — and returning it would make the ratio assertion
/// below compare two negative numbers and sail under its bound. So a negative
/// delta is reported as NaN, which every comparison in the caller rejects, and
/// the assertion fires with a message naming the cause instead of a nonsense
/// ratio.
///
/// A NaN is also the honest answer for the *report*: a per-lookup cost that
/// cannot be separated from process startup is not a number worth printing.
fn per_lookup(probed: f64, base: f64, probes: usize) -> f64 {
    if probed <= base {
        return f64::NAN;
    }
    (probed - base) / probes as f64 * 1e6
}

/// Microseconds per lookup in the interpreter, by differencing two runs.
///
/// Returns `(per_lookup_us, checksum)`.
fn measure_interp(n: usize, probes: usize, dir: &Path) -> (f64, String) {
    let db = dir.join("i.ainl-db");
    let _ = std::fs::remove_file(&db);
    let dbp = db.to_string_lossy().into_owned();

    // Load once, in its own process, so it is in no measurement.
    let load = dir.join("load.ainl");
    std::fs::write(&load, load_src(n, &dbp)).expect("write");
    let out = Command::new(ainl())
        .arg("run")
        .arg(&load)
        .current_dir(dir)
        .output()
        .expect("run ainl");
    assert!(
        out.status.success(),
        "the load program failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let run_probe = |probes: usize| -> (f64, String) {
        let p = dir.join(format!("probe{probes}.ainl"));
        std::fs::write(&p, probe_src(n, probes, &dbp)).expect("write");
        best_of(3, || {
            let t = Instant::now();
            let out = Command::new(ainl())
                .arg("run")
                .arg(&p)
                .current_dir(dir)
                .output()
                .expect("run ainl");
            assert!(
                out.status.success(),
                "the probe program failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            (
                t.elapsed().as_secs_f64(),
                String::from_utf8_lossy(&out.stdout).trim().to_string(),
            )
        })
    };

    let (base, _) = run_probe(0);
    let (probed, sum) = run_probe(probes);
    (per_lookup(probed, base, probes), sum)
}

/// Microseconds per lookup in the compiled binary, by the same differencing.
///
/// The loader is compiled and run too, in its own process, so the measured
/// program is a reopen and a probe loop and nothing else.
fn measure_aot(n: usize, probes: usize, dir: &Path) -> (f64, String) {
    let db = dir.join("c.ainl-db");
    let _ = std::fs::remove_file(&db);
    let dbp = db.to_string_lossy().into_owned();

    let compile_and_run = |name: &str, src: &str| -> (f64, String) {
        let p = dir.join(format!("{name}.ainl"));
        std::fs::write(&p, src).expect("write");
        let bin = dir.join(format!("{name}.bin"));
        let out = Command::new(ainl())
            .arg("compile")
            .arg(&p)
            .arg("-o")
            .arg(&bin)
            .output()
            .expect("ainl compile");
        assert!(
            out.status.success(),
            "ainl compile refused {name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // Best of three: a compiled binary's first run pays for page faults the
        // file cache would not, and that cost is the same order as the thing
        // being measured.
        best_of(3, || {
            let t = Instant::now();
            let out = Command::new(&bin)
                .current_dir(dir)
                .output()
                .expect("run the binary");
            assert!(
                out.status.success(),
                "{name} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            (
                t.elapsed().as_secs_f64(),
                String::from_utf8_lossy(&out.stdout).trim().to_string(),
            )
        })
    };

    compile_and_run("load", &load_src(n, &dbp));
    let (base, _) = compile_and_run("probe0", &probe_src(n, 0, &dbp));
    let (probed, sum) = compile_and_run("probeN", &probe_src(n, probes, &dbp));
    (per_lookup(probed, base, probes), sum)
}

/// Every probe really found its row, on both engines, at both sizes.
///
/// This is the guard that makes the timings mean anything, and it is checked
/// against the **expected series**, not merely against the other engine. The
/// loader builds its keys in AINL (`(str i "-" ...)`) and the probe builds them
/// in AINL too, but the two rules still have to agree. If they did not, every
/// probe would miss — and a miss in a B-tree is a **shorter** walk than a hit,
/// because it stops at the leaf that should have held the key. So the error
/// would make the lookup look *faster* and flatter the reported numbers, which
/// is the worst direction for a benchmark to be wrong in.
#[test]
fn every_probe_finds_its_row_on_both_engines() {
    const N: usize = 2_000;
    const P: usize = 5_000;
    let s = Scratch::new("keys");
    let dbp = s.path.join("k.ainl-db").to_string_lossy().into_owned();

    let load = load_src(N, &dbp);
    let probe = probe_src(N, P, &dbp);
    let want = expected_sum(N, P).to_string();

    let lp = s.path.join("load.ainl");
    std::fs::write(&lp, &load).expect("write");
    let pp = s.path.join("probe.ainl");
    std::fs::write(&pp, &probe).expect("write");
    let bin = s.path.join("probe.bin");
    let out = Command::new(ainl())
        .arg("compile")
        .arg(&pp)
        .arg("-o")
        .arg(&bin)
        .output()
        .expect("ainl compile");
    assert!(
        out.status.success(),
        "ainl compile refused: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = Command::new(ainl())
        .arg("run")
        .arg(&lp)
        .current_dir(&s.path)
        .output()
        .expect("run ainl");
    assert!(
        out.status.success(),
        "the load failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    for (label, out) in [
        (
            "interpreter",
            Command::new(ainl())
                .arg("run")
                .arg(&pp)
                .current_dir(&s.path)
                .output()
                .expect("run ainl"),
        ),
        (
            "AOT C",
            Command::new(&bin)
                .current_dir(&s.path)
                .output()
                .expect("run"),
        ),
    ] {
        assert!(
            out.status.success(),
            "{label} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let got = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert_eq!(
            got, want,
            "{label}: the probe sum is {got}, expected {want} — not every key was \
             found, so the timings would be measuring misses"
        );
    }
}

/// The shape of the curve, on both engines, and the two numbers for the report.
///
/// The assertion is the ratio, not the absolute. 4x the rows, and a B-tree of
/// order 16 adds about *half* a level (4x the keys is 4^(1/15) = 1.09 levels,
/// so under one), so a correct tree moves the per-lookup time by very little
/// while a linear index would multiply it by 4. The bound of 3x is loose enough
/// that a loaded runner cannot trip it.
#[test]
fn lookup_time_grows_far_slower_than_the_row_count() {
    // --- interpreter -------------------------------------------------------
    let si = Scratch::new("interp-small");
    let (i_small, i_sum_small) = measure_interp(SMALL_N, PROBES, &si.path);
    let si2 = Scratch::new("interp-large");
    let (i_large, i_sum_large) = measure_interp(LARGE_N, PROBES, &si2.path);

    // --- compiled AOT ------------------------------------------------------
    let sc = Scratch::new("aot-small");
    let (c_small, c_sum_small) = measure_aot(SMALL_N, PROBES, &sc.path);
    let sc2 = Scratch::new("aot-large");
    let (c_large, c_sum_large) = measure_aot(LARGE_N, PROBES, &sc2.path);

    // --- every probe found its row -----------------------------------------
    assert_eq!(
        i_sum_small,
        expected_sum(SMALL_N, PROBES).to_string(),
        "the interpreter missed rows at 10k, so its timing is measuring misses"
    );
    assert_eq!(
        c_sum_small,
        expected_sum(SMALL_N, PROBES).to_string(),
        "the C port missed rows at 10k, so its timing is measuring misses"
    );
    assert_eq!(
        i_sum_large,
        expected_sum(LARGE_N, PROBES).to_string(),
        "the interpreter missed rows at 40k"
    );
    assert_eq!(
        c_sum_large,
        expected_sum(LARGE_N, PROBES).to_string(),
        "the C port missed rows at 40k"
    );

    // --- the report --------------------------------------------------------
    println!("\n--- lookup cost, reported separately per engine ---");
    println!("  {PROBES} probes over a table reopened from disk, best of 3");
    println!(
        "  interpreter : {:.3} us/lookup at {SMALL_N} rows, {:.3} us/lookup at {LARGE_N} rows",
        i_small, i_large
    );
    println!(
        "  AOT C       : {:.3} us/lookup at {SMALL_N} rows, {:.3} us/lookup at {LARGE_N} rows",
        c_small, c_large
    );
    println!(
        "  4x the rows changed the per-lookup time: interpreter {:.2}x, AOT C {:.2}x",
        i_large / i_small,
        c_large / c_small
    );
    println!(
        "  AOT C / interpreter at {SMALL_N} rows: {:.2}x",
        c_small / i_small
    );

    // --- the assertion -----------------------------------------------------
    // A NaN here means the two runs could not be separated: the noise floor was
    // larger than the lookup loop. That is checked first and on its own, because
    // every comparison against NaN is false, so a bare `ratio < 3.0` would
    // pass on a measurement that does not exist.
    for (label, small, large) in [
        ("interpreter", i_small, i_large),
        ("AOT C", c_small, c_large),
    ] {
        assert!(
            small.is_finite() && large.is_finite(),
            "{label}: the {PROBES}-probe run was not measurably slower than the \
             0-probe run ({small} / {large} us per lookup) -- process startup \
             noise is larger than the lookup loop, so this runner cannot \
             resolve a per-lookup cost. Raise PROBES."
        );
    }
    assert!(
        i_large / i_small < 3.0,
        "interpreter: 4x the rows changed the per-lookup time {:.2}x \
         ({i_small:.3} -> {i_large:.3} us) -- closer to linear than logarithmic",
        i_large / i_small
    );
    assert!(
        c_large / c_small < 3.0,
        "AOT C: 4x the rows changed the per-lookup time {:.2}x \
         ({c_small:.3} -> {c_large:.3} us) -- closer to linear than logarithmic",
        c_large / c_small
    );

    // --- and the two engines found the same rows --------------------------
    // The two timings are only comparable if the two B-trees were asked the same
    // question, and the checksum over every probe is what says so.
    assert_eq!(
        i_sum_small, c_sum_small,
        "the interpreter and the compiled binary summed different rows at {SMALL_N}, \
         so the two timings are not measuring the same work"
    );
    assert_eq!(
        i_sum_large, c_sum_large,
        "the interpreter and the compiled binary summed different rows at {LARGE_N}"
    );
    println!("  both engines: identical checksum over {PROBES} lookups at each size");
}
