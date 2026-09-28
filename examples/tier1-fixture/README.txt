Deterministic fixture for examples/stdlib.ainl's `list-dir` case.

The names are chosen so a *wrong* sort order is visible: byte order puts
"Capital" (C = 0x43) and "Under_score" (U = 0x55) and "beta" (b = 0x62) in
that sequence, where a case-insensitive or locale-aware collation would give
"beta, Capital, Under_score". scripts/check-transpile.sh and
scripts/check-aot.sh run that example against all four backends, so if any one
of them sorted differently the byte-equal comparison would fail.

A hidden file is included on purpose (AINL has no concept of a hidden file, and
a listing that silently dropped dotfiles would be a surprise), and "with
space.txt" checks that a name needing no escaping survives the round trip.
