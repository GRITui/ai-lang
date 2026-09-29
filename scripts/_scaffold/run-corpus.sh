#!/usr/bin/env bash
# Run the storage corpus example, because it is few-shot material and a broken
# example teaches the wrong thing.
set -uo pipefail
# Two levels up: this script lives in scripts/_scaffold/.
cd "$(dirname "$0")/../.."
A="$PWD/target/release/ainl"
[ -x "$A" ] || { echo "building ainl (release)..."; cargo build --release --quiet; }
D=$(mktemp -d)
trap 'rm -rf "$D"' EXIT
cp examples/corpus/storage.ainl "$D/"
cd "$D"
"$A" run storage.ainl
