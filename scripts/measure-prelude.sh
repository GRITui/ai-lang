#!/usr/bin/env bash
# MEASURE the two headline builtin numbers, so nobody has to do the arithmetic.
#
# 4.1's lesson, recorded in this repo: a card bumped the builtin count in the
# README and the site and got the second number wrong by hand. So the next card
# to touch the prelude runs this first, and the two numbers it prints are the
# two that go into README.md and site/index.html.
#
#   total    = every name the prelude binds
#   portable = total - the names some backend refuses
#
# Neither is a guess. The total comes from `eval::builtin_names()`, which
# installs the real prelude into a throwaway Env and reads the names back — the
# same function the "did you mean" suggester uses, so it cannot be out of step
# with what the language actually binds. The refused set comes from the lists
# the engines themselves publish, and each refusal list is subtracted *by name*
# rather than by length, because the two lists overlap on `db-get`: it is bound
# once and refused by the transpilers, so `total - refused.len()` would
# undercount the portable set by one.
set -uo pipefail
cd "$(dirname "$0")/.."
mkdir -p crates/ainl-core/examples

cat > crates/ainl-core/examples/prelude_probe.rs <<'EOF'
//! Print the two headline builtin counts, measured. Driven by
//! scripts/measure-prelude.sh, which writes and deletes this file.
fn main() {
    let names = ainl_core::eval::builtin_names();
    let refused: Vec<&str> = ainl_core::interpreter_only::INTERPRETER_ONLY
        .iter()
        .copied()
        .chain(ainl_core::db::DB_BUILTINS.iter().copied())
        .chain(ainl_core::dbkv::KV_BUILTINS.iter().copied())
        .chain(ainl_core::dbtab::TABLE_BUILTINS.iter().copied())
        .chain(ainl_core::dbquery::SQL_BUILTINS.iter().copied())
        .collect();
    let mut unique: Vec<&str> = refused.clone();
    unique.sort_unstable();
    unique.dedup();
    let real: Vec<&str> = unique
        .iter()
        .copied()
        .filter(|n| names.iter().any(|b| b == *n))
        .collect();
    let not_real: Vec<&str> = unique
        .iter()
        .copied()
        .filter(|n| !names.iter().any(|b| b == *n))
        .collect();

    println!("total    = {}", names.len());
    println!("refused  = {} ({})", real.len(), real.join(" "));
    println!("portable = {}", names.len() - real.len());
    if !not_real.is_empty() {
        // A name in a refusal list that the prelude never binds is usually a
        // *special form* rather than a builtin — `import` is the one today,
        // handled by the evaluator and so correctly absent from
        // `builtin_names()`. The line is printed rather than swallowed so that
        // a genuine typo in a refusal list is visible instead of quietly making
        // the portable number too high.
        println!(
            "not prelude bindings (special forms?): {}",
            not_real.join(" ")
        );
    }
}
EOF

cargo run --quiet -p ainl-core --example prelude_probe 2>&1 | tail -10
rm -f crates/ainl-core/examples/prelude_probe.rs
rmdir crates/ainl-core/examples 2>/dev/null
exit 0
