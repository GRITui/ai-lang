#!/usr/bin/env python3
"""Sound, fast membership test for the AINL GBNF language.

Why not the general Earley parser (scripts/gbnf-conformance.py::gbnf_accepts)?
It is correct but O(n^3) in practice because the `ws` star rule spawns O(n)
origins per position; on a ~400-char model output it exceeds 300s in Python.

The AINL GBNF is a small, fixed context-free grammar (nested lists need a
stack, so a DFA is impossible), but it is trivially parseable by a
recursive-descent parser. This module implements exactly that: it accepts
*precisely* the strings the exported GBNF accepts, in O(n).

Soundness: the production structure mirrors `ainl grammar --gbnf` line for
line (see docs/SYNTAX.md §6). If the grammar changes, update SYMCHARS / the
rules here to match.

Public API:
    ainl_gbnf_accepts(s: str) -> bool
"""

# sym-char ::= [a-zA-Z0-9] | "+" | "-" | "*" | "/" | "<" | ">" | "=" | "!"
#             | "?" | "." | "_" | "&"
_SYMCHARS = (
    set("abcdefghijklmnopqrstuvwxyz")
    | set("ABCDEFGHIJKLMNOPQRSTUVWXYZ")
    | set("0123456789")
    | set("+-*/<>!=?._&")
)
# A form can start with "(" (list), '"' (string atom), or any sym-char
# (symbol / number; digits and '-' are sym-chars too).
_FORM_START = set('("') | _SYMCHARS
_WS = set(" \t\n\r")


class _ParseError(Exception):
    pass


class _P:
    __slots__ = ("s", "i", "n")

    def __init__(self, s):
        self.s = s
        self.i = 0
        self.n = len(s)

    # ws ::= ( [ \t\n\r] | comment )*
    def ws(self):
        s, n = self.s, self.n
        while self.i < n:
            c = s[self.i]
            if c in _WS:
                self.i += 1
            elif c == ";":
                self.comment()
            else:
                break

    # comment ::= ";" [^\n]* "\n"
    def comment(self):
        s, n = self.s, self.n
        self.i += 1  # consume ';'
        while self.i < n and s[self.i] != "\n":
            self.i += 1
        if self.i >= n:
            raise _ParseError("unterminated comment (no newline)")
        self.i += 1  # consume the newline

    # string ::= "\"" ( [^"\\] | "\\" ["\\/nrt] )* "\""
    def string(self):
        s, n = self.s, self.n
        self.i += 1  # consume opening '"'
        while self.i < n:
            c = s[self.i]
            if c == "\\":
                if self.i + 1 < n and s[self.i + 1] in '"\\/nrt':
                    self.i += 2
                else:
                    raise _ParseError("invalid escape")
            elif c == '"':
                self.i += 1
                return
            else:
                self.i += 1
        raise _ParseError("unterminated string")

    # number ::= "-"? [0-9]+ ( "." [0-9]+ )? ( [eE] "-"? [0-9]+ )?
    # Returns True and advances iff a number was consumed.
    def number(self):
        s, n = self.s, self.n
        if self.i < n and s[self.i] == "-":
            self.i += 1
        if self.i >= n or not s[self.i].isdigit():
            return False
        while self.i < n and s[self.i].isdigit():
            self.i += 1
        if self.i < n and s[self.i] == ".":
            j = self.i + 1
            if j < n and s[j].isdigit():
                self.i = j
                while self.i < n and s[self.i].isdigit():
                    self.i += 1
        if self.i < n and s[self.i] in "eE":
            j = self.i + 1
            if j < n and s[j] == "-":
                j += 1
            if j < n and s[j].isdigit():
                self.i = j
                while self.i < n and s[self.i].isdigit():
                    self.i += 1
        return True

    # symbol ::= sym-char+
    def symbol(self):
        s, n = self.s, self.n
        start = self.i
        while self.i < n and s[self.i] in _SYMCHARS:
            self.i += 1
        if self.i == start:
            raise _ParseError("empty symbol")

    # form ::= list | atom
    def form(self):
        if self.i < self.n and self.s[self.i] == "(":
            self.list_()
        else:
            self.atom()

    # list ::= "(" ws (form ws)* ")"
    def list_(self):
        s, n = self.s, self.n
        self.i += 1  # consume '('
        self.ws()
        while True:
            if self.i < n and s[self.i] == ")":
                break
            if self.i < n and s[self.i] in _FORM_START:
                self.form()
                self.ws()
            else:
                raise _ParseError("expected a form or ')' in list")
        if self.i >= n or s[self.i] != ")":
            raise _ParseError("unterminated list")
        self.i += 1  # consume ')'

    # atom ::= (string | number | symbol) ws
    def atom(self):
        s, n = self.s, self.n
        if self.i >= n:
            raise _ParseError("expected atom, got end of input")
        if s[self.i] == '"':
            self.string()
        else:
            save = self.i
            if not self.number():
                self.i = save
                self.symbol()
        self.ws()

    # root ::= ws form (ws form)* ws
    def parse(self):
        s, n = self.s, self.n
        self.ws()
        if self.i >= n or s[self.i] not in _FORM_START:
            raise _ParseError("expected a form at top level")
        self.form()
        while self.i < n:
            save = self.i
            self.ws()
            if self.i < n and s[self.i] in _FORM_START:
                self.form()
            else:
                self.i = save
                break
        self.ws()
        if self.i != n:
            raise _ParseError("trailing input after last form")


def ainl_gbnf_accepts(s: str) -> bool:
    """True iff `s` is a member of the AINL GBNF language."""
    try:
        _P(s).parse()
        return True
    except _ParseError:
        return False


if __name__ == "__main__":
    import sys
    # Self-test on known cases.
    cases = {
        "(print 1)\n": True,
        "(+ 1(+ 2 3))\n": True,
        "(def x 1)\n(print x)\n": True,
        "(a b)(c d)\n": True,
        '(print "a\\qb")\n': False,          # invalid escape
        '(print "a\\nb")\n': True,           # valid escape
        "": False,                            # empty program
        "\n  \n": False,                      # whitespace-only
        "# not a comment\n(print 1)\n": False,  # '#' is not a sym-char
        "(print 1: 2)\n": False,              # ':' not a sym-char
        "(print 1)\n# trailing\n": False,     # '#' not allowed
        "hello\n": True,                      # bare symbol
        "(list 1 2 3)\n": True,
        "((1 2) 3)\n": True,                  # nested lists
        "(print \"a:b#c\")\n": True,          # specials OK inside a string
    }
    ok = True
    for src, exp in cases.items():
        got = ainl_gbnf_accepts(src)
        mark = "ok" if got == exp else "FAIL"
        if got != exp:
            ok = False
        print(f"  {mark:4} expect={'A' if exp else 'R'} got={'A' if got else 'R'}  {src!r}")
    print("self-test:", "PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)
