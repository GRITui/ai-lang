//! Close-match ("did you mean …?") suggestions for unbound symbols.
//!
//! This is the highest-value part of the repair loop. A program that calls
//! `prnt` instead of `print` fails at runtime; the difference between a
//! cryptic `unbound symbol 'prnt'` and
//! `unbound symbol 'prnt' at line 3, col 8 — did you mean 'print'?` is the
//! difference between the model re-reading the whole file and one edit.
//!
//! # The rule
//!
//! A candidate is suggested when it is within a bounded Levenshtein distance
//! of the misspelled name, **case-insensitively**, and the bound scales with
//! the length of the name so a long name tolerates more slips than a short
//! one. Two guards keep the advice from becoming noise:
//!
//! * The misspelled name must be at least [`MIN_LEN`] characters. Every
//!   one-character name is edit-distance 1 from every other, so without this
//!   a bare `f` would be "corrected" to `*`.
//! * Candidates that are pure punctuation (`+`, `*`, `<=`, …) are never
//!   suggested. A reader who typed a word did not mean an operator.
//!
//! # Determinism (the 4-backend rule)
//!
//! The four backends must produce byte-identical stderr, so the suggestion
//! cannot depend on `HashMap` iteration order, a locale, or anything else
//! non-deterministic. Ties are broken **alphabetically**, which is a total
//! order available to every backend. This is the same tie-break the C
//! runtime and the transpilers must use — see docs/SYNTAX.md.
//!
//! The candidate set is the names actually in scope rather than a hardcoded
//! list of builtins, so a suggestion respects the program's own vocabulary
//! (a typo'd local is corrected against the local) and a builtin added later
//! needs no change here.

use std::collections::BTreeSet;

/// Shortest misspelled name that will get a suggestion at all.
pub const MIN_LEN: usize = 3;

/// Candidate distances are capped at `max(1, len(name) * DIST_RATIO)`.
///
/// 1/3 is forgiving enough for the common slips (a dropped, doubled or
/// transposed character in a 3–12 char name) while staying tight enough that
/// a genuinely different word is left alone — the difference between advice
/// and noise.
const DIST_RATIO: f32 = 0.34;

/// Levenshtein edit distance, case-insensitive, over `char`s.
///
/// Two rows of a standard DP table: the algorithm is `O(len_a * len_b)` in
/// time and `O(min)` in space. Names are short (builtins are < 12 chars), so
/// the simplicity of the full table is worth more than the space saving, and
/// `lev` is on an error path only.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().flat_map(|c| c.to_lowercase()).collect();
    let b: Vec<char> = b.chars().flat_map(|c| c.to_lowercase()).collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// The number of leading characters two names have in common.
///
/// Used only to break a tie between candidates at the *same* edit distance,
/// where the number of edits cannot decide. It is a proxy for "which name did
/// the reader most likely mean": for `gret`, `greet` shares `gre` and `get`
/// shares only `ge`, so the longer common prefix is the better guess even
/// though both are one edit away.
///
/// Purely a tie-break — it never overrides a smaller edit distance — so it
/// cannot make a distant name look close.
fn common_prefix_len(a: &str, b: &str) -> usize {
    a.chars()
        .zip(b.chars())
        .take_while(|(x, y)| x.eq_ignore_ascii_case(y))
        .count()
}

/// The name in `candidates` closest to `name`, or `None` when nothing is close
/// enough to be worth suggesting.
///
/// Returns the *best* candidate, not a list: one concrete name to change is
/// easier to act on than a menu, and it keeps every backend's output to a
/// single line.
///
/// The ranking is, in order: fewest edits, then longest shared prefix, then
/// alphabetical. The last key exists only so the answer is a total order every
/// backend can reproduce (the 4-backend rule); the second exists because
/// alphabetical order alone picks the wrong name too often (see
/// [`common_prefix_len`]).
///
/// Takes and returns owned `String`s so a caller can pass a freshly built
/// candidate set (the scope chain) without borrowing from it.
pub fn close_match<I, S>(name: &str, candidates: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    if name.chars().count() < MIN_LEN {
        return None;
    }
    let max_dist = 1.max((name.chars().count() as f32 * DIST_RATIO).floor() as usize);
    // `BTreeSet` for a deterministic, de-duplicated candidate order, so the
    // ranking below never depends on the order the caller supplied.
    let uniq: BTreeSet<String> = candidates
        .into_iter()
        .map(|c| c.as_ref().to_string())
        .collect();
    // (edits, -shared_prefix, name) — lower is better, and the name makes it a
    // total order.
    let mut best: Option<(usize, std::cmp::Reverse<usize>, String)> = None;
    for cand in uniq {
        if !cand.chars().any(|c| c.is_alphanumeric()) {
            continue;
        }
        if cand == name {
            // Exact match: the name is bound, so this branch should not be
            // reachable, but suggesting a name that equals the one already
            // reported would be actively misleading.
            continue;
        }
        let d = edit_distance(name, &cand);
        if d > max_dist {
            continue;
        }
        // `Reverse` so that a *longer* shared prefix sorts lower (better).
        let key = (d, std::cmp::Reverse(common_prefix_len(name, &cand)), cand);
        best = match best {
            Some(ref cur) if *cur <= key => best,
            _ => Some(key),
        };
    }
    best.map(|(_, _, c)| c)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUILTINS: &[&str] = &[
        "print",
        "str",
        "list",
        "len",
        "first",
        "rest",
        "nth",
        "cons",
        "push",
        "hash",
        "get",
        "assoc",
        "has",
        "keys",
        "vals",
        "not",
        "error",
        "abs",
        "min",
        "max",
        "floor",
        "sqrt",
        "join",
        "split",
        "trim",
        "replace",
        "upcase",
        "downcase",
        "contains",
        "read-file",
        "write-file",
        "append-file",
        "env-get",
        "exit",
        "now",
        "sleep",
        "file-exists",
        "delete-file",
        "list-dir",
        "path-join",
        "path-base",
        "path-dir",
        "json-parse",
        "json-serialize",
        "+",
        "-",
        "*",
        "/",
        "<=",
        ">=",
        "mod",
    ];

    fn m(name: &str) -> Option<String> {
        close_match(name, BUILTINS.iter().copied())
    }

    #[test]
    fn a_typo_suggests_the_real_builtin() {
        assert_eq!(m("prnt").as_deref(), Some("print"));
        assert_eq!(m("lst").as_deref(), Some("list"));
        assert_eq!(m("sqr").as_deref(), Some("sqrt"));
    }

    #[test]
    fn a_dash_spelled_builtin_suggests_the_dashed_one() {
        assert_eq!(m("readfile").as_deref(), Some("read-file"));
        assert_eq!(m("jso-parse").as_deref(), Some("json-parse"));
    }

    #[test]
    fn an_unrelated_word_stays_silent() {
        assert_eq!(m("nonsensezzz"), None);
        assert_eq!(m("hello"), None);
    }

    #[test]
    fn a_one_character_name_never_guesses_an_operator() {
        // Every 1-char name is distance 1 from every other; without the guards
        // this reports `f` -> `*`, which is worse than saying nothing.
        assert_eq!(m("f"), None);
        assert_eq!(m("x"), None);
    }

    #[test]
    fn the_exact_name_is_never_suggested_back() {
        assert_eq!(m("print"), None);
    }

    #[test]
    fn case_differences_are_a_match() {
        assert_eq!(m("PRINT").as_deref(), Some("print"));
    }

    #[test]
    fn a_tie_prefers_the_longer_shared_prefix_over_alphabetical_order() {
        // `gret` is one edit from both `greet` and `get`. Alphabetically `get`
        // wins, which is the wrong answer: the reader who typed `gret` was
        // thinking `greet`. The shared-prefix tie-break fixes that.
        let cands = ["get", "greet"];
        assert_eq!(close_match("gret", cands).as_deref(), Some("greet"));
        // …and it is not order-dependent (4-backend rule).
        let reversed = ["greet", "get"];
        assert_eq!(close_match("gret", reversed).as_deref(), Some("greet"));
    }

    #[test]
    fn fewer_edits_always_beat_a_longer_prefix() {
        // The prefix is only a tie-break: a strictly nearer name must still
        // win even when it shares less of the prefix.
        let cands = ["print", "printer"];
        // `prntt` is 2 edits from `print` and 2 from `printer`, but 1 from
        // `print` once the trailing char is counted; assert the nearer wins.
        assert_eq!(close_match("prnt", cands).as_deref(), Some("print"));
    }

    #[test]
    fn ties_break_alphabetically_so_every_backend_agrees() {
        // Three candidates, all equidistant and sharing the same prefix, so
        // the final alphabetical key decides — reproducibly, on every backend.
        let cands = ["abx1", "abx2", "abx3"];
        assert_eq!(close_match("abx", cands).as_deref(), Some("abx1"));
        let reversed = ["abx3", "abx2", "abx1"];
        assert_eq!(close_match("abx", reversed).as_deref(), Some("abx1"));
    }
}
