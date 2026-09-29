#!/usr/bin/env bash
# Every example in examples/corpus/ must actually RUN.
#
# A dead example is worse than no example: the whole point of this corpus is to
# be shown to a model that has never seen AINL, and a model shown a broken
# program learns the broken shape. So every example is executed here and its
# exit code is asserted to be 0 — and, when the example declares a portable
# `@scope`, its output is compared byte-for-byte against all three transpiler
# targets, because "it ran on the interpreter" is exactly the claim that lets
# a 4-backend divergence ship unnoticed.
#
# The header of each example carries its own metadata:
#
#   ; @example   <name>      unique, matches the file stem
#   ; @summary   <one line>  what it does, for the README index
#   ; @teaches   <a, b, c>   the patterns it exists to demonstrate
#   ; @scope     <scope>     portable | interpreter-only | server
#   ; @expect    <one line>  the key line the output must contain
#
# The `@expect` line is what keeps an example honest: an example that stops
# printing its headline number would still exit 0, and a corpus that silently
# became a no-op is a corpus that stops teaching. It is matched as a substring
# so a real example can print more than the one line.
#
# No network, no API key. Exits non-zero if any example fails.
set -uo pipefail
cd "$(dirname "$0")/.."

BIN=target/release/ainl
if [ ! -x "$BIN" ]; then
  echo "building ainl (release)..."
  cargo build --release --quiet || exit 1
  BIN=target/release/ainl
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

fail=0
count=0
portable=0

# Read one @key from an example's header. Blank value if absent.
field() {
  sed -n "s/^; @$2[ ]\{1,\}\(.*\)\$/\1/p" "$1" | head -1
}

# EVERY example in the corpus, at any depth.
#
# The search recurses rather than globbing `examples/corpus/*.ainl`, because
# two of the corpus's examples are multi-file by nature — a `main.ainl` that
# imports modules, and a `run-tests.ainl` that points at a suite — and a
# top-level-only glob silently skips both. That is the failure this script
# exists to prevent, applied to the script itself: a check that passes while
# never looking at two of the examples reports a corpus size that is not the
# corpus size, and the number is the one a reader trusts.
#
# A file WITHOUT a header of its own — the modules a main imports, the fixture
# a suite loads — is skipped, because running one on its own means nothing.
# A file WITH a header is an example, and is checked. `find | sort` rather
# than a glob so the order is the same on every machine and every shell.
find_examples() {
  find examples/corpus -name '*.ainl' -type f | sort | while read -r f; do
    if grep -q '^; @example' "$f"; then
      echo "$f"
    fi
  done
}

for ex in $(find_examples); do
  # The example's identity is its path RELATIVE to examples/corpus/, not its
  # bare filename. `libmod/main.ainl` and `testing/run-tests.ainl` are both
  # multi-file examples whose leaf names are generic — two files called
  # `main.ainl` in one corpus would collide on a bare-stem key, and the
  # @example/@expect pair the gate reads would be ambiguous. The relative path
  # is unique, and it is also what a reader types.
  rel=${ex#examples/corpus/}
  name=$rel
  count=$((count + 1))
  scope=$(field "$ex" scope)
  expect=$(field "$ex" expect)
  summary=$(field "$ex" summary)
  # `@example` is the leaf stem, so it is compared against the basename.
  declared=$(field "$ex" example)
  stem=$(basename "$ex" .ainl)

  if [ -z "$summary" ]; then
    echo "FAIL $name — no @summary in the header"
    fail=1
    continue
  fi
  if [ -z "$scope" ]; then
    echo "FAIL $name — no @scope in the header"
    fail=1
    continue
  fi
  if [ "$declared" != "$stem" ]; then
    echo "FAIL $name — @example says '$declared'; it must match the file stem '$stem'"
    fail=1
    continue
  fi

  # ---- a `server` example needs its fixture up BEFORE it is run ----------
  # Not a weaker check than the portable ones: the fixture is started on a
  # port the OS picks, the example is run against it, and the refusal claims
  # its own comments make (https refused by name; AOT and all three
  # transpilers refusing http) are asserted here. A `server` example that no
  # CI job runs is exactly the dead example this script exists to prevent.
  #
  # This block sits BEFORE the interpreter run rather than after it, because a
  # connection refused is indistinguishable from a broken program: both are
  # "exit 1 with a message", and getting that order wrong makes the gate
  # report a code bug when the only fault is that nothing was listening yet.
  if [ "$scope" = "server" ]; then
    if ! command -v python3 > /dev/null 2>&1; then
      echo "FAIL $name — needs python3 to start its fixture"
      fail=1
      continue
    fi
    FIXTURE=examples/corpus/http-fixture/server.py
    if [ ! -f "$FIXTURE" ]; then
      echo "FAIL $name — its fixture $FIXTURE is missing"
      fail=1
      continue
    fi
    # A port the OS picks, so the check never collides with a real service on
    # the runner. A fixed port in a CI fixture is a flake waiting to happen.
    PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
    python3 "$FIXTURE" "$PORT" > /dev/null 2> "$WORK/srv.log" &
    SRV=$!
    # Wait for the listener to ACCEPT, rather than sleeping a guessed
    # interval: a fixed sleep is a flaky gate, and this one runs in CI.
    for _ in $(seq 1 100); do
      if python3 -c "
import socket,sys
s=socket.socket()
s.settimeout(0.2)
sys.exit(0 if s.connect_ex(('127.0.0.1',$PORT))==0 else 1)
" 2> /dev/null; then break; fi
    done
  fi

  # ---- the interpreter must run it, exit 0 ------------------------------
  if [ "$scope" = "server" ]; then
    run_status=0
    AINL_PORT="$PORT" "$BIN" run "$ex" > "$WORK/out" 2> "$WORK/err" || run_status=$?
  else
    run_status=0
    "$BIN" run "$ex" > "$WORK/out" 2> "$WORK/err" || run_status=$?
  fi
  if [ "$run_status" -ne 0 ]; then
    echo "FAIL $name — interpreter exited non-zero:"
    sed 's/^/    /' "$WORK/err" | head -5
    fail=1
    if [ "$scope" = "server" ]; then
      kill "$SRV" 2> /dev/null
      wait "$SRV" 2> /dev/null
    fi
    continue
  fi

  if [ -n "$expect" ] && ! grep -qF -- "$expect" "$WORK/out"; then
    echo "FAIL $name — output does not contain the declared @expect line:"
    echo "    want: $expect"
    echo "    got:"
    sed 's/^/      /' "$WORK/out"
    fail=1
    if [ "$scope" = "server" ]; then
      kill "$SRV" 2> /dev/null
      wait "$SRV" 2> /dev/null
    fi
    continue
  fi

  if [ "$scope" = "server" ]; then
    echo "ok   $name (server on 127.0.0.1:$PORT)"

    # The example's comments claim https is refused BY NAME, before any
    # connection. Assert it: a doc claim written from memory is the one that
    # rots, and this is the claim a reader is most likely to rely on.
    #
    # stderr, not stdout, and captured to a file rather than piped: these
    # refusals exit non-zero, and a `cmd | grep -q` pipeline reports the
    # status of the LAST element, so a successful match on a failing command
    # reads as "refused" even when the message was something else entirely.
    # Checking the file's contents against the command's own status is the
    # only way to assert both halves.
    printf '(http-get "https://example.invalid/")\n' > "$WORK/https.ainl"
    https_status=0
    "$BIN" run "$WORK/https.ainl" > "$WORK/https.out" 2> "$WORK/https.err" || https_status=$?
    if [ "$https_status" -ne 0 ] && grep -q "not supported" "$WORK/https.err"; then
      echo "ok   $name — https is refused by name, and the message names the fix"
    else
      echo "FAIL $name — https was not refused as the example claims (exit $https_status):"
      sed 's/^/    /' "$WORK/https.err" | head -3
      fail=1
    fi

    # ... and that AOT and all three transpilers refuse http outright.
    printf '(print (get (http-get "http://127.0.0.1:1/x") "status"))\n' > "$WORK/h.ainl"
    if "$BIN" compile "$WORK/h.ainl" -o "$WORK/h.aot" 2> "$WORK/aot.err" &&
       [ ! -s "$WORK/aot.err" ]; then
      echo "FAIL $name — ainl compile accepted a program with http-get"
      fail=1
    elif ! grep -q "interpreter-only" "$WORK/aot.err"; then
      echo "FAIL $name — AOT refused, but not for the documented reason:"
      sed 's/^/    /' "$WORK/aot.err" | head -3
      fail=1
    else
      echo "ok   $name — AOT refuses http"
    fi
    for target in python js ruby; do
      if "$BIN" transpile "$WORK/h.ainl" --to "$target" > /dev/null 2> "$WORK/t.err"; then
        echo "FAIL $name — transpile --to $target accepted a program with http-get"
        fail=1
      elif ! grep -q "interpreter-only" "$WORK/t.err"; then
        echo "FAIL $name — $target refused, but not for the documented reason:"
        sed 's/^/    /' "$WORK/t.err" | head -3
        fail=1
      else
        echo "ok   $name — $target refuses http"
      fi
    done
    kill "$SRV" 2> /dev/null
    wait "$SRV" 2> /dev/null
    continue
  fi

  if [ "$scope" = "interpreter-only" ]; then
    echo "ok   $name (interpreter-only)"
    # An interpreter-only example is only honest if NO backend that can build it
    # then produces the wrong answer.
    #
    # Each backend has exactly one acceptable outcome, and the assertion differs
    # by backend because the *reason* it was interpreter-only differs too:
    #
    # - AOT either REFUSES (the honest answer when the program needs something
    #   the C runtime has no way to provide), or BUILDS IT AND GETS IT RIGHT.
    #   Building is not a failure: `import` used to make AOT refuse, and AOT now
    #   resolves and inlines the graph, so a program that only needed modules is
    #   legitimately no longer interpreter-only. What must never happen is a
    #   binary that builds and then misbehaves — so a successful compile is
    #   checked by RUNNING it against the interpreter's own output, which is
    #   the only evidence that "it compiled" means "it works".
    # - The transpilers must REFUSE. `import` is a keyword in Python, Ruby and
    #   JavaScript, so an unhandled directive would lower to a call into the
    #   host's own import machinery — a program that builds cleanly and does the
    #   wrong thing. They have no way to inline a graph into a single output
    #   file, so they are the backends that still say no.
    if "$BIN" compile "$ex" -o "$WORK/io.aot" 2> "$WORK/io.err"; then
      # It compiled. Now it has to be RIGHT, which is the real claim.
      if ! "$WORK/io.aot" > "$WORK/io.out" 2>&1; then
        echo "FAIL $name — ainl compile accepted it, but the binary failed at run time:"
        sed 's/^/    /' "$WORK/io.out" | head -3
        fail=1
      elif ! diff -q "$WORK/io.out" "$WORK/out" > /dev/null 2>&1; then
        echo "FAIL $name — the AOT binary's output differs from the interpreter's:"
        diff "$WORK/out" "$WORK/io.out" | head -6 | sed 's/^/    /'
        fail=1
      else
        echo "ok   $name — AOT builds it and its output matches the interpreter"
      fi
    elif ! grep -q "interpreter-only" "$WORK/io.err"; then
      echo "FAIL $name — AOT refused, but not for the documented reason:"
      sed 's/^/    /' "$WORK/io.err" | head -3
      fail=1
    else
      echo "ok   $name — AOT refuses it, as its @scope claims"
    fi
    for target in python js ruby; do
      if "$BIN" transpile "$ex" --to "$target" > /dev/null 2> "$WORK/io.err"; then
        echo "FAIL $name — transpile --to $target accepted an interpreter-only program"
        fail=1
      elif ! grep -q "interpreter-only" "$WORK/io.err"; then
        echo "FAIL $name — $target refused, but not for the documented reason:"
        sed 's/^/    /' "$WORK/io.err" | head -3
        fail=1
      fi
    done
    echo "ok   $name — python, js and ruby refuse it"
    continue
  fi

  # ---- a portable example must agree across all backends ---------------
  # Not "should" agree: `scope` is the example's own claim about where it runs,
  # and the README and SYNTAX.md both repeat that claim to a reader. An example
  # whose own header is wrong teaches the wrong thing on every backend that
  # does not get checked here.
  if [ "$scope" = "portable" ]; then
    portable=$((portable + 1))
    for target in python js ruby; do
      case "$target" in
        python) runner=python3 ;;
        js)     runner=node ;;
        ruby)   runner=ruby ;;
      esac
      if ! command -v "$runner" > /dev/null 2>&1; then
        echo "skip $name/$target ($runner not installed)"
        continue
      fi
      if ! "$BIN" transpile "$ex" --to "$target" > "$WORK/t.$target" 2> "$WORK/terr"; then
        echo "FAIL $name/$target — transpile refused:"
        sed 's/^/    /' "$WORK/terr" | head -3
        fail=1
        continue
      fi
      if ! "$runner" "$WORK/t.$target" > "$WORK/r.$target" 2>&1; then
        echo "FAIL $name/$target — the transpiled program errored:"
        sed 's/^/    /' "$WORK/r.$target" | head -5
        fail=1
        continue
      fi
      if ! cmp -s "$WORK/out" "$WORK/r.$target"; then
        # ONE documented divergence is allowed, and only in JS. A JS `Number`
        # is a single type, so AINL cannot distinguish an int from a float
        # there, and a whole value loses its mandatory ".0":
        #
        #     (print 3.0)                  3.0   ->  3
        #     (print (/ 4 2))              2.0   ->  2
        #     (json-serialize (hash "a" 1)) {"a":1} -> {"a":1.0}
        #
        # docs/SYNTAX.md 3 records this for `json-serialize` inside a
        # container. MEASURED while building this corpus, it is in fact
        # broader: a whole float printed at TOP LEVEL collapses too, which the
        # section does not say. That gap is reported rather than papered over
        # here — see the note left in examples/README.md.
        #
        # The normalization is applied to the expected side, in both
        # directions, and ONLY to numbers that sit in a value-printing
        # position: a bare token, or a JSON scalar. It is deliberately narrow.
        # A blanket "strip .0 from every number" would also erase a real
        # arithmetic divergence — a program computing 1.5 on one backend and
        # 1 on another would pass — and this script exists precisely to catch
        # that class of bug rather than absorb it.
        #
        #   bare token : 3.0  -> 3        (a whole float losing its .0)
        #   JSON value : :1  -> :1.0      (an int gaining one)
        #
        # Anything else that differs still fails the example.
        sed -E -e 's/(^|[( ])([0-9]+)\.0([,)]|$)/\1\2\3/g' \
               -e 's/:([0-9]+)([,}])/:\1.0\2/g' \
          "$WORK/out" > "$WORK/out.norm"
        if [ "$target" = "js" ] && cmp -s "$WORK/out.norm" "$WORK/r.$target"; then
          echo "ok   $name (js: the documented int/float collapse)"
          continue
        fi
        echo "FAIL $name/$target — output differs from the interpreter:"
        diff "$WORK/out" "$WORK/r.$target" | sed 's/^/    /' | head -10
        fail=1
        continue
      fi
      echo "ok   $name ($scope; $target byte-equal)"
    done
  else
    echo "ok   $name ($scope)"
  fi
done

echo
# ---- the README index must list every example, and match their scope -----
# The index is the only map from "I need file I/O" to "run this file", so a
# stale row sends a reader to a program that no longer exists or no longer
# teaches what the row claims. Both are the kind of rot that nothing else in
# this repo would catch: the examples are tested, but nothing was connecting
# the table to them.
#
# Checked by NAME, not by reading the prose: every `@example` must appear as a
# link target, and every link target must exist. The scope words in the table
# are prose and are deliberately not parsed — a table that says "portable" in
# the wrong place is a documentation nit, whereas a missing row is a dead end
# for a reader, and only the second is worth failing a build over.
if [ ! -f examples/README.md ]; then
  echo "FAIL examples/README.md is missing — the corpus has no index"
  fail=1
else
  # Match on the path RELATIVE to examples/, which is what a markdown link
  # from examples/README.md actually contains — `corpus/libmod/main.ainl`, not
  # `corpus/main.ainl`. Matching on the bare filename would pass for any
  # example with that leaf name, which is exactly the ambiguity the relative
  # path is used to avoid.
  for ex in $(find_examples); do
    rel=${ex#examples/}
    if ! grep -qF "($rel)" examples/README.md; then
      echo "FAIL examples/README.md does not link to $rel"
      fail=1
    fi
  done
  for link in $(grep -oE '\]\(corpus/[^)]+\)' examples/README.md | sed 's/^](//; s/)$//'); do
    if [ ! -e "examples/$link" ]; then
      echo "FAIL examples/README.md links to $link, which does not exist"
      fail=1
    fi
  done
  if [ "$fail" -eq 0 ]; then
    echo "ok   examples/README.md lists every example and every link resolves"
  fi
fi

echo
# ---- the generated few-shot corpus must be current ----------------------
# Checked LAST and as a separate script, because it is the artifact a model
# actually reads. An out-of-date corpus teaches syntax that no longer parses,
# which is the single failure this whole directory cannot have.
if ! ./scripts/build-few-shot.sh --check; then
  fail=1
fi

echo
if [ "$fail" -ne 0 ]; then
  echo "examples: FAILED"
  exit 1
fi
echo "examples: all $count run clean ($portable verified byte-equal on all three transpiler targets)"
exit 0
