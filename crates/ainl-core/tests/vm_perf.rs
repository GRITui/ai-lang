//! Performance test: the bytecode VM must be >=5x faster than the tree-walk
//! evaluator on a 40,000-iteration loop, with identical results.
//!
//! The loop is the integer sum-to-N benchmark from docs/PERFORMANCE.md:
//!     (def i 0) (def s 0) (while (< i 40000) (def s (+ s i)) (def i (+ i 1))) s
//! which sums 0..39999 = 799,980,000.

use ainl_core::{BigNum, Value};
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
    assert_eq!(
        tw_val,
        Value::Int(BigNum::small(EXPECTED)),
        "tree-walk result"
    );
    assert_eq!(vm_val, Value::Int(BigNum::small(EXPECTED)), "VM result");

    let speedup = tw_dur.as_secs_f64() / vm_dur.as_secs_f64();
    eprintln!("40k loop  tree-walk={tw_dur:?}  vm={vm_dur:?}  speedup={speedup:.2}x");
    assert!(
        speedup >= 5.0,
        "VM must be >=5x faster than tree-walk; got {speedup:.2}x \
         (tree-walk {tw_dur:?}, vm {vm_dur:?})"
    );
}

#[test]
fn the_vm_and_the_tree_walk_agree_on_json() {
    // The two in-process evaluators are separate code paths: the VM dispatches
    // a `Value::Builtin` by calling the stored closure, while the tree-walk
    // resolves the name in the environment. A builtin that only worked in one
    // of them would pass a perf test and fail a program, so the JSON pair is
    // checked on both.
    let cases = [
        r#"(json-parse "[1.5,2.5,true,null]")"#,
        r#"(json-parse "{\"b\":1.5,\"a\":[2.25]}")"#,
        r#"(json-serialize (list 1.5 2.5 "s" true nil))"#,
        r#"(json-serialize (hash "z" 1.5 "a" 2.5))"#,
        r#"(json-serialize (/ 1.0 3))"#,
        r#"(json-serialize 1e-7)"#,
        r#"(json-serialize (json-parse "\"\\u00e9\\ud83d\\ude00\\u0007\""))"#,
        // Errors agree too, by message — the stdlib rule covers stderr.
        r#"(json-parse "{")"#,
        r#"(json-serialize (hash 1.5 "v"))"#,
    ];
    for case in cases {
        let vm = ainl_core::run_str(case);
        let tw = ainl_core::run_in_tree_walk(case);
        match (&vm, &tw) {
            (Ok(a), Ok(b)) => assert_eq!(a, b, "`{case}` differs between the VM and the tree-walk"),
            (Err(a), Err(b)) => {
                let (a, b) = (a.to_string(), b.to_string());
                assert_eq!(a, b, "`{case}` fails differently in the two evaluators");
            }
            (v, t) => panic!(
                "`{case}` succeeded in one evaluator and failed in the other:\n\
                 vm={v:?}\ntree-walk={t:?}"
            ),
        }
    }
}
