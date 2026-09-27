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
        Some("ast") => cmd_ast(&args[1..]),
        Some("compile") => cmd_compile(&args[1..]),
        Some("transpile") => cmd_transpile(&args[1..]),
        Some("grammar") => cmd_grammar(&args[1..]),
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
         ainl ast <file> --json   emit the AST as stable JSON (with source-map loc)\n  \
         ainl ast <file> --json-out <f>  read a JSON AST back (inverse of --json)\n  \
         ainl compile <file.ainl> -o <out>   AOT-compile to a standalone C binary (via cc)\n  \
         ainl transpile <file>    project AINL to another language (--to python|js|ruby)\n  \
         ainl grammar             print the AINL grammar (GBNF; --ebnf for EBNF)\n  \
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

fn cmd_ast(rest: &[String]) -> ExitCode {
    let mut path: Option<&String> = None;
    let mut json = false;
    let mut json_out: Option<&String> = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--json" => json = true,
            "--json-out" => {
                let Some(f) = rest.get(i + 1) else {
                    eprintln!("--json-out needs a file (JSON AST to read back)");
                    return ExitCode::FAILURE;
                };
                json_out = Some(f);
                i += 1;
            }
            flag if flag.starts_with("--") => {
                eprintln!("unknown flag '{flag}' (supported: --json, --json-out <file>)");
                return ExitCode::FAILURE;
            }
            _ => path = Some(&rest[i]),
        }
        i += 1;
    }
    let Some(path) = path else {
        eprintln!("usage: ainl ast <file.ainl> [--json] [--json-out <json-file>]");
        return ExitCode::FAILURE;
    };
    // `--json-out` mode: read a JSON AST document back into a Node tree and
    // print it (indented, with spans). This is the deserialization direction —
    // the inverse of `--json` — and lets you verify a round trip:
    //   ainl ast x.ainl --json > x.json
    //   ainl ast x.ainl --json-out x.json   # must match `ainl ast x.ainl`
    if let Some(json_path) = json_out {
        let doc = match std::fs::read_to_string(json_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("cannot read {json_path}: {e}");
                return ExitCode::FAILURE;
            }
        };
        return match ainl_core::json_to_forms(&doc) {
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
        };
    }
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    match ainl_core::parse(&src) {
        Ok(forms) => {
            if json {
                println!("{}", ainl_core::forms_to_json(&forms, &src, Some(path)));
            } else {
                for form in &forms {
                    print_node(form, 0);
                }
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

fn cmd_compile(rest: &[String]) -> ExitCode {
    let mut path: Option<&String> = None;
    let mut out: Option<&String> = None;
    let mut c_only = false;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "-o" => {
                let Some(o) = rest.get(i + 1) else {
                    eprintln!("-o needs an output path");
                    return ExitCode::FAILURE;
                };
                out = Some(o);
                i += 2;
            }
            "--c-only" => {
                c_only = true;
                i += 1;
            }
            flag if flag.starts_with("--") => {
                eprintln!("unknown flag '{flag}' (supported: -o <file>, --c-only)");
                return ExitCode::FAILURE;
            }
            _ => {
                path = Some(&rest[i]);
                i += 1;
            }
        }
    }
    let Some(path) = path else {
        eprintln!("usage: ainl compile <file.ainl> -o <out> [--c-only]");
        return ExitCode::FAILURE;
    };
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let forms = match ainl_core::parse(&src) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let c = ainl_cc::generate(&forms);
    if c_only {
        match out {
            Some(o) => {
                if let Err(e) = std::fs::write(o, c) {
                    eprintln!("cannot write {o}: {e}");
                    return ExitCode::FAILURE;
                }
                println!("wrote {o}");
            }
            None => print!("{c}"),
        }
        return ExitCode::SUCCESS;
    }
    let out = match out {
        Some(o) => o.clone(),
        None => {
            eprintln!("usage: ainl compile <file.ainl> -o <out>");
            return ExitCode::FAILURE;
        }
    };
    // Write the generated .c next to the output binary, then invoke cc.
    let stem = std::path::Path::new(&out)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "ainl_prog".to_string());
    let c_path = format!("{stem}.c");
    if let Err(e) = std::fs::write(&c_path, &c) {
        eprintln!("cannot write {c_path}: {e}");
        return ExitCode::FAILURE;
    }
    let status = std::process::Command::new("cc")
        .args(["-O2", "-o", &out, &c_path])
        .status();
    match status {
        Ok(s) if s.success() => {
            println!("compiled {path} -> {out}");
            ExitCode::SUCCESS
        }
        Ok(s) => {
            eprintln!("cc failed with {s}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("failed to run cc: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_transpile(rest: &[String]) -> ExitCode {
    let mut path: Option<&String> = None;
    let mut target = "python".to_string();
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--to" => {
                let Some(t) = rest.get(i + 1) else {
                    eprintln!("--to needs a language (e.g. --to python)");
                    return ExitCode::FAILURE;
                };
                target = t.clone();
                i += 2;
            }
            flag if flag.starts_with("--") => {
                eprintln!("unknown flag '{flag}' (supported: --to <lang>)");
                return ExitCode::FAILURE;
            }
            _ => {
                path = Some(&rest[i]);
                i += 1;
            }
        }
    }
    let Some(path) = path else {
        eprintln!("usage: ainl transpile <file.ainl> [--to python|js|ruby]");
        return ExitCode::FAILURE;
    };
    let Some(target) = ainl_transpile::Target::from_name(&target) else {
        eprintln!("unsupported target '{target}' (supported: python, js, ruby)");
        return ExitCode::FAILURE;
    };
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    match ainl_transpile::transpile_src(target, &src) {
        Ok(code) => {
            print!("{code}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_grammar(rest: &[String]) -> ExitCode {
    let dialect = match rest.first().map(String::as_str) {
        None | Some("--gbnf") => ainl_core::Dialect::Gbnf,
        Some("--ebnf") => ainl_core::Dialect::Ebnf,
        Some(other) => {
            eprintln!("unknown flag '{other}' (supported: --gbnf, --ebnf)");
            return ExitCode::FAILURE;
        }
    };
    print!("{}", ainl_core::grammar::grammar(dialect));
    ExitCode::SUCCESS
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
