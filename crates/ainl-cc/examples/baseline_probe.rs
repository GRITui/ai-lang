//! In-process baseline: the AINL tree-walk vs the bytecode VM on the 40k
//! sum-to-N loop.
//!
//! These are the two numbers in docs/PERFORMANCE.md that `scripts/bench-aot.sh`
//! cannot produce: it measures *whole processes* (fork+exec+link), which is the
//! right way to time a compiled binary but hides the interpreter's in-process
//! cost. Run both to get the full picture on one machine:
//!
//!     cargo run --release -p ainl-cc --example baseline_probe
//!     ./scripts/bench-aot.sh
//!
//! Deliberately not a `#[test]`: this prints measurements rather than asserting
//! a gate, and the gating lives in `tests/aot_perf.rs` and
//! `crates/ainl-core/tests/vm_perf.rs`.

use std::time::Instant;

const N: i64 = 40_000;
const EXPECTED: &str = "799980000"; // 0 + 1 + ... + 39999

fn loop_src() -> String {
    format!("(def i 0)\n(def s 0)\n(while (< i {N})\n  (def s (+ s i))\n  (def i (+ i 1)))\ns\n")
}

fn main() {
    let src = loop_src();
    // Warm up: allocator, branch predictors, and lazily-initialized statics.
    let _ = ainl_core::run_str(&src).unwrap();
    let _ = ainl_core::run_in_tree_walk(&src).unwrap();

    let mut tw = f64::MAX;
    let mut vm = f64::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        let v = ainl_core::run_in_tree_walk(&src).unwrap();
        tw = tw.min(t.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(v.to_string(), EXPECTED, "tree-walk result");

        let t = Instant::now();
        let v = ainl_core::run_str(&src).unwrap();
        vm = vm.min(t.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(v.to_string(), EXPECTED, "bytecode VM result");
    }

    println!("40k sum-to-N loop, in-process, best of 5");
    println!("TREEWALK_MS={tw:.3}");
    println!("VM_MS={vm:.3}");
    println!("VM_SPEEDUP={:.2}x", tw / vm);
}
