//! JSON → AST deserialization: the inverse of [`crate::serialize`].
//!
//! Reads the stable JSON AST document back into a `Node` tree, recovering the
//! exact `Span` (byte range) on every node — so `parse → to_json → from_json`
//! is a lossless round trip, including the source-map location info.
//!
//! Hand-written (no serde) so `ainl-core` stays dependency-free, mirroring the
//! serializer. The JSON value parser is a small iterative reader over the
//! document's bytes (explicit stack, so arbitrarily deep JSON can't overflow
//! the native stack); the document shape it accepts is exactly what
//! `serialize::forms_to_json` emits:
//!
//! ```json
//! { "version": "0.1", "source": "…",
//!   "forms": [ { "t": "list", "span": [s,e], "loc": [line,col], "items": [ … ] } ] }
//! ```
//!
//! Unknown node types, missing required fields, or extra fields are rejected
//! rather than silently dropped — a malformed document must not produce a
//! silently-wrong AST.

use crate::error::{Error, Result};
use crate::parser::{Node, Span};

/// Max list nesting accepted from a JSON document. The serializer can only
/// emit as deep as the parser allows (512), but a hand-edited document could
/// nest arbitrarily; this bound keeps the recursive `parse_node` descent from
/// overflowing the native stack on a hostile input. (The JSON *value* parser
/// itself is iterative and unbounded in depth.)
const MAX_JSON_DEPTH: usize = 1024;

/// The interchange-format version this deserializer understands.
const SUPPORTED_VERSION: &str = "0.1";

/// Read a stable JSON AST document (as emitted by `forms_to_json`) back into
/// a `Node` tree. Every node's `Span` is recovered exactly; `loc` is validated
/// as a two-element non-negative array but is not stored (it is a pure
/// function of `span` + the original source).
pub fn json_to_forms(json: &str) -> Result<Vec<Node>> {
    let mut p = Parser {
        b: json.as_bytes(),
        i: 0,
    };
    p.skip_ws();
    let v = p.parse_value()?;
    p.skip_ws();
    if p.i != p.b.len() {
        return Err(p.err("trailing data after JSON document"));
    }
    let obj = v
        .as_object()
        .ok_or_else(|| p.err("top-level JSON value must be an object"))?;

    let mut version = None;
    let mut source = None;
    let mut forms = None;
    for (k, val) in obj {
        match k.as_str() {
            "version" => version = Some(val.clone()),
            "source" => source = Some(val.clone()),
            "forms" => forms = Some(val.clone()),
            other => {
                return Err(Error::Json {
                    msg: format!("unknown top-level field '{other}'"),
                    at: 0,
                    loc: None,
                })
            }
        }
    }
    let version = version
        .ok_or_else(|| Error::Json {
            msg: "missing 'version'".into(),
            at: 0,
            loc: None,
        })?
        .as_str()
        .ok_or_else(|| Error::Json {
            msg: "'version' must be a string".into(),
            at: 0,
            loc: None,
        })?
        .to_string();
    if version != SUPPORTED_VERSION {
        return Err(Error::Json {
            msg: format!("unsupported version '{version}' (expected '{SUPPORTED_VERSION}')"),
            at: 0,
            loc: None,
        });
    }
    let _ = source; // recorded by the serializer for provenance; not part of the AST
    let forms = forms.ok_or_else(|| Error::Json {
        msg: "missing 'forms'".into(),
        at: 0,
        loc: None,
    })?;
    let forms = forms.as_array().ok_or_else(|| Error::Json {
        msg: "'forms' must be an array".into(),
        at: 0,
        loc: None,
    })?;

    let mut nodes = Vec::with_capacity(forms.len());
    for (idx, fv) in forms.iter().enumerate() {
        nodes.push(parse_node(fv, 0, &mut |at, msg| Error::Json {
            msg: format!("forms[{idx}]: {msg}"),
            at,
            loc: None,
        })?);
    }
    Ok(nodes)
}

type ErrFn<'a> = &'a mut dyn FnMut(usize, String) -> Error;

/// A unit of work in the iterative node builder (see `parse_node`).
enum Work<'a> {
    /// Parse this JSON value into a node.
    Node(&'a Value, usize),
    /// Close the innermost open list, emitting its `Node::List`.
    Close(Span),
}

/// A list node being assembled (innermost first on the builder's stack).
/// The node's `Span` is carried by the matching `Work::Close(span)` marker, so
/// the frame only needs its items.
struct ListFrame {
    items: Vec<Node>,
}

fn parse_node(v: &Value, depth: usize, err: ErrFn<'_>) -> Result<Node> {
    // Iterative post-order build so arbitrarily deep lists can't overflow the
    // native stack (a 512-deep program is legal; the old recursive descent
    // needed one frame per level). `work` is a LIFO of values still to parse;
    // `frames` holds list nodes currently open (innermost last).
    let mut work: Vec<Work> = vec![Work::Node(v, depth)];
    let mut frames: Vec<ListFrame> = Vec::new();
    let mut root: Option<Node> = None;

    while let Some(w) = work.pop() {
        match w {
            Work::Close(span) => {
                let f = frames
                    .pop()
                    .ok_or_else(|| err(0, "unbalanced list close".into()))?;
                let node = Node::List(f.items, span);
                match frames.last_mut() {
                    Some(parent) => parent.items.push(node),
                    None => root = Some(node),
                }
            }
            Work::Node(val, d) => {
                let obj = val
                    .as_object()
                    .ok_or_else(|| err(0, "node must be a JSON object".into()))?;
                // Borrow (never clone) from the original tree: the top-level
                // `Value` is alive for the whole call, so references into it are
                // stable across loop iterations of the explicit work stack.
                let mut t: Option<&str> = None;
                let mut span: Option<Span> = None;
                let mut payload: Option<(&str, &Value)> = None;
                for (k, v) in obj {
                    match k.as_str() {
                        "t" => t = v.as_str(),
                        "span" => span = Some(parse_pair(v, err, "span")?),
                        "loc" => {
                            // Validated as a two-element non-negative array; not
                            // stored (it is a pure function of span + source).
                            parse_pair(v, err, "loc")?;
                        }
                        "v" | "items" => payload = Some((k.as_str(), v)),
                        other => return Err(err(0, format!("unknown node field '{other}'"))),
                    }
                }
                let span = span.ok_or_else(|| err(0, "missing 'span'".into()))?;
                let t = t.ok_or_else(|| err(0, "missing 't'".into()))?;
                let (field, val) =
                    payload.ok_or_else(|| err(0, "missing value field ('v' or 'items')".into()))?;

                if t == "list" {
                    if field != "items" {
                        return Err(err(0, "list node: expected 'items'".into()));
                    }
                    if d >= MAX_JSON_DEPTH {
                        return Err(err(
                            0,
                            format!("nesting too deep (max {MAX_JSON_DEPTH} levels)"),
                        ));
                    }
                    let items = val
                        .as_array()
                        .ok_or_else(|| err(0, "list node: 'items' must be an array".into()))?;
                    // Open this list, then schedule a close marker followed by
                    // its items (pushed in reverse so they pop in order).
                    frames.push(ListFrame {
                        items: Vec::with_capacity(items.len()),
                    });
                    work.push(Work::Close(span));
                    for iv in items.iter().rev() {
                        work.push(Work::Node(iv, d + 1));
                    }
                } else {
                    let node = match t {
                        "int" => {
                            let n = val.as_int().ok_or_else(|| {
                                err(0, format!("int node: 'v' must be an integer, got {val:?}"))
                            })?;
                            Node::Int(n, span)
                        }
                        "float" => {
                            let f = val.as_float().ok_or_else(|| {
                                err(0, format!("float node: 'v' must be a number, got {val:?}"))
                            })?;
                            Node::Float(f, span)
                        }
                        "str" => {
                            if field != "v" {
                                return Err(err(0, "str node: expected 'v'".into()));
                            }
                            let s = val
                                .as_str()
                                .ok_or_else(|| err(0, "str node: 'v' must be a string".into()))?;
                            Node::Str(s.to_string(), span)
                        }
                        "sym" => {
                            if field != "v" {
                                return Err(err(0, "sym node: expected 'v'".into()));
                            }
                            let s = val
                                .as_str()
                                .ok_or_else(|| err(0, "sym node: 'v' must be a string".into()))?;
                            Node::Sym(s.to_string(), span)
                        }
                        other => return Err(err(0, format!("unknown node type '{other}'"))),
                    };
                    match frames.last_mut() {
                        Some(parent) => parent.items.push(node),
                        None => root = Some(node),
                    }
                }
            }
        }
    }
    root.ok_or_else(|| err(0, "empty node".into()))
}

fn parse_pair(v: &Value, err: ErrFn<'_>, what: &str) -> Result<Span> {
    let arr = v
        .as_array()
        .ok_or_else(|| err(0, format!("'{what}' must be a two-element array")))?;
    if arr.len() != 2 {
        return Err(err(0, format!("'{what}' must have exactly 2 elements")));
    }
    let a = nonneg_int(&arr[0], err, what)?;
    let b = nonneg_int(&arr[1], err, what)?;
    Ok(Span::new(a, b))
}

fn nonneg_int(v: &Value, err: ErrFn<'_>, what: &str) -> Result<usize> {
    match v.as_int() {
        Some(n) if n >= 0 => Ok(n as usize),
        _ => Err(err(
            0,
            format!("'{what}' element must be a non-negative integer"),
        )),
    }
}

// ---- minimal JSON value model + parser -------------------------------------

/// A parsed JSON value. Only the shape the deserializer needs is kept.
#[derive(Clone, Debug)]
enum Value {
    Null,
    Bool,
    Int(i64),
    Float(f64),
    Str(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

impl Value {
    fn as_object(&self) -> Option<&[(String, Value)]> {
        match self {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }
    fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }
    fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
    /// Integer if the JSON number had no fractional/exponent part.
    fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            _ => None,
        }
    }
    /// Any JSON number as f64.
    fn as_float(&self) -> Option<f64> {
        match self {
            Value::Int(n) => Some(*n as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }
}

/// Position within a container being parsed (see `Parser::parse_value`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// First member; the container may be empty (closed immediately).
    First,
    /// After a `,` — expecting the next member.
    Item,
    /// Object only: after `:` — expecting a value.
    Value,
}

/// A container on the explicit parse stack.
enum Frame {
    Object {
        entries: Vec<(String, Value)>,
        /// The key whose value is still pending (set between `:` and the value).
        key: Option<String>,
        state: State,
    },
    Array {
        items: Vec<Value>,
        state: State,
    },
}

fn frame_value(frame: Frame) -> Value {
    match frame {
        Frame::Object { entries, .. } => Value::Object(entries),
        Frame::Array { items, .. } => Value::Array(items),
    }
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn err(&self, msg: impl Into<String>) -> Error {
        Error::Json {
            msg: msg.into(),
            at: self.i,
            loc: None,
        }
    }

    fn skip_ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn parse_value(&mut self) -> Result<Value> {
        // Iterative (explicit stack) so arbitrarily deep JSON can't overflow the
        // native stack. AINL caps list nesting at 512, but each level becomes a
        // JSON object *and* an array, so the document nests ~2x deeper than the
        // source; a hand-edited document could nest far deeper still.
        let mut stack: Vec<Frame> = Vec::new();
        let mut current: Option<Value> = None;
        loop {
            // 1. Place a pending value into the innermost container (or finish).
            if let Some(val) = current.take() {
                if stack.is_empty() {
                    return Ok(val);
                }
                match stack.last_mut().unwrap() {
                    Frame::Array { items, state } => {
                        items.push(val);
                        *state = State::Item;
                    }
                    Frame::Object {
                        entries,
                        key,
                        state,
                    } => {
                        let k = key
                            .take()
                            .ok_or_else(|| self.err("object value without a key"))?;
                        entries.push((k, val));
                        *state = State::Item;
                    }
                }
                self.skip_ws();
                let (close_ch, err_msg) = match stack.last().unwrap() {
                    Frame::Array { .. } => (b']', "expected ',' or ']' in array"),
                    Frame::Object { .. } => (b'}', "expected ',' or '}' in object"),
                };
                match self.peek() {
                    Some(b',') => self.i += 1,
                    Some(c) if c == close_ch => {
                        self.i += 1;
                        let v = frame_value(stack.pop().unwrap());
                        if stack.is_empty() {
                            return Ok(v);
                        }
                        current = Some(v);
                        continue;
                    }
                    _ => return Err(self.err(err_msg)),
                }
                continue;
            }
            // 2. No pending value: start the next member.
            if stack.is_empty() {
                current = self.start_value(&mut stack)?;
                continue;
            }
            let (is_array, state) = match stack.last().unwrap() {
                Frame::Array { state, .. } => (true, *state),
                Frame::Object { state, .. } => (false, *state),
            };
            // An empty container in its first state closes immediately.
            if state == State::First {
                self.skip_ws();
                let close_ch = if is_array { b']' } else { b'}' };
                if self.peek() == Some(close_ch) {
                    self.i += 1;
                    let v = frame_value(stack.pop().unwrap());
                    if stack.is_empty() {
                        return Ok(v);
                    }
                    current = Some(v);
                    continue;
                }
            }
            if is_array || state == State::Value {
                current = self.start_value(&mut stack)?;
            } else {
                self.skip_ws();
                let key = self.parse_string()?;
                self.skip_ws();
                if self.peek() != Some(b':') {
                    return Err(self.err("expected ':' after object key"));
                }
                self.i += 1;
                if let Frame::Object {
                    key: k, state: s, ..
                } = stack.last_mut().unwrap()
                {
                    *k = Some(key);
                    *s = State::Value;
                }
            }
        }
    }

    /// Parse a scalar into a value, or open a container (pushing a frame).
    /// Returns the scalar if one was read, or `None` if a container was opened.
    fn start_value(&mut self, stack: &mut Vec<Frame>) -> Result<Option<Value>> {
        self.skip_ws();
        let c = self
            .peek()
            .ok_or_else(|| self.err("unexpected end of JSON input"))?;
        Ok(match c {
            b'{' => {
                self.i += 1;
                stack.push(Frame::Object {
                    entries: Vec::new(),
                    key: None,
                    state: State::First,
                });
                None
            }
            b'[' => {
                self.i += 1;
                stack.push(Frame::Array {
                    items: Vec::new(),
                    state: State::First,
                });
                None
            }
            b'"' => Some(Value::Str(self.parse_string()?)),
            b't' => Some(self.parse_literal("true", Value::Bool)?),
            b'f' => Some(self.parse_literal("false", Value::Bool)?),
            b'n' => Some(self.parse_literal("null", Value::Null)?),
            c if c == b'-' || c.is_ascii_digit() => Some(self.parse_number()?),
            other => {
                let ch = other as char;
                return Err(self.err(format!("unexpected character '{ch}' in JSON")));
            }
        })
    }

    fn parse_literal(&mut self, word: &str, val: Value) -> Result<Value> {
        if self.b.len() - self.i >= word.len()
            && &self.b[self.i..self.i + word.len()] == word.as_bytes()
        {
            self.i += word.len();
            Ok(val)
        } else {
            Err(self.err("invalid JSON literal"))
        }
    }

    fn parse_string(&mut self) -> Result<String> {
        self.i += 1; // opening '"'
        let mut s = String::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(self.err("unterminated JSON string"));
            };
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let Some(e) = self.peek() else {
                        return Err(self.err("unterminated escape in JSON string"));
                    };
                    self.i += 1;
                    match e {
                        b'"' => s.push('"'),
                        b'\\' => s.push('\\'),
                        b'/' => s.push('/'),
                        b'n' => s.push('\n'),
                        b't' => s.push('\t'),
                        b'r' => s.push('\r'),
                        b'b' => s.push('\u{0008}'),
                        b'f' => s.push('\u{000C}'),
                        b'u' => {
                            let cp = self.parse_hex4()?;
                            s.push(
                                char::from_u32(cp).ok_or_else(|| self.err("invalid \\u escape"))?,
                            );
                        }
                        other => {
                            let ch = other as char;
                            return Err(self.err(format!("invalid escape '\\{ch}'")));
                        }
                    }
                }
                _ => s.push(c as char),
            }
        }
        Ok(s)
    }

    fn parse_hex4(&mut self) -> Result<u32> {
        if self.i + 4 > self.b.len() {
            return Err(self.err("truncated \\u escape"));
        }
        let h = std::str::from_utf8(&self.b[self.i..self.i + 4])
            .map_err(|_| self.err("invalid \\u escape"))?;
        let v = u32::from_str_radix(h, 16).map_err(|_| self.err("invalid \\u escape"))?;
        self.i += 4;
        Ok(v)
    }

    fn parse_number(&mut self) -> Result<Value> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        // Integer part: `0` or `[1-9][0-9]*` — JSON forbids leading zeros.
        match self.peek() {
            Some(b'0') => {
                self.i += 1;
                if let Some(c) = self.peek() {
                    if c.is_ascii_digit() {
                        return Err(self.err("leading zeros are not allowed in JSON numbers"));
                    }
                }
            }
            Some(c) if c.is_ascii_digit() => {
                while let Some(c) = self.peek() {
                    if c.is_ascii_digit() {
                        self.i += 1;
                    } else {
                        break;
                    }
                }
            }
            _ => return Err(self.err("expected digit in JSON number")),
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.i += 1;
            let mut digits = false;
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    digits = true;
                    self.i += 1;
                } else {
                    break;
                }
            }
            if !digits {
                return Err(self.err("expected digit after '.' in JSON number"));
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            is_float = true;
            self.i += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.i += 1;
            }
            let mut digits = false;
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    digits = true;
                    self.i += 1;
                } else {
                    break;
                }
            }
            if !digits {
                return Err(self.err("expected digit in JSON number exponent"));
            }
        }
        let text =
            std::str::from_utf8(&self.b[start..self.i]).map_err(|_| self.err("invalid number"))?;
        if is_float {
            text.parse::<f64>()
                .map(Value::Float)
                .map_err(|_| self.err("invalid JSON number"))
        } else {
            text.parse::<i64>()
                .map(Value::Int)
                .map_err(|_| self.err("integer out of range in JSON number"))
        }
    }
}
