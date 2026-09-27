//! Performance test: the bytecode VM must be >=5x faster than the tree-walk
//! evaluator on a 40,000-iteration loop, with identical results.
//!
//! The loop is the integer sum-to-N benchmark from docs/PERFORMANCE.md:
//!     (def i 0) (def s 0) (while (< i 40000) (def s (+ s i)) (def i (+ i 1))) s
//! which sums 0..39999 = 799,980,000.

use ainl_core::Value;
use std::time::{Duration, Instant};

const N: i64 = 40_000;
const EXPECTED: i64 = 799_980_000; // 0 + 1 + ... + 39999

fn loop_src() -> String {
    format!("(def i 0)\n(def s 0)\n(while (< i {N})\n  (def s (+ s i))\n  (def i (+ i 1)))\ns")
}

/// Warm up, then run `runs` times and return (last value, best duration).
fn best_of<F: FnMut() -> Value>(runs: usize, mut f: F) -> (Value, Duration) {
    let mut best = Duration::MAX;
    let mut last = Value::Nil;
    for _ in 0..runs {
        let t = Instant::now();
        last = f();
        let d = t.elapsed();
        if d < best {
            best = d;
        }
    }
    (last, best)
}

#[test]
fn vm_is_faster_than_tree_walk_on_40k_loop() {
    let src = loop_src();

    // Warm up both paths (JIT/allocator/branch-predictor stabilization).
    let _ = ainl_core::run_str(&src).unwrap();
    let _ = ainl_core::run_in_tree_walk(&src).unwrap();

    let (tw_val, tw_dur) = best_of(3, || ainl_core::run_in_tree_walk(&src).unwrap());
    let (vm_val, vm_dur) = best_of(3, || ainl_core::run_str(&src).unwrap());

    // Identical results.
    assert_eq!(tw_val, Value::Int(EXPECTED), "tree-walk result");
    assert_eq!(vm_val, Value::Int(EXPECTED), "VM result");

    let speedup = tw_dur.as_secs_f64() / vm_dur.as_secs_f64();
    eprintln!("40k loop  tree-walk={tw_dur:?}  vm={vm_dur:?}  speedup={speedup:.2}x");
    assert!(
        speedup >= 5.0,
        "VM must be >=5x faster than tree-walk; got {speedup:.2}x \
         (tree-walk {tw_dur:?}, vm {vm_dur:?})"
    );
}
