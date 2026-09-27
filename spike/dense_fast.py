#!/usr/bin/env python3
"""Sound, fast membership test for the DENSE-AINL GBNF language (spike t_4bb53833).

Why a dedicated parser (mirrors scripts/gen-harness/gbnf_fast.py):
  * The reference Earley (scripts/gbnf-conformance.py::gbnf_accepts) is correct
    but O(n^3) in practice (the `ws` star spawns O(n) origins per position) and
    recurses too deep for the dense grammar's expression spine — unusable as a
    per-output detector on ~400-char model generations.
  * The dense GBNF is a small, fixed context-free grammar (nested expressions /
    blocks need a stack, so a DFA is impossible) but is trivially parseable by a
    backtracking recursive-descent parser. This module accepts *precisely* the
    strings the dense GBNF accepts, in O(n) on well-formed input.

Soundness: the rules mirror spike/dense-ainl/dense.ainl.gbnf line for line.
It is cross-validated against the reference Earley in spike/dense_gbnf_check.py
(accept/reject probe set) — the same methodology that pins gbnf_fast.py.

Public API:
    dense_gbnf_accepts(s: str) -> bool
"""

_WS = set(" \t\r\n")


class _P:
    __slots__ = ("s", "i", "n")

    def __init__(self, s):
        self.s = s
        self.i = 0
        self.n = len(s)

    # -- low-level helpers -------------------------------------------------
    def save(self):
        return self.i

    def restore(self, p):
        self.i = p

    def peek(self):
        return self.s[self.i] if self.i < self.n else None

    def match_str(self, lit):
        if self.s.startswith(lit, self.i):
            self.i += len(lit)
            return True
        return False

    def is_ident_start(self):
        c = self.peek()
        return c is not None and (c.isalpha() or c == "_")

    def is_ident_char(self):
        c = self.peek()
        return c is not None and (c.isalnum() or c == "_")

    def is_digit(self):
        c = self.peek()
        return c is not None and c.isdigit()

    # ws ::= ( [ \t\r\n] | comment )*
    # Consumes a maximal run of whitespace/comments. Returns False only on a
    # dangling '#' comment with no terminating newline (the GBNF rejects that).
    def ws(self):
        s, n = self.s, self.n
        while self.i < n:
            c = s[self.i]
            if c in _WS:
                self.i += 1
            elif c == "#":
                self.i += 1  # consume '#'
                while self.i < n and s[self.i] != "\n":
                    self.i += 1
                if self.i >= n:
                    return False  # unterminated comment (no newline)
                self.i += 1  # consume the newline
            else:
                break
        return True

    # -- atoms -------------------------------------------------------------
    def ident(self):
        if not self.is_ident_start():
            return False
        while self.is_ident_char():
            self.i += 1
        return True

    # number ::= [0-9]+ ( "." [0-9]+ )?
    def number(self):
        s, n = self.s, self.n
        if not self.is_digit():
            return False
        while self.is_digit():
            self.i += 1
        if self.i < n and s[self.i] == ".":
            j = self.i + 1
            if j < n and s[j].isdigit():
                self.i = j
                while self.i < n and s[self.i].isdigit():
                    self.i += 1
        return True

    # string ::= "\"" ( [^"\\] | "\\" [\"\\/nrt] )* "\""
    def string(self):
        s, n = self.s, self.n
        if self.i >= n or s[self.i] != '"':
            return False
        self.i += 1
        while self.i < n:
            c = s[self.i]
            if c == "\\":
                if self.i + 1 < n and s[self.i + 1] in '"\\/nrt':
                    self.i += 2
                else:
                    return False
            elif c == '"':
                self.i += 1
                return True
            else:
                self.i += 1
        return False

    # -- statements --------------------------------------------------------
    def stmt(self):
        # assignment | if_stmt | while_stmt | return_stmt | expr_stmt
        for alt in (self.assignment, self.if_stmt, self.while_stmt,
                    self.return_stmt, self.expr_stmt):
            save = self.save()
            if alt():
                return True
            self.restore(save)
        return False

    # assignment ::= ident ws "=" ws expr
    def assignment(self):
        if not self.ident():
            return False
        if not self.ws():
            return False
        if not self.match_str("="):
            return False
        if not self.ws():
            return False
        return self.expr()

    # if_stmt ::= "if" ws expr ws "{" ws block "}" (ws "else" ws (if_stmt | "{" ws block "}") )?
    def if_stmt(self):
        if not self.match_str("if"):
            return False
        if not (self.ws() and self.expr() and self.ws()):
            return False
        if not self.match_str("{"):
            return False
        if not (self.ws() and self.block()):
            return False
        if not self.match_str("}"):
            return False
        # optional else
        save = self.save()
        if self.ws() and self.match_str("else"):
            if not self.ws():
                return False
            s2 = self.save()
            if self.if_stmt():
                return True
            self.restore(s2)
            if self.match_str("{") and self.ws() and self.block() \
                    and self.match_str("}"):
                return True
            return False
        self.restore(save)
        return True

    # while_stmt ::= "while" ws expr ws "{" ws block "}"
    def while_stmt(self):
        if not self.match_str("while"):
            return False
        if not (self.ws() and self.expr() and self.ws()):
            return False
        if not self.match_str("{"):
            return False
        if not (self.ws() and self.block()):
            return False
        return self.match_str("}")

    # return_stmt ::= "return" ws expr
    def return_stmt(self):
        if not self.match_str("return"):
            return False
        if not self.ws():
            return False
        return self.expr()

    def expr_stmt(self):
        return self.expr()

    # block ::= (ws stmt)* ws
    def block(self):
        while True:
            save = self.save()
            if not self.ws():
                return False
            if self.i >= self.n:
                break
            if not self.stmt():
                self.restore(save)
                break
        return self.ws()

    # -- expressions (precedence climbing, lowest to highest) --------------
    def expr(self):
        return self.ternary()

    # ternary ::= or_expr (ws "?" ws expr ws ":" ws expr)?
    def ternary(self):
        if not self.or_expr():
            return False
        save = self.save()
        if self.ws() and self.match_str("?"):
            if not (self.ws() and self.expr() and self.ws()):
                return False
            if not self.match_str(":"):
                return False
            if not self.ws():
                return False
            return self.expr()
        self.restore(save)
        return True

    def or_expr(self):
        if not self.and_expr():
            return False
        while True:
            save = self.save()
            if self.ws() and self.match_str("or") and self.ws() \
                    and self.and_expr():
                continue
            self.restore(save)
            break
        return True

    def and_expr(self):
        if not self.not_expr():
            return False
        while True:
            save = self.save()
            if self.ws() and self.match_str("and") and self.ws() \
                    and self.not_expr():
                continue
            self.restore(save)
            break
        return True

    # not_expr ::= "not" ws not_expr | cmp_expr
    def not_expr(self):
        save = self.save()
        if self.match_str("not") and self.ws() and self.not_expr():
            return True
        self.restore(save)
        return self.cmp_expr()

    def cmp_expr(self):
        if not self.add_expr():
            return False
        while True:
            save = self.save()
            if self.ws() and self.cmp_op() and self.ws() and self.add_expr():
                continue
            self.restore(save)
            break
        return True

    def cmp_op(self):
        for op in ("<=", ">=", "==", "!=", "<", ">"):
            if self.match_str(op):
                return True
        return False

    def add_expr(self):
        if not self.mul_expr():
            return False
        while True:
            save = self.save()
            if self.ws() and self.add_op() and self.ws() and self.mul_expr():
                continue
            self.restore(save)
            break
        return True

    def add_op(self):
        return self.match_str("+") or self.match_str("-")

    def mul_expr(self):
        if not self.unary():
            return False
        while True:
            save = self.save()
            if self.ws() and self.mul_op() and self.ws() and self.unary():
                continue
            self.restore(save)
            break
        return True

    def mul_op(self):
        return self.match_str("*") or self.match_str("/") or self.match_str("%")

    # unary ::= "-" ws unary | postfix
    def unary(self):
        save = self.save()
        if self.match_str("-") and self.ws() and self.unary():
            return True
        self.restore(save)
        return self.postfix()

    # postfix ::= primary (ws call_args)*
    def postfix(self):
        if not self.primary():
            return False
        while True:
            save = self.save()
            if self.ws() and self.call_args():
                continue
            self.restore(save)
            break
        return True

    # primary ::= number | string | list_lit | map_lit | fn_expr | ident | "(" ws expr ws ")"
    def primary(self):
        for alt in (self.number, self.string, self.list_lit, self.map_lit,
                    self.fn_expr, self.ident, self.paren):
            save = self.save()
            if alt():
                return True
            self.restore(save)
        return False

    def paren(self):
        if not self.match_str("("):
            return False
        if not (self.ws() and self.expr() and self.ws()):
            return False
        return self.match_str(")")

    # fn_expr ::= "fn" ws "(" ws params? ws ")" ws "{" ws block "}"
    def fn_expr(self):
        if not self.match_str("fn"):
            return False
        if not (self.ws() and self.match_str("(") and self.ws()):
            return False
        save = self.save()
        self.params()  # optional; result ignored, position left as-is on fail
        if not self.ws():
            return False
        if not self.match_str(")"):
            return False
        if not (self.ws() and self.match_str("{") and self.ws()):
            return False
        if not self.block():
            return False
        return self.match_str("}")

    def params(self):
        if not self.param():
            return False
        while True:
            save = self.save()
            if self.ws() and self.match_str(",") and self.ws() and self.param():
                continue
            self.restore(save)
            break
        return True

    # param ::= "&"? ident
    def param(self):
        save = self.save()
        if self.match_str("&"):
            self.ws()
        if not self.ident():
            self.restore(save)
            return False
        return True

    # call_args ::= "(" ws (expr ws ("," ws expr)*)? ws ")"
    def call_args(self):
        if not self.match_str("("):
            return False
        if not self.ws():
            return False
        save = self.save()
        if self.expr():
            if not self.ws():
                return False
            while True:
                s2 = self.save()
                if self.match_str(",") and self.ws() and self.expr():
                    continue
                self.restore(s2)
                break
            if not self.ws():
                return False
        else:
            self.restore(save)
        if not self.ws():
            return False
        return self.match_str(")")

    # list_lit ::= "[" ws (expr ws ("," ws expr)*)? ws "]"
    def list_lit(self):
        if not self.match_str("["):
            return False
        if not self.ws():
            return False
        save = self.save()
        if self.expr():
            if not self.ws():
                return False
            while True:
                s2 = self.save()
                if self.match_str(",") and self.ws() and self.expr():
                    continue
                self.restore(s2)
                break
            if not self.ws():
                return False
        else:
            self.restore(save)
        if not self.ws():
            return False
        return self.match_str("]")

    # map_lit ::= "{" ws (map_entry ws ("," ws map_entry)*)? ws "}"
    def map_lit(self):
        if not self.match_str("{"):
            return False
        if not self.ws():
            return False
        save = self.save()
        if self.map_entry():
            if not self.ws():
                return False
            while True:
                s2 = self.save()
                if self.match_str(",") and self.ws() and self.map_entry():
                    continue
                self.restore(s2)
                break
            if not self.ws():
                return False
        else:
            self.restore(save)
        if not self.ws():
            return False
        return self.match_str("}")

    # map_entry ::= (string | ident) ws ":" ws expr
    def map_entry(self):
        save = self.save()
        if self.string():
            pass
        else:
            self.restore(save)
            if not self.ident():
                return False
        if not (self.ws() and self.match_str(":") and self.ws()):
            return False
        return self.expr()

    # root ::= ws stmt (ws stmt)* ws
    def parse(self):
        if not self.ws():
            return False
        if not self.stmt():
            return False
        while self.i < self.n:
            save = self.save()
            if not self.ws():
                self.restore(save)
                break
            if self.i >= self.n:
                break
            if not self.stmt():
                self.restore(save)
                break
        return self.ws() and self.i == self.n


def dense_gbnf_accepts(s: str) -> bool:
    """True iff `s` is a member of the dense-AINL GBNF language."""
    try:
        return _P(s).parse()
    except (IndexError, RecursionError):
        return False


if __name__ == "__main__":
    import sys

    # Clean cases: well-formed dense AINL (accept) vs. structurally broken
    # (reject). These are the cases a human would unambiguously classify.
    clean = {
        # accept
        "x = 1\n": True,
        "print(1)\n": True,
        "a = 1 + 2 * 3\n": True,
        "if n < 2 { return n }\n": True,
        "while i < n { i = i + 1 }\n": True,
        "f = fn(x) { x * x }\n": True,
        "xs = [1, 2, 3]\n": True,
        "m = {\"a\": 1, \"b\": 2}\n": True,
        "m = {a: 1}\n": True,
        "y = a ? b : c\n": True,
        "z = not x\n": True,
        "w = x and y or z\n": True,
        "# a comment\nx = 1\n": True,
        "if a { x = 1 } else { x = 2 }\n": True,
        "if a { x = 1 } else if b { x = 2 }\n": True,
        "f = fn(&xs) { return go(xs, 0) }\n": True,
        "s = \"a\\nb\"\n": True,
        "r = -5\n": True,
        "q = (1 + 2)\n": True,
        "n = 3.14\n": True,
        # reject
        "": False,
        "\n  \n": False,
        "x =\n": False,
        "if a { x = 1\n": False,          # missing '}'
        "f = fn(x) { x * x\n": False,     # missing '}'
        "xs = [1, 2\n": False,            # missing ']'
        "m = {\"a\": 1\n": False,         # missing '}'
        "s = \"a\\qb\"\n": False,         # invalid escape
        "s = \"unterminated\n": False,    # unterminated string
        "x = 1 # dangling": False,        # comment w/o terminating newline
        "x ==\n": False,                  # dangling comparison
        "= 5\n": False,                   # leading '='
        "x = 1 = 2\n": False,             # double assignment
    }

    # Permissive cases: the GBNF ACCEPTS these (the parser is correct), but a
    # human would call them malformed. GBNF has no negative-lookahead, so
    # keywords (if/fn/while/return) are not reserved and bare expression
    # statements are whitespace-separated with no delimiter — so `x 1` parses
    # as two expr-stmts and `if a x = 1 { }` as `if`(expr) `a`(expr) `x=1`
    # (assign) `{}`(empty map). This is a known GBNF limitation, documented in
    # DENSITY_SPIKE.md, not a parser bug.
    permissive = {
        "x 1\n": True,                     # two bare expr-stmts
        "if a x = 1 { }\n": True,          # if/ident not reserved
        "fn = 1\n": True,                  # 'fn' usable as an identifier
    }

    ok = True
    for label, cases in (("clean", clean), ("permissive", permissive)):
        for src, exp in cases.items():
            got = dense_gbnf_accepts(src)
            mark = "ok" if got == exp else "FAIL"
            if got != exp:
                ok = False
            print(f"  [{label:10}] {mark:4} expect={'A' if exp else 'R'} "
                  f"got={'A' if got else 'R'}  {src!r}")
    print("self-test:", "PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)
