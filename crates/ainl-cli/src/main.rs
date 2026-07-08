//! `ainl` — the command-line runtime for the AI-Native Language.
//!
//! Subcommands:
//!   run <file>     evaluate a program
//!   repl           interactive read-eval-print loop
//!   ast <file>     print the parsed AST (with spans) for tooling / source maps
//!   eval <code>    evaluate a snippet passed on the command line
//!   version        print version

use ainl_core::{parser::Node, Env};
use std::io::{self, BufRead, Write};
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run") => cmd_run(args.get(1)),
        Some("eval") => cmd_eval(&args[1..].join(" ")),
        Some("ast") => cmd_ast(args.get(1)),
        Some("repl") => cmd_repl(),
        Some("version") | Some("--version") | Some("-v") => {
            println!("ainl {VERSION}");
            ExitCode::SUCCESS
        }
        Some("help") | Some("--help") | Some("-h") | None => {
            print_help();
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("unknown command '{other}'\n");
            print_help();
            ExitCode::FAILURE
        }
    }
}

fn print_help() {
    println!(
        "ainl {VERSION} — the AI-Native Language runtime\n\n\
         USAGE:\n  \
         ainl run <file.ainl>     evaluate a program file\n  \
         ainl eval <code>         evaluate a snippet\n  \
         ainl ast <file.ainl>     print the parsed AST with source spans\n  \
         ainl repl                start an interactive REPL\n  \
         ainl version             print version\n"
    );
}

fn cmd_run(path: Option<&String>) -> ExitCode {
    let Some(path) = path else {
        eprintln!("usage: ainl run <file.ainl>");
        return ExitCode::FAILURE;
    };
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    match ainl_core::run_str(&src) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_eval(code: &str) -> ExitCode {
    match ainl_core::run_str(code) {
        Ok(v) => {
            println!("{v}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_ast(path: Option<&String>) -> ExitCode {
    let Some(path) = path else {
        eprintln!("usage: ainl ast <file.ainl>");
        return ExitCode::FAILURE;
    };
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    match ainl_core::parse(&src) {
        Ok(forms) => {
            for form in &forms {
                print_node(form, 0);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// Indented AST dump. Every line shows the node kind, value, and byte span so a
/// consumer can build a source map back to the original AINL.
fn print_node(node: &Node, depth: usize) {
    let pad = "  ".repeat(depth);
    let s = node.span();
    match node {
        Node::Int(i, _) => println!("{pad}Int {i} [{}..{}]", s.start, s.end),
        Node::Float(x, _) => println!("{pad}Float {x} [{}..{}]", s.start, s.end),
        Node::Str(v, _) => println!("{pad}Str {v:?} [{}..{}]", s.start, s.end),
        Node::Sym(v, _) => println!("{pad}Sym {v} [{}..{}]", s.start, s.end),
        Node::List(items, _) => {
            println!("{pad}List [{}..{}]", s.start, s.end);
            for it in items {
                print_node(it, depth + 1);
            }
        }
    }
}

fn cmd_repl() -> ExitCode {
    println!("ainl {VERSION} REPL — type (exit) or Ctrl-D to quit");
    let env = Env::with_prelude();
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    loop {
        print!("λ ");
        let _ = stdout.flush();
        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => {
                println!();
                break;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("input error: {e}");
                break;
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "(exit)" || trimmed == "exit" {
            break;
        }
        match ainl_core::run_in(trimmed, &env) {
            Ok(v) => println!("{}", v.repr()),
            Err(e) => eprintln!("{e}"),
        }
    }
    ExitCode::SUCCESS
}
