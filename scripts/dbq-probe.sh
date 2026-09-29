#!/usr/bin/env bash
# Compare interpreter and AOT stderr for a query. Both must print the same
# bytes. Used to probe parity by hand while the C port is being written.
set -u
ainl=/Users/grit/.hermes/profiles/product-owner/cache/scratch/ai-lang/target/debug/ainl
q="$1"
for eng in interp aot; do
  d=$(mktemp -d)
  cat > $d/p.ainl <<EOF
(do
  (def h (db-open "t.db"))
  (def t (db-create-table h "people"))
  (db-insert h t (list "ada" 36 "math"))
  (db-insert h t (list "bob" 41 "navy"))
  (print (db-query h "$q")))
EOF
  if [ "$eng" = aot ]; then
    (cd $d && $ainl compile p.ainl -o p.bin >/dev/null 2>&1 && ./p.bin 2>&1 >/dev/null) | sed "s/^/[$eng] /"
  else
    (cd $d && $ainl run p.ainl 2>&1 >/dev/null) | sed "s/^/[$eng] /"
  fi
  rm -rf $d
done
