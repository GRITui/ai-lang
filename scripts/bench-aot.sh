#!/usr/bin/env bash
# Measure the 40k sum-to-N loop across all engines, for docs/PERFORMANCE.md.
#
#   AOT       the compiled binary, whole process (what a user pays)
#   AOT floor the same binary shape running `(print 1)` — process startup cost
#   Rust      a hand-written native Rust equivalent of the same loop
#
# The in-process tree-walk / bytecode-VM numbers come from the test suite
# (vm_perf.rs, aot_perf.rs) — see the note printed at the end.
#
# Process-inclusive numbers come from scripts/execbench.c, which forks/execs
# the target directly. An external timer (python3 subprocess, /usr/bin/time)
# charges its own fork/exec + interpreter startup to the binary; on this
# machine that is ~27 ms, which is >10x the thing being measured.
#
# usage: ./scripts/bench-aot.sh [runs]
set -uo pipefail
cd "$(dirname "$0")/.."
RUNS="${1:-25}"
BIN=target/release/ainl
cargo build --release --quiet || exit 1
cc -O2 -o target/execbench scripts/execbench.c || exit 1
EB=target/execbench

W=$(mktemp -d); trap 'rm -rf "$W"' EXIT

cat > "$W/rs.rs" <<'EOF'
fn main() {
    let mut i: i64 = 0;
    let mut s: i64 = 0;
    while i < 40_000 { s += i; i += 1; }
    println!("{}", s);
}
EOF
cat > "$W/triv.rs" <<'EOF'
fn main() { println!("1"); }
EOF
rustc -O -o "$W/rs" "$W/rs.rs" 2>/dev/null || echo "note: rustc unavailable, skipping the Rust comparison"
rustc -O -o "$W/rstriv" "$W/triv.rs" 2>/dev/null

"$BIN" compile bench/loop40k.ainl -o "$W/loop.aot" >/dev/null || exit 1
printf '(print 1)\n' > "$W/triv.ainl"
"$BIN" compile "$W/triv.ainl" -o "$W/triv.aot" >/dev/null || exit 1

row() { # row <label> <cmd...>
  local label="$1"; shift
  if [ ! -x "$1" ]; then return; fi
  printf '%-24s %s\n' "$label" "$("$EB" "$RUNS" "$@")"
}

echo "40k sum-to-N loop ($RUNS runs each)"
echo
row "AOT (whole process)"   "$W/loop.aot"
row "AOT startup floor"     "$W/triv.aot"
if [ -x "$W/rs" ]; then
  row "Rust (whole process)" "$W/rs"
  row "Rust startup floor"   "$W/rstriv"
fi
echo
echo "In-process tree-walk / bytecode VM (no process startup, no link cost):"
cargo run --release --quiet -p ainl-cc --example baseline_probe 2>/dev/null || \
  echo "  cargo run --release -p ainl-cc --example baseline_probe"
