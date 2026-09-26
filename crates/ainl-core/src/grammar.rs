//! The canonical AINL grammar, in machine-consumable forms.
//!
//! The same low-entropy grammar the parser accepts, exported for
//! **grammar-constrained decoding** so a model can be forced to emit only
//! programs that parse. `SYNTAX.md` documents this grammar for humans/models;
//! these constants are the mechanical version.

/// GBNF grammar (llama.cpp / `grammars` format). Feed to a constrained decoder
/// to force valid AINL output.
pub const GBNF: &str = r#"# AINL v0.1 — GBNF grammar for constrained decoding.
# Every string this grammar accepts is a syntactically valid AINL program.
root    ::= ws form (ws form)* ws
form    ::= list | atom
list    ::= "(" ws (form ws)* ")"
atom    ::= (string | number | symbol) ws
string  ::= "\"" ( [^"\\] | "\\" ["\\/nrt] )* "\""
number  ::= "-"? [0-9]+ ( "." [0-9]+ )? ( [eE] "-"? [0-9]+ )?
symbol  ::= sym-char+
sym-char ::= [a-zA-Z0-9] | "+" | "-" | "*" | "/" | "<" | ">" | "=" | "!" | "?" | "." | "_" | "&"
ws      ::= ( [ \t\n\r] | comment )*
comment ::= ";" [^\n]* "\n"
"#;

/// EBNF grammar — the same language in a notation-neutral form for docs and
/// alternative constrained-decoding backends (e.g. Outlines, Lark).
pub const EBNF: &str = r#"(* AINL v0.1 — EBNF grammar *)
program = { form } ;
form    = list | atom ;
list    = "(" , { form } , ")" ;
atom    = number | string | symbol ;
number  = [ "-" ] , digit , { digit } , [ "." , digit , { digit } ] ;
string  = '"' , { char - '"' | '\' , any } , '"' ;
symbol  = sym_char , { sym_char } ;   (* any run of non-delimiter chars not parsed as a number *)
(* delimiters that end a symbol: whitespace ( ) " ; *)
"#;

/// Return the requested grammar dialect.
pub fn grammar(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::Gbnf => GBNF,
        Dialect::Ebnf => EBNF,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Gbnf,
    Ebnf,
}
