#!/usr/bin/env bash
# Verify every claim made in docs/SYNTAX.md section 3n (db-query / db-query-count)
# against the real binary.
#
# The sibling of check-docs-db.sh. A query section is almost entirely about what
# the layer *refuses* and *orders* — a stable sort, an enforced clause order, a
# named refusal for every out-of-scope keyword, nil in the projection but not in
# ORDER BY — and every one of those is a claim a unit test cannot see drift on.
# The tests check the engine; this checks that the docs still describe it.
#
# The error strings are asserted **exactly**, and not by substring where it can
# be avoided, because they are part of the documented contract: a model writing
# the next query reads them, and `try`/`catch` callers match on them.
#
# The cross-backend half — the C hand-port, and the transpiler refusals — lives
# in `dbq_aot.rs` and in the final block here respectively. This script is the
# documentation half.
set -uo pipefail
cd "$(dirname "$0")/.."
B=./target/release/ainl
# The release binary is only trusted when it is at least as new as the sources;
# otherwise a stale build becomes the reference and every claim "fails".
if [ -x "$B" ] && [ target/debug/ainl -nt "$B" ]; then
  B=./target/debug/ainl
fi
[ -x "$B" ] || { echo "building ainl (release)..."; cargo build --release --quiet; }
D=$(mktemp -d)
trap 'rm -rf "$D"' EXIT
fail=0

# The three-row fixture every case shares. Ada/bob/grace is chosen so the
# interesting orderings are reachable in three rows: ages 36/41/45 sort
# differently from the primary keys, and two rows share the department "navy",
# so a stable sort is observable without a fourth row.
#
# `q <query>` runs one query and prints its result. `bad <query>` runs one query
# that must be refused and prints the message with the call-site position
# stripped, because the interpreter appends ` at line N, col M (byte B)` naming
# the call in the *AINL* source and the docs never show that part.
fixture() {
  cat <<EOF
(do
  (def h (db-open "t.ainl-db"))
  (def t (db-create-table h "people"))
  (db-insert h t (list "ada" 36 "math"))
  (db-insert h t (list "bob" 41 "navy"))
  (db-insert h t (list "grace" 45 "navy"))
  (db-close h))
EOF
}

want() {
  local label="$1" expected="$2" src="$3"
  local dir="$D/$(echo "$label" | tr -c 'a-zA-Z0-9' '_')"
  mkdir -p "$dir"
  printf '%s\n' "$src" > "$dir/t.ainl"
  local got
  got=$(cd "$dir" && "$OLDPWD/$B" run t.ainl 2>&1)
  if [ "$got" == "$expected" ]; then
    echo "ok   $label"
  else
    echo "FAIL $label"
    echo "     want: $expected"
    echo "     got:  $got"
    fail=1
  fi
}

# A query, expected to succeed. The database is rebuilt per case so no case can
# see another's rows.
q() {
  local label="$1" query="$2" expected="$3"
  want "$label" "$expected" "(do
  (def h (db-open \"t.ainl-db\"))
  (def t (db-create-table h \"people\"))
  (db-insert h t (list \"ada\" 36 \"math\"))
  (db-insert h t (list \"bob\" 41 \"navy\"))
  (db-insert h t (list \"grace\" 45 \"navy\"))
  (print (db-query h \"$query\"))
  (db-close h))"
}

# A query, expected to be refused with exactly `$3`.
bad() {
  local label="$1" query="$2" expected="$3"
  local dir="$D/$(echo "$label" | tr -c 'a-zA-Z0-9' '_')"
  mkdir -p "$dir"
  printf '(do\n  (def h (db-open "t.ainl-db"))\n  (def t (db-create-table h "people"))\n  (db-insert h t (list "ada" 36 "math"))\n  (print (db-query h "%s"))\n  (db-close h))\n' \
    "$query" > "$dir/t.ainl"
  local got
  # Strip the interpreter's call-site position, which is a property of where the
  # call sits in *this* generated file and not of the query layer at all. The
  # pattern is anchored at the end and requires the full `(byte N)` shape, so it
  # cannot match the ` at line 1, col 21:` the *message* begins with — that is
  # the position inside the query, and it is part of the documented contract.
  got=$(cd "$dir" && "$OLDPWD/$B" run t.ainl 2>&1 |
    sed -e 's/^runtime error: //' \
        -e 's/ at line [0-9][0-9]*, col [0-9][0-9]* (byte [0-9][0-9]*)$//')
  if [ "$got" == "$expected" ]; then
    echo "ok   $label"
  else
    echo "FAIL $label"
    echo "     want: $expected"
    echo "     got:  $got"
    fail=1
  fi
}

# ---- the worked example in the section -----------------------------------
# The three snippets the docs show, asserted together so the section's opening
# block is the engine's output rather than a plausible-looking transcript.
#
# `print` of a list of rows writes one paren layer, so a two-row result is
# `(("bob") ("grace"))` — the inner parens belong to the rows.
want "the section's worked example" \
'(("bob") ("grace"))
(("grace"))
2' \
'(do
  (def h (db-open "t.ainl-db"))
  (def t (db-create-table h "people"))
  (db-insert h t (list "ada" 36 "math"))
  (db-insert h t (list "bob" 41 "navy"))
  (db-insert h t (list "grace" 45 "navy"))
  (print (db-query h "SELECT 1 FROM people WHERE 2 > 40"))
  (print (db-query h "SELECT 1 FROM people ORDER BY 2 DESC LIMIT 1"))
  (print (db-query-count h "SELECT 1 FROM people WHERE 3 = '"'"'navy'"'"'"))
  (db-close h))'

# ---- the projection and the shapes ---------------------------------------
q "SELECT * returns every row, in primary-key order" \
  "SELECT * FROM people" \
  '(("ada" 36 "math") ("bob" 41 "navy") ("grace" 45 "navy"))'

q "a projection takes columns by position" \
  "SELECT 1, 2 FROM people" \
  '(("ada" 36) ("bob" 41) ("grace" 45))'

q "the projection is applied after the sort" \
  "SELECT 1 FROM people ORDER BY 2" \
  '(("ada") ("bob") ("grace"))'

# ---- filtering ------------------------------------------------------------
q "WHERE selects the matching rows" \
  "SELECT 1 FROM people WHERE 3 = 'navy'" \
  '(("bob") ("grace"))'

q "AND requires both sides" \
  "SELECT 1 FROM people WHERE 2 > 40 AND 3 = 'navy'" \
  '(("bob") ("grace"))'

# `a OR b AND c` is `a OR (b AND c)`: ada matches the first disjunct, and
# 45='math' is false so the AND branch contributes nothing.
q "OR takes either, and AND binds tighter" \
  "SELECT 1 FROM people WHERE 1 = 'ada' OR 2 = 45 AND 3 = 'math'" \
  '(("ada"))'

q "an inequality selects the rest, sorted" \
  "SELECT 1 FROM people WHERE 2 != 41 ORDER BY 2" \
  '(("ada") ("grace"))'

# ---- ordering -------------------------------------------------------------
# The one the docs spell out: "navy" sorts after "math", and bob and grace both
# hold it, so DESC puts them first *in primary-key order* and ada last. A sort
# that reversed ties would answer (("grace") ("bob") ("ada")).
q "DESC reverses the comparison, not the ties" \
  "SELECT 1 FROM people ORDER BY 3 DESC" \
  '(("bob") ("grace") ("ada"))'

q "ORDER BY may name a column the projection drops" \
  "SELECT 1 FROM people ORDER BY 2 DESC" \
  '(("grace") ("bob") ("ada"))'

q "LIMIT applies after the sort" \
  "SELECT 1 FROM people ORDER BY 2 LIMIT 2" \
  '(("ada") ("bob"))'

# ---- counting -------------------------------------------------------------
want "db-query-count answers with the number" "2" \
'(do
  (def h (db-open "t.ainl-db"))
  (def t (db-create-table h "people"))
  (db-insert h t (list "ada" 36 "math"))
  (db-insert h t (list "bob" 41 "navy"))
  (db-insert h t (list "grace" 45 "navy"))
  (print (db-query-count h "SELECT 1 FROM people WHERE 2 > 40"))
  (db-close h))'

want "the count is taken after LIMIT" "2" \
'(do
  (def h (db-open "t.ainl-db"))
  (def t (db-create-table h "people"))
  (db-insert h t (list "ada" 36 "math"))
  (db-insert h t (list "bob" 41 "navy"))
  (db-insert h t (list "grace" 45 "navy"))
  (print (db-query-count h "SELECT 1 FROM people LIMIT 2"))
  (db-close h))'

# ---- nil: a projection, but not an ORDER BY ------------------------------
q "a projection past the end of a row is nil" \
  "SELECT 9 FROM people" \
  '((nil) (nil) (nil))'

# The three errors below are raised while *evaluating*, after the query parsed
# cleanly, so they do not carry the trailing ` in the query "..."` echo that a
# parse error does. Asserted as they are, because "why does this one error have
# the query attached and that one does not" is a thing a caller can observe.
bad "ORDER BY on a column no row has is refused" \
  "SELECT 1 FROM people ORDER BY 9" \
  "db-query: at line 1, col 31: ORDER BY column 9 is nil in every row of 'people' — a row in this database is a list, and no row here is that long"

# The one-row case is separate because a merge of a single element never calls a
# comparator, so the shape check cannot live inside the comparator.
bad "ORDER BY on a missing column is refused with one row too" \
  "SELECT 1 FROM people WHERE 1 = 'ada' ORDER BY 9" \
  "db-query: at line 1, col 47: ORDER BY column 9 is nil in every row of 'people' — a row in this database is a list, and no row here is that long"

bad "an ordered comparison against nil names both types" \
  "SELECT 1 FROM people WHERE 9 > 1" \
  "db-query: at line 1, col 28: WHERE column 9 is a nil and the value compared with it is a int — a column that is compared with <, <=, > or >= has to hold one type in every row (db-query: cannot order a nil and a int)"

q "'=' against nil is a plain answer, not an error" \
  "SELECT 1 FROM people WHERE 9 = nil" \
  '(("ada") ("bob") ("grace"))'

# ---- the clause order, enforced ------------------------------------------
bad "LIMIT before ORDER BY is refused, not reordered" \
  "SELECT * FROM people LIMIT 1 ORDER BY 2" \
  "db-query: at line 1, col 30: ORDER BY comes before LIMIT in a query, so 'ORDER' was written too late in the query \"SELECT * FROM people LIMIT 1 ORDER BY 2\""

# ---- unsupported SQL, refused by name -------------------------------------
SUBSET="SELECT <* | col, ...> FROM <table> [WHERE <col> <op> <value> [AND|OR <cond>]] [ORDER BY <col> [ASC|DESC]] [LIMIT <n>]"

bad "GROUP BY is named, with the supported subset" \
  "SELECT * FROM people GROUP BY 1" \
  "db-query: at line 1, col 22: 'GROUP' is not supported in v1; the supported subset is: $SUBSET in the query \"SELECT * FROM people GROUP BY 1\""

bad "a join is named" \
  "SELECT * FROM people JOIN people p ON 1 = 1" \
  "db-query: at line 1, col 22: 'JOIN' is not supported in v1; the supported subset is: $SUBSET in the query \"SELECT * FROM people JOIN people p ON 1 = 1\""

bad "DISTINCT is named" \
  "SELECT DISTINCT 1 FROM people" \
  "db-query: at line 1, col 8: 'DISTINCT' is not supported in v1; the supported subset is: $SUBSET in the query \"SELECT DISTINCT 1 FROM people\""

# The column is the `(` itself — the shape that cannot be anything else.
bad "a subquery in FROM is named as one" \
  "SELECT * FROM (SELECT 1 FROM people)" \
  "db-query: at line 1, col 15: a subquery or a parenthesised table (a join) in FROM is not supported in v1; the supported subset is: $SUBSET in the query \"SELECT * FROM (SELECT 1 FROM people)\""

bad "a subquery as a value is named as one" \
  "SELECT 1 FROM people WHERE 1 = (SELECT 1)" \
  "db-query: at line 1, col 32: a subquery as a value is not supported in v1; the supported subset is: $SUBSET in the query \"SELECT 1 FROM people WHERE 1 = (SELECT 1)\""

# ---- aggregates: refused, with the builtin that does the job -------------
bad "COUNT is refused and db-query-count is named" \
  "SELECT COUNT(1) FROM people" \
  "db-query: at line 1, col 8: COUNT is not supported in v1; the supported subset is: $SUBSET — use (db-query-count handle \"SELECT …\") to count matching rows in the query \"SELECT COUNT(1) FROM people\""

bad "SUM has no equivalent, and says so" \
  "SELECT SUM(1) FROM people" \
  "db-query: at line 1, col 8: SUM is not supported in v1; the supported subset is: $SUBSET — SUM is an aggregate, and v1 has no aggregate expressions in the query \"SELECT SUM(1) FROM people\""

# ---- read-only ------------------------------------------------------------
# The column is 1, not 8: a query that opens with the keyword fails at the
# keyword, before SELECT is even reached. That is the position a writer of the
# query needs — it is the first character of the statement.
for w in "INSERT INTO people VALUES (1)" "DELETE FROM people" "UPDATE people SET 2 = 3" "DROP TABLE people" "CREATE TABLE x (1)"; do
  bad "the query layer refuses '$w'" "$w" \
    "db-query: at line 1, col 1: '$(echo "$w" | cut -d' ' -f1 | tr 'a-z' 'A-Z')' is not supported in v1; the supported subset is: $SUBSET in the query \"$w\""
done

# ---- suggestions ----------------------------------------------------------
# A transposed pair. Plain edit distance scores FORM->FROM as two substitutions
# and the suggestion cap is one, so this is the case a shared close_match cannot
# see; it is repaired in the query layer on both engines.
bad "a transposed keyword gets a suggestion" \
  "SELECT 1 FORM people" \
  "db-query: at line 1, col 10: unexpected 'FORM' in the query \"SELECT 1 FORM people\" — did you mean 'FROM'?"

# ---- the column claims ---------------------------------------------------
bad "a column name explains that rows have no names" \
  "SELECT 1 FROM people WHERE name = 1" \
  "db-query: at line 1, col 28: 'name' is not a column: a row in this database is a list, so columns are numbered from 1 and 1 is the primary key in the query \"SELECT 1 FROM people WHERE name = 1\""

bad "column 0 does not exist" \
  "SELECT 0 FROM people" \
  "db-query: at line 1, col 8: column 0 does not exist; columns are numbered from 1 in the query \"SELECT 0 FROM people\""

# ---- backend scope: AOT accepts, the transpilers do not ------------------
# The asymmetry is the point of the section, so it gets its own assertion: a
# "a query is too hard for a compiled binary" change would break the docs and
# the section's argument while every unit test kept passing.
{
  dir="$D/aot"
  mkdir -p "$dir"
  printf '(do\n  (def h (db-open "t.ainl-db"))\n  (def t (db-create-table h "people"))\n  (db-insert h t (list "ada" 36 "math"))\n  (print (db-query h "SELECT 1 FROM people"))\n  (db-close h))\n' > "$dir/t.ainl"
  if (cd "$dir" && "$OLDPWD/$B" compile t.ainl -o t >/dev/null 2>&1); then
    echo "ok   ainl compile accepts a query program"
  else
    echo "FAIL ainl compile accepts a query program"
    fail=1
  fi
}

for tgt in python js ruby; do
  dir="$D/tr-$tgt"
  mkdir -p "$dir"
  printf '(db-query 1 "SELECT 1")\n' > "$dir/t.ainl"
  got=$(cd "$dir" && "$OLDPWD/$B" transpile t.ainl --to "$tgt" 2>&1)
  case "$got" in
    *'db-query'*interpreter-only*)
      echo "ok   transpile --to $tgt refuses db-query" ;;
    *)
      echo "FAIL transpile --to $tgt refuses db-query"
      echo "     got: $got"
      fail=1 ;;
  esac
  # The refusal must name the symbol and the offset, or it is not actionable.
  case "$got" in
    *byte*) echo "ok   the $tgt query refusal names the symbol and the offset" ;;
    *)
      echo "FAIL the $tgt query refusal names the symbol and the offset"
      fail=1 ;;
  esac
done

echo
[ "$fail" -eq 0 ] && echo "SYNTAX 3n: every doc claim verified" || echo "SYNTAX 3n: DOC CLAIMS FAILED"
exit "$fail"
