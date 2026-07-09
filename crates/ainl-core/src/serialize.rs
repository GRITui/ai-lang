//! Stable JSON serialization of the AST, with source-map location info.
//!
//! This is the interchange format for AST tooling: a deterministic, diffable
//! JSON tree where every node carries both its byte `span` and a 1-based `loc`
//! (line, char-column). Downstream tools consume this instead of re-parsing
//! text, and the `loc`/`span` pair is exactly what a source map needs to trace
//! projected human-readable code back to the original AINL bytes.
//!
//! Hand-written (no serde) so `ainl-core` stays dependency-free and the runtime
//! compiles to a zero-dependency static binary.

use crate::parser::Node;

/// Maps byte offsets to 1-based (line, char-column) positions for source maps.
pub struct LineIndex<'a> {
    src: &'a str,
    /// Byte offset at which each line starts. Always begins with `0`.
    line_starts: Vec<usize>,
}

impl<'a> LineIndex<'a> {
    pub fn new(src: &'a str) -> LineIndex<'a> {
        let mut line_starts = vec![0];
        for (i, b) in src.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        LineIndex { src, line_starts }
    }

    /// 1-based `(line, column)` for a byte offset. Column is counted in
    /// characters (not bytes) so it is correct for non-ASCII source.
    pub fn locate(&self, offset: usize) -> (usize, usize) {
        let line = match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let line_start = self.line_starts[line];
        let col = self.src[line_start..offset.min(self.src.len())]
            .chars()
            .count();
        (line + 1, col + 1)
    }
}

/// Serialize a program (its top-level forms) to a stable, pretty-printed JSON
/// document. `source_name` is recorded in the output when provided.
pub fn forms_to_json(forms: &[Node], src: &str, source_name: Option<&str>) -> String {
    let idx = LineIndex::new(src);
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str("  \"version\": \"0.1\",\n");
    if let Some(name) = source_name {
        out.push_str("  \"source\": ");
        push_json_str(&mut out, name);
        out.push_str(",\n");
    }
    if forms.is_empty() {
        out.push_str("  \"forms\": []\n}");
        return out;
    }
    out.push_str("  \"forms\": [\n");
    for (i, form) in forms.iter().enumerate() {
        out.push_str("    ");
        write_node(form, &idx, 2, &mut out);
        if i + 1 < forms.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("  ]\n}");
    out
}

fn write_node(node: &Node, idx: &LineIndex, indent: usize, out: &mut String) {
    let span = node.span();
    let (line, col) = idx.locate(span.start);
    let trailer = format!(
        "\"span\": [{}, {}], \"loc\": [{}, {}]",
        span.start, span.end, line, col
    );
    match node {
        Node::Int(i, _) => {
            out.push_str(&format!("{{ \"t\": \"int\", \"v\": {i}, {trailer} }}"));
        }
        Node::Float(x, _) => {
            out.push_str(&format!(
                "{{ \"t\": \"float\", \"v\": {}, {trailer} }}",
                json_float(*x)
            ));
        }
        Node::Str(s, _) => {
            out.push_str("{ \"t\": \"str\", \"v\": ");
            push_json_str(out, s);
            out.push_str(&format!(", {trailer} }}"));
        }
        Node::Sym(s, _) => {
            out.push_str("{ \"t\": \"sym\", \"v\": ");
            push_json_str(out, s);
            out.push_str(&format!(", {trailer} }}"));
        }
        Node::List(items, _) => {
            if items.is_empty() {
                out.push_str(&format!("{{ \"t\": \"list\", {trailer}, \"items\": [] }}"));
                return;
            }
            let pad = "  ".repeat(indent);
            out.push_str("{\n");
            out.push_str(&format!("{pad}  \"t\": \"list\",\n"));
            out.push_str(&format!("{pad}  {trailer},\n"));
            out.push_str(&format!("{pad}  \"items\": [\n"));
            for (i, it) in items.iter().enumerate() {
                out.push_str(&"  ".repeat(indent + 2));
                write_node(it, idx, indent + 2, out);
                if i + 1 < items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&format!("{pad}  ]\n"));
            out.push_str(&format!("{pad}}}"));
        }
    }
}

/// Render an f64 as a valid JSON number, keeping a trailing `.0` on whole
/// values so the float-ness survives a round trip visually.
fn json_float(x: f64) -> String {
    if !x.is_finite() {
        return "null".to_string();
    }
    if x.fract() == 0.0 {
        format!("{x:.1}")
    } else {
        format!("{x}")
    }
}

fn push_json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}
