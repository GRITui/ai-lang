//! JSON: `(json-parse string)` and `(json-serialize value)`.
//!
//! This module is the **normative** definition of both builtins. The AOT C
//! runtime (`crates/ainl-cc/src/runtime.c`) and the three transpiler targets
//! (`crates/ainl-transpile/src/{python,js,ruby}.rs`) reimplement exactly what is
//! here; `crates/ainl-cc/tests/aot_stdlib.rs` and the three `*_stdlib.rs`
//! suites diff their output against this one, so a rule that is only written
//! down in a comment is worthless. Every choice below is picked for
//! *reproducibility across four hosts*, not for resemblance to a host library —
//! where the hosts disagree (they do, in several places, deliberately) AINL
//! defines its own answer and the other three are written to match it.
//!
//! ## The four design decisions
//!
//! **1. Object keys must be strings.** AINL's map accepts *any* value as a key
//! (docs/SYNTAX.md §3), but JSON object names are strings, so a map with a
//! non-string key has no faithful JSON form. Serializing it as `{"1": 2}` or
//! coercing `sym` to its name would make two different AINL maps produce the
//! same bytes and silently break round-tripping. It is an error instead, in all
//! four backends, with the same message.
//!
//! **2. Key order is insertion order.** AINL's map *is* an ordered association
//! list, and `hash`/`assoc` document a repeat key as keeping its first
//! position, so insertion order is already the map's observable order in
//! `keys`/`vals` and in map equality. Reusing it means `json-serialize` needs no
//! sorting and cannot disagree with `(keys h)`. Sorting would also be
//! *unimplementable* identically: the four hosts' default string order is
//! codepoint order in Rust/JS/Python, byte order in Ruby for some cases, and
//! locale collation if anyone is tempted to use `sort` — the same trap
//! `list-dir` already had to avoid (see its unsigned-byte compare in C).
//!
//! **3. Numbers are emitted in one canonical decimal form, never scientific and
//! never a host's `repr`.** This is the sharpest edge in the whole feature.
//! AINL's ordinary float `Display` *already* diverges across these four
//! backends — measured, `(print 1e300)` gives
//!
//! | target | output |
//! |---|---|
//! | interpreter / AOT | a 301-digit fixed-point number |
//! | JS | `1e+300` |
//! | Python | the same 301-digit number |
//! | Ruby | `1.0e+300` |
//!
//! (docs/NUMERIC_MODEL.md records that divergence as pre-existing and out of
//! scope.) `json-serialize` therefore uses **its own** rule rather than
//! inheriting `Display`: plain fixed-point notation, never scientific, with a
//! `.0` on a whole value so the output is still unambiguously a float. The
//! digits are the shortest decimal that round-trips through `f64`, which is
//! computable in all four backends — unlike `Display`'s exact-expansion
//! branch, which the JS target cannot reproduce at all. See
//! [`format_json_float`] for the exact rule and the two reasons it is not
//! `Display`.
//!
//! **4. Non-finite floats are an error.** `json-serialize` of NaN or ±inf has
//! no valid JSON (the spec has no literal for them, and JS's `JSON.stringify`
//! silently substitutes `null`, which would make AINL lose a value rather than
//! report it). AINL errors in all four backends with one shared message, in the
//! spirit of the `split ""` / `replace ""` / `sqrt -1` rejections in
//! docs/SYNTAX.md §3: never invent a value where the hosts disagree.
//!
//! ## Round-tripping
//!
//! `parse ∘ serialize` is the identity on every value `json-parse` can
//! produce, and `serialize ∘ parse` is the identity on every well-formed JSON
//! document (modulo the object key order below). `crates/ainl-core/tests/`
//! pins this, and one caveat is worth stating plainly: AINL has **no float
//! type distinct from its integer type in every backend** — JS has one `number`
//! type, so a parsed `1.0` comes back as the int `1` there. That is a
//! pre-existing property of the JS target (docs/NUMERIC_MODEL.md), not
//! something this module introduces; `json-serialize` still emits `1.0` for a
//! float, so a JS program that round-trips still produces valid JSON.

use crate::error::{Error, Result};
use crate::eval::Env;
use crate::value::{ConsCell, Value};
use std::rc::Rc;

/// Deepest nesting `json-parse` will descend. Matches the AINL parser's own
/// `MAX_NEST_DEPTH` bound and keeps a hostile `[[[[…` document from
/// overflowing the native stack — the AOT C runtime and the transpiler targets
/// each carry the same constant for the same reason.
pub const MAX_JSON_DEPTH: usize = 512;

// ---- json-parse ------------------------------------------------------------

/// A recursive-descent JSON reader over `&[u8]` (rather than a `char`
/// iterator) so the "position" in an error message is a byte offset, which is
/// what a caller can act on, and so the C and Python ports can mirror the same
/// index arithmetic exactly.
struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn err<T>(&self, msg: &str) -> Result<T> {
        Err(Error::runtime(format!(
            "json-parse: {msg} at position {}",
            self.i
        )))
    }

    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn eat(&mut self, lit: &[u8], what: &str) -> Result<()> {
        if self.b[self.i..].starts_with(lit) {
            self.i += lit.len();
            Ok(())
        } else {
            self.err(&format!("expected {what}"))
        }
    }

    /// One JSON value. `depth` is the nesting level of the value *being
    /// returned*, starting at 0 for the whole document.
    fn value(&mut self, depth: usize) -> Result<Value> {
        if depth > MAX_JSON_DEPTH {
            return Err(Error::runtime(format!(
                "json-parse: nesting too deep (max {MAX_JSON_DEPTH} levels)"
            )));
        }
        self.ws();
        let Some(c) = self.peek() else {
            return self.err("unexpected end of input");
        };
        match c {
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b'"' => Ok(Value::Str(Rc::new(self.string()?))),
            b't' => {
                self.eat(b"true", "'true'")?;
                Ok(Value::Bool(true))
            }
            b'f' => {
                self.eat(b"false", "'false'")?;
                Ok(Value::Bool(false))
            }
            b'n' => {
                self.eat(b"null", "'null'")?;
                Ok(Value::Nil)
            }
            b'-' | b'0'..=b'9' => self.number(),
            _ => self.err("unexpected character"),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value> {
        self.i += 1; // '{'
        let mut pairs: Vec<(Value, Value)> = Vec::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Value::Map(Rc::new(pairs)));
        }
        loop {
            self.ws();
            if self.peek() != Some(b'"') {
                return self.err("expected a string key");
            }
            let k = self.string()?;
            self.ws();
            if self.peek() != Some(b':') {
                return self.err("expected ':' after a key");
            }
            self.i += 1;
            let v = self.value(depth + 1)?;
            // Same "last value wins, first position" rule as `hash`/`assoc`,
            // so a document with a duplicate key round-trips through AINL's
            // map exactly as it would through `(hash k v k v …)`.
            match pairs
                .iter_mut()
                .find(|(ek, _)| *ek == Value::Str(Rc::new(k.clone())))
            {
                Some((_, ev)) => *ev = v,
                None => pairs.push((Value::Str(Rc::new(k)), v)),
            }
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Value::Map(Rc::new(pairs)));
                }
                _ => return self.err("expected ',' or '}'"),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value> {
        self.i += 1; // '['
        let mut items = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Value::List(ConsCell::empty()));
        }
        loop {
            items.push(self.value(depth + 1)?);
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Value::List(ConsCell::from_values(items)));
                }
                _ => return self.err("expected ',' or ']'"),
            }
        }
    }

    fn string(&mut self) -> Result<String> {
        self.i += 1; // opening quote
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else {
                return self.err("unterminated string");
            };
            self.i += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let Some(e) = self.peek() else {
                        return self.err("unterminated escape");
                    };
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => out.push(self.unicode_escape()?),
                        _ => return self.err("invalid escape"),
                    }
                }
                // Raw control characters are not allowed inside a JSON string,
                // and rejecting them here is what stops a parse/serialize
                // round-trip from emitting a document no parser accepts.
                0x00..=0x1F => return self.err("control character in string"),
                _ => {
                    // Copy the whole UTF-8 sequence. The bytes were validated as
                    // UTF-8 when the AINL string was created, so a lead byte
                    // here always starts a complete character.
                    let start = self.i - 1;
                    let mut end = self.i;
                    while end < self.b.len() && (self.b[end] & 0xC0) == 0x80 {
                        end += 1;
                    }
                    self.i = end;
                    out.push_str(
                        std::str::from_utf8(&self.b[start..end])
                            .map_err(|_| Error::runtime("json-parse: invalid utf-8 in string"))?,
                    );
                }
            }
        }
    }

    /// `\uXXXX`, with surrogate pairs. A lone surrogate is an error rather than
    /// the replacement character: silently substituting U+FFFD would make
    /// `parse ∘ serialize` non-injective, and Rust cannot represent a lone
    /// surrogate in a `String` at all.
    fn unicode_escape(&mut self) -> Result<char> {
        let hi = self.hex4()?;
        // (0xD800..=0xDBFF) is the high half; it must be followed by `\uDC00..\uDFFF`.
        if (0xD800..=0xDBFF).contains(&hi) {
            if self.b[self.i..].starts_with(br"\u") {
                self.i += 2;
                let lo = self.hex4()?;
                if !(0xDC00..=0xDFFF).contains(&lo) {
                    return self.err("invalid low surrogate");
                }
                let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                return char::from_u32(cp)
                    .ok_or_else(|| Error::runtime("json-parse: invalid code point"));
            }
            return self.err("unpaired surrogate");
        }
        if (0xDC00..=0xDFFF).contains(&hi) {
            return self.err("unpaired surrogate");
        }
        char::from_u32(hi).ok_or_else(|| Error::runtime("json-parse: invalid code point"))
    }

    fn hex4(&mut self) -> Result<u32> {
        if self.i + 4 > self.b.len() {
            return self.err("truncated \\u escape");
        }
        let mut v = 0u32;
        for k in 0..4 {
            let c = self.b[self.i + k];
            let d = match c {
                b'0'..=b'9' => u32::from(c - b'0'),
                b'a'..=b'f' => u32::from(c - b'a') + 10,
                b'A'..=b'F' => u32::from(c - b'A') + 10,
                _ => return self.err("invalid \\u escape"),
            };
            v = v * 16 + d;
        }
        self.i += 4;
        Ok(v)
    }

    /// A JSON number. An integer that fits `i64` becomes `Value::Int` (so
    /// `(get (json-parse "{\"a\":1}") "a")` is an int and prints `1`, as a
    /// number written in the document should); anything with a fraction or an
    /// exponent, or too large for `i64`, is an `f64`.
    fn number(&mut self) -> Result<Value> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        // Integer part: either a lone '0' (leading zeros are not allowed) or a
        // run of digits with no leading zero.
        match self.peek() {
            Some(b'0') => {
                self.i += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return self.err("leading zero in number");
                }
            }
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.i += 1;
                }
            }
            _ => return self.err("expected a digit"),
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.i += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return self.err("expected a digit after '.'");
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.i += 1;
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            is_float = true;
            self.i += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.i += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return self.err("expected a digit in the exponent");
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.i += 1;
            }
        }
        let text = std::str::from_utf8(&self.b[start..self.i])
            .map_err(|_| Error::runtime("json-parse: invalid utf-8 in number"))?;
        if !is_float {
            // `-0` parses as the integer 0: AINL has one int type and no
            // negative zero, and `(= 0 -0)` is true anyway.
            if let Ok(i) = text.parse::<i64>() {
                return Ok(Value::Int(i));
            }
        }
        text.parse::<f64>()
            .map(Value::Float)
            .map_err(|_| Error::runtime(format!("json-parse: invalid number '{text}'")))
    }
}

/// `(json-parse string)` → an AINL value.
///
/// The whole document must be one value: trailing non-whitespace is an error,
/// which is what makes `(json-parse "[1,2] extra")` a typo rather than a
/// silently-truncated parse.
pub fn builtin_json_parse(args: &[Value]) -> Result<Value> {
    let [s] = args else {
        return Err(Error::runtime("json-parse expects (json-parse string)"));
    };
    let text = match s {
        Value::Str(s) => s,
        other => {
            return Err(Error::runtime(format!(
                "json-parse expects a str, got {}",
                other.type_name()
            )))
        }
    };
    let mut p = Parser {
        b: text.as_bytes(),
        i: 0,
    };
    let v = p.value(0)?;
    p.ws();
    if p.i != p.b.len() {
        return p.err("trailing content after the value");
    }
    Ok(v)
}

// ---- json-serialize --------------------------------------------------------

/// `(json-serialize value)` → a JSON string.
///
/// See the module docs for the four decisions. The recursion here is the
/// structural mirror of `json-parse`, so the two are easy to diff.
pub fn builtin_json_serialize(args: &[Value]) -> Result<Value> {
    let [v] = args else {
        return Err(Error::runtime(
            "json-serialize expects (json-serialize value)",
        ));
    };
    let mut out = String::new();
    write_value(&mut out, v, 0)?;
    Ok(Value::str(out))
}

fn write_value(out: &mut String, v: &Value, depth: usize) -> Result<()> {
    if depth > MAX_JSON_DEPTH {
        return Err(Error::runtime(format!(
            "json-serialize: nesting too deep (max {MAX_JSON_DEPTH} levels)"
        )));
    }
    match v {
        Value::Nil => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::Float(x) => out.push_str(&format_json_float(*x)?),
        Value::Str(s) => write_string(out, s),
        Value::List(items) => {
            out.push('[');
            let mut cur = items;
            let mut first = true;
            while let Some(v) = cur.first() {
                if !first {
                    out.push(',');
                }
                first = false;
                write_value(out, v, depth + 1)?;
                match cur.rest() {
                    Some(next) => cur = next,
                    None => break,
                }
            }
            out.push(']');
        }
        Value::Map(pairs) => {
            out.push('{');
            for (i, (k, v)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                // Decision 1: a non-string key has no faithful JSON form.
                let Value::Str(name) = k else {
                    return Err(Error::runtime(format!(
                        "json-serialize: object keys must be str, got {}",
                        k.type_name()
                    )));
                };
                write_string(out, name);
                out.push(':');
                write_value(out, v, depth + 1)?;
            }
            out.push('}');
        }
        // A symbol has no JSON form (`sym` is a distinct AINL type from `str`
        // precisely so that quoted data stays distinguishable), and a function
        // obviously has none.
        Value::Sym(_) => {
            return Err(Error::runtime("json-serialize: cannot serialize a sym"));
        }
        Value::Builtin { .. } | Value::Closure(_) => {
            return Err(Error::runtime(format!(
                "json-serialize: cannot serialize a {}",
                v.type_name()
            )));
        }
    }
    Ok(())
}

/// Write a JSON string literal, escaping exactly the characters JSON requires
/// plus the two C0 controls an AINL string can actually contain.
///
/// `\b` and `\f` are *not* used: a string holding U+0008 would otherwise
/// serialize to `\b` and the C/Python/Ruby ports — which spell that escape
/// differently in their source — would be three chances to drift. Using `\u0008`
/// everywhere is one rule, and it is the same rule a `json-parse` round-trip
/// reads back. Every other character, including all non-ASCII text, is emitted
/// literally as UTF-8: JSON is defined over Unicode, and escaping it would make
/// `json-serialize` of a Japanese string unreadable for no benefit.
fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
}

/// Decision 3: the one float spelling, identical in all five implementations:
/// plain **fixed-point** notation (never scientific), using the shortest
/// decimal digit string that reads back as the same `f64`, with `.0` appended
/// to a whole value.
///
/// Note what this rule is *not*: it is deliberately NOT `Value`'s `Display`
/// (value.rs), which branches to `{:.1}` for a whole float and so prints the
/// exact binary expansion — a 303-character answer for `1e300`. Following
/// `Display` here would have been the obvious move (it is what `(print 1e300)`
/// does) and it is wrong for JSON, for two independent reasons:
///
/// * **The AOT C runtime would have needed a second implementation.** Its
///   `format_float` is a hand-port of `Display`; a *different* rule for
///   json-serialize means a second, near-duplicate exact-decimal-expansion
///   routine in C that nothing else exercises.
/// * **The JS target could not have followed at all.** `toFixed` is specified
///   only up to 1e21 and falls back to exponential form beyond it, so
///   `(1e300).toFixed(1)` is the 6-character string `"1e+300"`. Emitting a
///   303-digit exact expansion from a JS `Number` needs arbitrary-precision
///   decimal arithmetic that does not exist there.
///
/// The rule below is computable in all four backends from the shortest
/// round-tripping digits, which is exactly why it is the rule. Each gets those
/// digits the same way — print with enough significant figures to round-trip
/// (Rust's `{}`, the C runtime's `%.{p}e` shortening loop, Python's `repr`,
/// JS's `toPrecision` shortening loop, Ruby's `%.{p}g` shortening loop) — then
/// places the decimal point by hand.
///
/// Both spellings parse back to the same `f64`. This is about producing the
/// same *bytes* in four backends, not about which decimal is "truer".
pub fn format_json_float(x: f64) -> Result<String> {
    if !x.is_finite() {
        return Err(Error::runtime(format!(
            "json-serialize: cannot serialize {x} (not a finite number)"
        )));
    }
    // `-0.0`: JSON has no negative zero, and `-0.0` parses back as int 0 in
    // AINL, so emitting `-0.0` would break the round-trip in a way the reader
    // cannot undo. Normalize it.
    if x == 0.0 {
        return Ok("0.0".to_string());
    }
    // Rust's `{}` is the shortest round-tripping decimal and is already
    // fixed-point for f64 (it never emits `1e-7` or `1e300`), so the only work
    // left is the `.0` that keeps a whole value a float in the output text.
    let s = format!("{x}");
    if s.contains('.') {
        Ok(s)
    } else {
        Ok(format!("{s}.0"))
    }
}

// ---- prelude wiring --------------------------------------------------------

/// Bind `json-parse` and `json-serialize` into `env`.
pub fn install(env: &Env) {
    env.define(
        "json-parse",
        Value::Builtin {
            name: "json-parse",
            f: builtin_json_parse,
        },
    );
    env.define(
        "json-serialize",
        Value::Builtin {
            name: "json-serialize",
            f: builtin_json_serialize,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Value> {
        builtin_json_parse(&[Value::str(s)])
    }
    fn ser(v: &Value) -> String {
        match builtin_json_serialize(std::slice::from_ref(v)).expect("serialize") {
            Value::Str(s) => s.as_str().to_string(),
            other => panic!("json-serialize returned {}", other.type_name()),
        }
    }
    fn rt(s: &str) -> String {
        ser(&parse(s).expect("parse"))
    }

    /// The card's round-trip property, stated as **value** identity:
    /// `parse(serialize(v)) == v`. Not text identity — `1e300` legitimately
    /// comes back as a 302-digit fixed-point literal, and `"\b"` as `"\u0008"`,
    /// because the canonical spelling is the point (decisions 3 and the
    /// control-character rule). The text is still valid JSON and reparses to
    /// the same value, which is what "the same data survives a round trip"
    /// has to mean once a canonical form is defined.
    fn rt_value(v: &Value) -> Value {
        let text = ser(v);
        let back = parse(&text).unwrap_or_else(|e| panic!("re-parsing {text:?} failed: {e}"));
        assert_eq!(back, *v, "round-trip changed the value: {text}");
        back
    }

    /// `parse(serialize(parse(s))) == parse(s)`: the true identity on the
    /// image of `parse`, and the direction the card's round-trip rule needs.
    fn rt_text(s: &str) -> String {
        let once = parse(s).unwrap_or_else(|e| panic!("parsing {s:?} failed: {e}"));
        rt_value(&once);
        ser(&parse(&ser(&once)).expect("reparse"))
    }

    #[test]
    fn scalars_parse() {
        assert_eq!(parse("null").unwrap(), Value::Nil);
        assert_eq!(parse("true").unwrap(), Value::Bool(true));
        assert_eq!(parse("false").unwrap(), Value::Bool(false));
        assert_eq!(parse("0").unwrap(), Value::Int(0));
        assert_eq!(parse("-0").unwrap(), Value::Int(0));
        assert_eq!(parse("42").unwrap(), Value::Int(42));
        assert_eq!(parse("-42").unwrap(), Value::Int(-42));
        assert_eq!(parse("1.5").unwrap(), Value::Float(1.5));
        assert_eq!(parse("1e3").unwrap(), Value::Float(1000.0));
        assert_eq!(parse("\"\"").unwrap(), Value::str(""));
        assert_eq!(parse("\"hi\"").unwrap(), Value::str("hi"));
    }

    #[test]
    fn i64_min_and_max_survive() {
        assert_eq!(parse("9223372036854775807").unwrap(), Value::Int(i64::MAX));
        assert_eq!(parse("-9223372036854775808").unwrap(), Value::Int(i64::MIN));
        // Past i64 it becomes a float, rather than erroring or wrapping.
        assert_eq!(
            parse("9223372036854775808").unwrap(),
            Value::Float(9223372036854775808.0)
        );
    }

    #[test]
    fn scalars_round_trip() {
        // `rt_text`, not `rt`: the identity is on the *value*, and the
        // canonical spelling deliberately differs from these inputs (`1e300`
        // -> a 302-digit fixed-point literal, `1e-7` -> `0.0000001`). What has
        // to hold is that the value is unchanged and the text is stable after
        // the first pass, which is exactly what the two-round form checks.
        for s in [
            "null",
            "true",
            "false",
            "0",
            "42",
            "-42",
            "1.5",
            "0.1",
            "1e300",
            "1e-7",
            "3.14159265358979",
            "\"\"",
            "\"hello\"",
        ] {
            let once = rt_text(s);
            assert_eq!(once, rt_text(&once), "round-trip of {s} is not idempotent");
        }
    }

    #[test]
    fn the_canonical_spelling_is_what_the_docs_promise() {
        // The exact text, pinned, because every one of the other four backends
        // is written to produce these bytes and CI diffs them.
        assert_eq!(rt("1.5"), "1.5");
        assert_eq!(rt("0.1"), "0.1");
        assert_eq!(rt("3.14159265358979"), "3.14159265358979");
        // 1e300 is whole, so it takes the shortest-form branch: 301 digits
        // plus the ".0" that marks it a float, i.e. 303 characters — which
        // happens to equal Display's exact-expansion length here, but is a
        // different string. The round-trip test below is what pins the rule.
        assert_eq!(rt("1e300").len(), 303);
        assert_eq!(rt("1e-7"), "0.0000001");
        assert_eq!(rt("42"), "42");
        assert_eq!(rt("null"), "null");
    }

    #[test]
    fn float_formatting_uses_the_shortest_round_tripping_digits() {
        // The rule is "shortest decimal that reads back as the same f64", NOT
        // Display's exact expansion. These two differ for every large whole
        // float: `{}` of 1e300 is 301 characters (a 1 and 300 zeros), while
        // Display's `{:.1}` is 303 (the exact binary expansion). The rule is
        // the shortest form because that is the only one all four backends can
        // compute — JS `toFixed` is undefined above 1e21 and returns
        // exponential form there.
        let shortest = format!("{}", 1e300f64);
        assert_eq!(shortest.len(), 301);
        assert_ne!(shortest, format!("{:.1}", 1e300f64));
        // json-serialize takes the shortest form and marks it a float.
        assert_eq!(format_json_float(1e300).unwrap(), format!("{shortest}.0"));
        // And it always round-trips back to the identical f64.
        for x in [
            1e300f64,
            1e16,
            1.0,
            123456789012345678.0,
            1e-7,
            0.1,
            1.0 / 3.0,
            2.5,
            -2.5,
            1e21,
            5e-324,
            f64::MAX,
            f64::MIN_POSITIVE,
            1e-300,
        ] {
            let s = format_json_float(x).unwrap();
            assert_eq!(
                s.parse::<f64>().unwrap(),
                x,
                "{s} does not read back as {x:e}"
            );
            assert!(!s.contains('e') && !s.contains('E'), "{s} is scientific");
        }
    }

    #[test]
    fn a_whole_float_is_distinguishable_from_an_int_in_the_output() {
        // The reason the rule appends ".0": without it, json-serialize of a
        // float 2.0 would be the two bytes `2`, identical to the int 2, and a
        // reader could not tell a float from an int at all.
        assert_eq!(format_json_float(2.0).unwrap(), "2.0");
        assert_eq!(rt("2.0"), "2.0");
        assert_eq!(rt("2"), "2");
        assert_ne!(rt("2.0"), rt("2"));
    }

    #[test]
    fn whole_float_keeps_its_point() {
        // 1.0 must not come back as `1`, or a float would be indistinguishable
        // from an int in the output text.
        assert_eq!(rt("1.0"), "1.0");
        assert_eq!(rt("2.0"), "2.0");
        assert_eq!(rt("-0.0"), "0.0");
        assert_eq!(rt("1e2"), "100.0");
    }

    #[test]
    fn float_formatting_is_the_documented_rule() {
        assert_eq!(format_json_float(0.1).unwrap(), "0.1");
        assert_eq!(format_json_float(1.0).unwrap(), "1.0");
        assert_eq!(format_json_float(-1.5).unwrap(), "-1.5");
        // 1/3 is the classic shortest-round-trip case.
        assert_eq!(format_json_float(1.0 / 3.0).unwrap(), "0.3333333333333333");
        // Never scientific, whatever the magnitude. 1e300 is the 1 followed by
        // 300 zeros, so fixed-point is 301 digits + "." + the "0" that marks
        // it a float = 303 characters.
        let big = format_json_float(1e300).unwrap();
        assert_eq!(big.len(), 303);
        assert!(!big.contains('e'));
        let small = format_json_float(1e-300).unwrap();
        assert!(!small.contains('e'));
        assert_eq!(small.len(), 302); // 0. + 299 zeros + the digit 1
    }

    #[test]
    fn non_finite_floats_are_errors() {
        for x in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let e = format_json_float(x).unwrap_err();
            assert!(
                e.to_string().contains("not a finite number"),
                "unexpected message: {e}"
            );
        }
    }

    #[test]
    fn escapes_parse() {
        assert_eq!(parse(r#""a\"b""#).unwrap(), Value::str("a\"b"));
        assert_eq!(parse(r#""a\\b""#).unwrap(), Value::str("a\\b"));
        assert_eq!(parse(r#""a\/b""#).unwrap(), Value::str("a/b"));
        assert_eq!(parse(r#""a\nb""#).unwrap(), Value::str("a\nb"));
        assert_eq!(parse(r#""a\rb""#).unwrap(), Value::str("a\rb"));
        assert_eq!(parse(r#""a\tb""#).unwrap(), Value::str("a\tb"));
        assert_eq!(parse(r#""\b\f""#).unwrap(), Value::str("\u{8}\u{c}"));
        assert_eq!(parse(r#""A""#).unwrap(), Value::str("A"));
        assert_eq!(parse(r#""😀""#).unwrap(), Value::str("\u{1F600}"));
        // Literal UTF-8 passes through untouched.
        assert_eq!(parse("\"日本\"").unwrap(), Value::str("日本"));
    }

    #[test]
    fn escapes_round_trip() {
        // The value is unchanged and the text is stable, but the canonical
        // spelling is not necessarily the input's: `\b` and `\f` are read on
        // input and written as `\u0008` / `\u000c`, and an uppercase `\u0041`
        // is written lowercase. One spelling on output is the whole point —
        // it is what makes the three transpiler ports and the C runtime able
        // to agree without each spelling five escapes its own way.
        for s in [
            r#""a\"b""#,
            r#""a\\b""#,
            r#""a\/b""#,
            r#""a\nb""#,
            r#""a\rb""#,
            r#""a\tb""#,
            r#""\b""#,
            r#""\f""#,
            r#""\u0000""#,
            r#""\u001f""#,
            r#""\u00e9""#,
            r#""\ud83d\ude00""#,
            r#""\u0041""#,
            "\"日本\"",
            "\"héllo\"",
            r#""""#,
        ] {
            let once = rt_text(s);
            assert_eq!(once, rt_text(&once), "round-trip of {s} is not idempotent");
        }
    }

    #[test]
    fn the_escape_spellings_are_pinned() {
        // Read-with-either-spelling, write-in-exactly-one.
        assert_eq!(rt(r#""\b""#), r#""\u0008""#);
        assert_eq!(rt(r#""\f""#), r#""\u000c""#);
        assert_eq!(rt(r#""\u0008""#), r#""\u0008""#);
        assert_eq!(rt(r#""\n\r\t""#), r#""\n\r\t""#);
        // The three escapes that must stay short: quote, backslash, solidus.
        // (Spelled out rather than written as one literal, because a raw string
        // holding backslash-quote sequences is easy to mis-count.)
        assert_eq!(ser(&Value::str("\"")), r#""\"""#);
        assert_eq!(ser(&Value::str("\\")), r#""\\""#);
        assert_eq!(ser(&Value::str("/")), r#""/""#, "solidus needs no escape");
        assert_eq!(rt(r#""\/""#), r#""/""#);
        assert_eq!(rt(r#""\u0041""#), r#""A""#); // printable -> literal
        assert_eq!(rt(r#""\u00e9""#), "\"é\""); // non-ASCII -> literal UTF-8
        assert_eq!(
            rt(r#""\ud83d\ude00""#),
            "\"\u{1F600}\"",
            "a surrogate pair becomes the character itself"
        );
    }

    #[test]
    fn control_characters_serialize_as_u_escapes() {
        // A raw U+0008 must not be emitted as `\b`: one spelling, everywhere.
        assert_eq!(ser(&Value::str("\u{8}")), r#""\u0008""#);
        assert_eq!(ser(&Value::str("\u{c}")), r#""\u000c""#);
        assert_eq!(ser(&Value::str("a\u{1}b")), r#""a\u0001b""#);
    }

    #[test]
    fn objects_and_arrays_parse_and_round_trip() {
        let s = r#"{"a":1,"b":[true,null,"x"],"c":{"d":-2.5}}"#;
        assert_eq!(rt(s), s);
        let v = parse(s).unwrap();
        let Value::Map(pairs) = &v else { panic!() };
        assert_eq!(pairs[0].0, Value::str("a"));
        assert_eq!(pairs[0].1, Value::Int(1));
        assert_eq!(pairs[1].0, Value::str("b"));
    }

    #[test]
    fn key_order_is_insertion_order() {
        // Not sorted: `a` was written first, so it comes back first.
        let s = r#"{"z":1,"a":2,"m":3}"#;
        assert_eq!(rt(s), s);
        // And it agrees with `keys`, which is the same order the map already has.
        let v = parse(s).unwrap();
        let Value::Map(pairs) = &v else { panic!() };
        let names: Vec<&str> = pairs
            .iter()
            .map(|(k, _)| match k {
                Value::Str(s) => s.as_str(),
                _ => panic!("a json-parse key is always a str"),
            })
            .collect();
        assert_eq!(names, ["z", "a", "m"]);
    }

    #[test]
    fn whitespace_is_ignored() {
        let v = parse("  {\n  \"a\" : [ 1 , 2 ]\t}\r\n ").unwrap();
        assert_eq!(ser(&v), r#"{"a":[1,2]}"#);
    }

    #[test]
    fn empty_containers() {
        assert_eq!(rt("[]"), "[]");
        assert_eq!(rt("{}"), "{}");
        assert_eq!(rt("[[]]"), "[[]]");
        assert_eq!(rt(r#"{"a":[],"b":{}}"#), r#"{"a":[],"b":{}}"#);
    }

    #[test]
    fn duplicate_keys_keep_the_last_value_and_first_position() {
        // Same rule as `hash`/`assoc`, so a document with a repeated key
        // collapses exactly as `(hash "a" 1 "a" 2)` does.
        assert_eq!(rt(r#"{"a":1,"b":2,"a":3}"#), r#"{"a":3,"b":2}"#);
    }

    #[test]
    fn nested_structures_round_trip() {
        let s = r#"[{"a":[{"b":[[[]]]},2,3]},[],{},null,true]"#;
        assert_eq!(rt(s), s);
    }

    #[test]
    fn malformed_documents_are_rejected() {
        for bad in [
            "",
            "  ",
            "{",
            "[",
            "[1,",
            "[1,]",
            "{,}",
            "{\"a\"}",
            "{\"a\":}",
            "{\"a\":1,}",
            "{a:1}",
            "{'a':1}",
            "[1 2]",
            "tru",
            "nul",
            "01",
            "-",
            "1.",
            ".5",
            "1e",
            "1e+",
            "+1",
            "\"unterminated",
            "\"bad \\q escape\"",
            "\"\\u00\"",
            "\"\\ud800\"",
            "\"\\udc00\"",
            "\"\\ud800\\u0041\"",
            "[1] trailing",
            "{} {}",
            "NaN",
            "Infinity",
        ] {
            assert!(
                parse(bad).is_err(),
                "`{bad}` should have been rejected, but parsed"
            );
        }
    }

    #[test]
    fn raw_control_characters_in_strings_are_rejected() {
        assert!(parse("\"a\nb\"").is_err());
        assert!(parse("\"a\tb\"").is_err());
    }

    #[test]
    fn non_string_keys_are_rejected() {
        let v = Value::Map(Rc::new(vec![(Value::Int(1), Value::Int(2))]));
        let e = builtin_json_serialize(&[v]).unwrap_err();
        assert!(
            e.to_string()
                .contains("json-serialize: object keys must be str, got int"),
            "unexpected message: {e}"
        );
        // A symbol key is its own error, not a coercion.
        let v = Value::Map(Rc::new(vec![(
            Value::Sym(Rc::new("a".into())),
            Value::Int(1),
        )]));
        let e = builtin_json_serialize(&[v]).unwrap_err();
        assert!(
            e.to_string()
                .contains("json-serialize: object keys must be str, got sym"),
            "unexpected message: {e}"
        );
    }

    #[test]
    fn unserializable_values_are_rejected() {
        for v in [
            Value::Sym(Rc::new("x".into())),
            Value::Closure(Rc::new(crate::value::Closure {
                params: vec![],
                variadic: None,
                body: vec![],
                env: Env::new(),
                code: None,
            })),
        ] {
            let e = builtin_json_serialize(std::slice::from_ref(&v)).unwrap_err();
            assert!(
                e.to_string().contains("json-serialize: cannot serialize"),
                "unexpected message: {e}"
            );
        }
    }

    #[test]
    fn nesting_is_bounded() {
        let deep = format!("{}{}", "[".repeat(600), "]".repeat(600));
        let e = parse(&deep).unwrap_err();
        assert!(
            e.to_string().contains("nesting too deep"),
            "unexpected message: {e}"
        );
        // Just inside the bound still works.
        let ok = format!("{}{}", "[".repeat(200), "]".repeat(200));
        assert!(parse(&ok).is_ok());
    }

    #[test]
    fn arity_and_type_errors() {
        assert!(builtin_json_parse(&[]).is_err());
        assert!(builtin_json_parse(&[Value::Int(1)]).is_err());
        assert!(builtin_json_parse(&[Value::str("1"), Value::str("2")]).is_err());
        assert!(builtin_json_serialize(&[]).is_err());
        assert!(builtin_json_serialize(&[Value::Int(1), Value::Int(2)]).is_err());
        // `Error::Runtime`'s Display prepends "runtime error: ", which the
        // CLI shows; the *message* the other backends must match is the
        // interior, so that's what is asserted here.
        let e = builtin_json_parse(&[Value::Int(1)]).unwrap_err();
        assert_eq!(
            e,
            Error::runtime("json-parse expects a str, got int"),
            "the type-error message must name the builtin and the actual type"
        );
        assert_eq!(
            builtin_json_parse(&[]).unwrap_err(),
            Error::runtime("json-parse expects (json-parse string)")
        );
        assert_eq!(
            builtin_json_serialize(&[]).unwrap_err(),
            Error::runtime("json-serialize expects (json-serialize value)")
        );
    }
}
