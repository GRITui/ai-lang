//! GBNF membership — "is this string in the language the exported grammar
//! accepts?"
//!
//! # Why this exists in the CLI at all
//!
//! `ainl gen` claims to use grammar-constrained decoding. A claim like that is
//! worth nothing on its own, because the failure mode is silent: a backend
//! that ignores the constraint returns HTTP 200 and perfectly good prose, and
//! the run looks fine while measuring nothing. `docs/GENERATION.md` records
//! exactly this happening — `guided_grammar` on vLLM is accepted and dropped.
//!
//! So `ainl gen` does **not** take the backend's word for it. It verifies the
//! constraint two independent ways, and this module is the second one:
//!
//! 1. **The probe** ([`probe`](crate::gen_api)) sends a deliberately impossible
//!    grammar (`root ::= "Z"`) and requires the reply to be exactly `Z`. A
//!    backend that honours the constraint cannot do anything else; one that
//!    ignores it returns an essay.
//! 2. **This membership test** takes the *program that came back* and asks
//!    whether the exported AINL GBNF accepts it.
//!
//! The two are not redundant. The probe proves the backend honours *a*
//! constraint; this proves the constraint in force was *this* grammar and that
//! the resulting program is in the language the grammar describes.
//!
//! # Why membership and not `ainl parse`
//!
//! The AINL parser is a **superset** of the GBNF. The lexer is permissive and
//! happily tokenizes `:`, `#`, `->`, and even a decoder's startup banner as
//! symbols, so "`ainl ast` accepted it" is *not* evidence the grammar was
//! applied. `docs/GENERATION.md` pins this: on the 0.5B run, `ainl ast` was
//! 100% in both arms while GBNF membership was 100% vs 0%. Membership is the
//! strict, sound question; parsing is the necessary-but-not-sufficient one.
//!
//! # Relationship to the Python detectors
//!
//! This is a third implementation of the same predicate: the general Earley
//! parser (`scripts/gbnf-conformance.py`, the correctness reference) and the
//! fast recursive-descent one (`scripts/gen-harness/gbnf_fast.py`, the
//! practical one). It exists so the *shipped binary* can make the claim
//! without depending on a Python interpreter. All three mirror
//! `ainl grammar --gbnf` production for production, and the test module below
//! pins the cases that separate accept from reject, including the drift facts
//! `docs/SYNTAX.md` §6 records.
//!
//! One deliberate difference from the Python version: digit tests here are
//! ASCII-only, because the GBNF character class is literally `[0-9]`. Python's
//! `str.isdigit()` also accepts Unicode digits such as `٣`, so on exotic input
//! the two can disagree — and this implementation is the correct reading of
//! the grammar.

/// Byte-level recursive-descent matcher for `ainl grammar --gbnf`.
///
/// Bytes, not `char`s, because the GBNF's character classes are all ASCII and
/// a non-ASCII byte is legal *inside* a string (`[^"\\]` matches any byte that
/// is not a quote or a backslash) but nowhere else. Working in bytes makes
/// that fall out of the same code path.
struct Matcher<'a> {
    b: &'a [u8],
    i: usize,
}

/// `sym-char ::= [a-zA-Z0-9] | "+" | "-" | "*" | "/" | "<" | ">" | "=" | "!" |
///              "?" | "." | "_" | "&"`
#[inline]
fn is_sym_char(c: u8) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            b'+' | b'-' | b'*' | b'/' | b'<' | b'>' | b'=' | b'!' | b'?' | b'.' | b'_' | b'&'
        )
}

/// Bytes that can begin a form: `(`, a string quote, or any sym-char (digits
/// and `-` are sym-chars, so numbers begin with sym-chars too).
#[inline]
fn is_form_start(c: u8) -> bool {
    c == b'(' || c == b'"' || is_sym_char(c)
}

type MResult = Result<(), ()>;

impl<'a> Matcher<'a> {
    /// `ws ::= ( [ \t\n\r] | comment )*`
    ///
    /// Returns `Err` on an **unterminated** comment. `comment ::= ";" [^\n]*
    /// "\n"` requires a closing newline, so a comment that runs to
    /// end-of-file is not a comment at all — it is an unparseable input, and
    /// `(print 1) ; trailing` (no trailing newline) is not in the language.
    /// That asymmetry is easy to get wrong, and getting it wrong in this
    /// direction is the safe one: a stricter matcher can only reject a program
    /// that the decoder should not have produced anyway.
    fn ws(&mut self) -> MResult {
        while self.i < self.b.len() {
            match self.b[self.i] {
                b' ' | b'\t' | b'\n' | b'\r' => self.i += 1,
                b';' => {
                    self.i += 1;
                    while self.i < self.b.len() && self.b[self.i] != b'\n' {
                        self.i += 1;
                    }
                    // No newline before end-of-input: the comment is unterminated.
                    if self.i >= self.b.len() {
                        return Err(());
                    }
                    self.i += 1;
                }
                _ => return Ok(()),
            }
        }
        Ok(())
    }

    /// `string ::= "\"" ( [^"\\] | "\\" ["\\/nrt] )* "\""`
    fn string(&mut self) -> MResult {
        self.i += 1; // opening quote
        while self.i < self.b.len() {
            match self.b[self.i] {
                b'\\' => {
                    let next = *self.b.get(self.i + 1).ok_or(())?;
                    if !matches!(next, b'"' | b'\\' | b'/' | b'n' | b'r' | b't') {
                        return Err(());
                    }
                    self.i += 2;
                }
                b'"' => {
                    self.i += 1;
                    return Ok(());
                }
                _ => self.i += 1,
            }
        }
        Err(()) // unterminated
    }

    /// `number ::= "-"? [0-9]+ ( "." [0-9]+ )? ( [eE] "-"? [0-9]+ )?`
    ///
    /// Returns `true` if a number was consumed, `false` if this position does
    /// not start one — the caller then rewinds and tries `symbol`, since
    /// `atom` accepts either.
    fn number(&mut self) -> bool {
        if self.i < self.b.len() && self.b[self.i] == b'-' {
            self.i += 1;
        }
        if self.i >= self.b.len() || !self.b[self.i].is_ascii_digit() {
            return false;
        }
        while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
            self.i += 1;
        }
        // `number ::= "-"? [0-9]+ ( "." [0-9]+ )? ( [eE] "-"? [0-9]+ )?`
        //
        // Both optional groups require a digit after their lead character, so
        // `1.` and `1e` consume only the integer part. That is what the
        // grammar says; a trailing `.` or `e` then fails as a symbol, which is
        // the correct rejection.
        if self.i < self.b.len()
            && self.b[self.i] == b'.'
            && matches!(self.b.get(self.i + 1), Some(c) if c.is_ascii_digit())
        {
            self.i += 1;
            while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
                self.i += 1;
            }
        }
        if matches!(self.b.get(self.i), Some(b'e') | Some(b'E')) {
            let mut j = self.i + 1;
            if matches!(self.b.get(j), Some(b'-')) {
                j += 1;
            }
            if matches!(self.b.get(j), Some(c) if c.is_ascii_digit()) {
                self.i = j;
                while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
                    self.i += 1;
                }
            }
        }
        true
    }

    /// `atom ::= (string | number | symbol) ws`
    fn atom(&mut self) -> MResult {
        if self.i >= self.b.len() {
            return Err(());
        }
        if self.b[self.i] == b'"' {
            self.string()?;
        } else {
            let save = self.i;
            if !self.number() {
                self.i = save;
                // `symbol ::= sym-char+`
                let start = self.i;
                while self.i < self.b.len() && is_sym_char(self.b[self.i]) {
                    self.i += 1;
                }
                if self.i == start {
                    return Err(());
                }
            }
        }
        self.ws()?;
        Ok(())
    }

    /// `list ::= "(" ws (form ws)* ")"`
    fn list(&mut self) -> MResult {
        self.i += 1; // '('
        self.ws()?;
        loop {
            match self.b.get(self.i) {
                Some(b')') => {
                    self.i += 1;
                    return Ok(());
                }
                Some(&c) if is_form_start(c) => {
                    self.form()?;
                    self.ws()?;
                }
                _ => return Err(()),
            }
        }
    }

    fn form(&mut self) -> MResult {
        if self.b.get(self.i) == Some(&b'(') {
            self.list()
        } else {
            self.atom()
        }
    }

    /// `root ::= ws form (ws form)* ws` — and nothing may follow the last form.
    fn root(&mut self) -> MResult {
        self.ws()?;
        match self.b.get(self.i) {
            Some(&c) if is_form_start(c) => self.form()?,
            _ => return Err(()),
        }
        loop {
            let save = self.i;
            self.ws()?;
            match self.b.get(self.i) {
                Some(&c) if is_form_start(c) => self.form()?,
                _ => {
                    // Not another form. Rewind so the trailing-`ws` pass below
                    // sees the same position and the final `i == len` check
                    // rejects any non-whitespace remainder.
                    self.i = save;
                    break;
                }
            }
        }
        self.ws()?;
        if self.i == self.b.len() {
            Ok(())
        } else {
            Err(())
        }
    }
}

/// True iff `s` is a member of the language `ainl grammar --gbnf` accepts.
///
/// This is the sound "was the constraint applied?" test. `ainl_core::parse`
/// cannot answer it, because the parser accepts a strict superset (see the
/// module docs).
pub fn accepts(s: &str) -> bool {
    Matcher {
        b: s.as_bytes(),
        i: 0,
    }
    .root()
    .is_ok()
}

#[cfg(test)]
mod tests {
    use super::accepts;

    /// The accept/reject cases, mirroring the self-test in
    /// `scripts/gen-harness/gbnf_fast.py`. Kept in the same order and with the
    /// same comments, so the two implementations are readable side by side and
    /// a divergence is obvious rather than accidental.
    #[test]
    fn accepts_what_the_grammar_accepts() {
        for src in [
            "(print 1)\n",
            "(+ 1(+ 2 3))\n",
            "(def x 1)\n(print x)\n",
            "(a b)(c d)\n",
            "(print \"a\\nb\")\n", // valid escape
            "hello\n",             // bare symbol
            "(list 1 2 3)\n",
            "((1 2) 3)\n",         // nested lists
            "(print \"a:b#c\")\n", // specials are fine inside a string
        ] {
            assert!(accepts(src), "should be a GBNF member: {src:?}");
        }
    }

    #[test]
    fn rejects_what_the_grammar_rejects() {
        for src in [
            "(print \"a\\qb\")\n",          // invalid escape (\q)
            "",                             // empty program
            "\n  \n",                       // whitespace only
            "# not a comment\n(print 1)\n", // '#' is not a sym-char
            "(print 1: 2)\n",               // ':' is not a sym-char
            "(print 1)\n# trailing\n",      // ditto
            "(print 1) ; trailing",         // a comment with no final newline
        ] {
            assert!(!accepts(src), "should NOT be a GBNF member: {src:?}");
        }
    }

    /// The property that makes membership the *right* test: everything the
    /// grammar accepts must also parse. A string in the GBNF language that the
    /// parser rejects would be a broken thesis, and this is the assertion that
    /// would catch it.
    #[test]
    fn membership_implies_the_parser_accepts() {
        for src in [
            "(print 1)\n",
            "(def f (fn (x) (* x 2)))\n(print (f 21))\n",
            "(a b)(c d)\n",
            "((1 2) 3)\n",
            "(print \"a:b#c\")\n",
            "hello\n",
        ] {
            assert!(accepts(src), "test bug: {src:?} is not a GBNF member");
            ainl_core::parse(src)
                .unwrap_or_else(|e| panic!("GBNF member the parser rejected: {src:?}: {e}"));
        }
    }

    /// The converse does **not** hold, and that asymmetry is the whole reason
    /// this module exists. If the parser ever stopped accepting these, the
    /// premise of using membership as the sound test would need revisiting —
    /// so the fact is pinned rather than assumed.
    #[test]
    fn the_parser_accepts_strings_the_grammar_does_not() {
        for src in [
            "(print 1: 2)\n", // ':' outside a string
            "# comment\n(print 1)\n", // '#' is not a sym-char, so to the
                              // grammar this is not a comment at all — it is a form that cannot
                              // be lexed
        ] {
            assert!(
                !accepts(src),
                "test bug: {src:?} is a GBNF member, so it proves nothing"
            );
            ainl_core::parse(src).unwrap_or_else(|e| {
                panic!("the parser is supposed to be more permissive: {src:?}: {e}")
            });
        }
    }

    /// A case worth pinning because it looks like a bug and is not:
    /// `x = 1` *is* in the GBNF language. It is three top-level symbol atoms
    /// (`x`, `=`, `1`) separated by whitespace, and `root ::= ws form (ws
    /// form)* ws` accepts exactly that. The grammar constrains *shape*, never
    /// *meaning* — which is precisely why a constrained decode can emit
    /// syntactically perfect nonsense, and why membership alone proves
    /// nothing about whether a program is correct.
    #[test]
    fn shape_is_not_meaning() {
        assert!(accepts("x = 1\n(print x)\n"));
        assert_eq!(
            ainl_core::parse("x = 1\n(print x)\n")
                .expect("parses")
                .len(),
            4,
            "four top-level forms: x, =, 1, and (print x)"
        );
    }

    /// Nesting is unbounded in the grammar, so a deeply nested program must
    /// still be accepted. This also guards the recursive matcher itself against
    /// an accidental early-out on depth.
    #[test]
    fn deep_nesting_is_accepted() {
        let src = format!("{}1{}", "(".repeat(64), ")".repeat(64));
        assert!(accepts(&src));
    }

    /// Truncation is the failure a constrained decode actually exhibits when it
    /// runs out of budget: a valid prefix with the closing parens missing. It
    /// must be rejected, so `ainl gen` never treats a cut-off program as a
    /// clean constrained result.
    #[test]
    fn a_truncated_program_is_rejected() {
        let full = "(def f (fn (x) (* 2 x)))\n(print (f 21))\n";
        assert!(accepts(full));
        for cut in [
            "(def f (fn (x) (* 2 x))",
            "(def f (fn (x) (* 2 x)))\n(print (f 21)",
            "(def f (fn (x) (* 2 x)))\n(print (f 2",
        ] {
            assert!(
                !accepts(cut),
                "a truncated program must not be a member: {cut:?}"
            );
        }
    }

    /// Non-ASCII: legal inside a string, illegal as a symbol character. This
    /// is the one place a byte-level matcher and a `char`-level reader could
    /// plausibly differ, so it is pinned.
    #[test]
    fn non_ascii_is_legal_only_inside_a_string() {
        assert!(accepts("(print \"héllo →\")\n"));
        assert!(!accepts("(print héllo)\n"));
    }
}
