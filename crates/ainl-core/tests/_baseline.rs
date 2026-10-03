//! Temporary baseline: time the current (tree-walk) run_str on the 40k loop.
use ainl_core::{BigNum, Value};
use std::time::Instant;

const N: i64 = 40_000;

#[test]
fn baseline_tree_walk_40k() {
    let src =
        format!("(def i 0)\n(def s 0)\n(while (< i {N})\n  (def s (+ s i))\n  (def i (+ i 1)))\ns");
    let v = ainl_core::run_str(&src).unwrap();
    assert_eq!(v, Value::Int(BigNum::small(799_980_000)));
    // warm
    for _ in 0..2 {
        let _ = ainl_core::run_str(&src).unwrap();
    }
    let mut best = std::time::Duration::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        let _ = ainl_core::run_str(&src).unwrap();
        best = std::cmp::min(best, t.elapsed());
    }
    eprintln!("BASELINE tree-walk 40k loop: {best:?}");
}
