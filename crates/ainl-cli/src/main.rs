//! `ainl` — the command-line runtime for the AI-Native Language.
//!
//! Subcommands:
//!   run <file>     evaluate a program
//!   repl           interactive read-eval-print loop (--stdin for a script)
//!   ast <file>     print the parsed AST (with spans) for tooling / source maps
//!   eval <code>    evaluate a snippet passed on the command line
//!   compile <file> AOT-compile to a standalone C binary (AINL -> C -> cc)
//!   transpile <f>  project AINL into Python / JS / Ruby
//!   grammar        print the AINL grammar (GBNF, for constrained decoding)
//!   doctor         verify this install end to end (exit 0 only if all pass)
//!   version        print version, build target, and source commit

mod doctor;
mod gbnf;
mod gen;
mod gen_api;
mod repl;
mod test_runner;

use ainl_core::parser::Node;
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The one-line provenance string, shared by `version` and `doctor` so the two
/// can never disagree about what binary is running.
fn version_line() -> String {
    let commit = env!("AINL_GIT_COMMIT");
    let dirty = match env!("AINL_GIT_DIRTY") {
        "true" => " (dirty tree)",
        "false" => "",
        _ => " (tree state unknown)",
    };
    format!(
        "ainl {VERSION} {target}{dirty} ({commit})",
        target = env!("AINL_TARGET"),
    )
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run") => cmd_run(args.get(1)),
        Some("eval") => cmd_eval(&args[1..].join(" ")),
        Some("ast") => cmd_ast(&args[1..]),
        Some("compile") => cmd_compile(&args[1..]),
        Some("transpile") => cmd_transpile(&args[1..]),
        Some("grammar") => cmd_grammar(&args[1..]),
        Some("repl") => cmd_repl(&args[1..]),
        Some("test") => cmd_test(&args[1..]),
        Some("gen") => gen::run(&args[1..]),
        Some("doctor") => cmd_doctor(&args[1..]),
        Some("version") | Some("--version") | Some("-v") => {
            println!("{}", version_line());
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
         ainl compile <file.ainl> -o <out> --keep-c <file.c>   keep the generated C\n  \
         ainl transpile <file>    project AINL to another language (--to python|js|ruby)\n  \
         ainl grammar             print the AINL grammar (GBNF; --ebnf for EBNF)\n  \
         ainl repl                interactive REPL (multi-line input, --stdin for a script)\n  \
         ainl test [path]         run AINL test files; exit non-zero on failure\n  \
         ainl gen <spec>          generate AINL from a spec, then validate/compile/run\n  \
         ainl gen --help          the full gen contract: flags, env vars, exit codes\n  \
         ainl doctor              self-test this install (exit 0 only if all pass)\n\
         ainl version             print version, build target, and source commit\n"
    );
}

/// `ainl doctor` — self-diagnostic. Rejects unknown flags rather than ignoring
/// them, so a typo like `--verbose` is a visible error instead of a silently
/// different run.
fn cmd_doctor(rest: &[String]) -> ExitCode {
    let mut quiet = false;
    for arg in rest {
        match arg.as_str() {
            "--quiet" | "-q" => quiet = true,
            other => {
                eprintln!("unknown flag '{other}' (supported: --quiet, -q)");
                return ExitCode::FAILURE;
            }
        }
    }
    doctor::run(quiet)
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
    // `run_named_in`, not `run_str`: a program's `import` directives resolve
    // against the file's own directory, so `ainl run` must hand the path down
    // or the same program behaves differently depending on the cwd.
    match ainl_core::run_named_in(
        &src,
        std::path::Path::new(path),
        &ainl_core::Env::with_prelude(),
    ) {
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
    let mut keep_c: Option<&String> = None;
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
            "--keep-c" => {
                // Write the generated C to this path instead of a temp dir
                // (useful for inspecting the standalone output by hand).
                let Some(o) = rest.get(i + 1) else {
                    eprintln!("--keep-c needs a .c path");
                    return ExitCode::FAILURE;
                };
                keep_c = Some(o);
                i += 2;
            }
            flag if flag.starts_with("--") => {
                eprintln!(
                    "unknown flag '{flag}' (supported: -o <file>, --c-only, --keep-c <file>)"
                );
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
    // `generate` can now refuse a program (an `import` it cannot lower), so
    // the refusal is surfaced here rather than becoming a confusing failure
    // further down the C pipeline.
    let c = match ainl_cc::generate(&forms) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
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
    // Write the generated C to a temp dir next to the output binary, invoke
    // `cc` on it, then remove it — a compile should not litter the user's
    // working directory. `--keep-c` opts into keeping it for inspection.
    let out_dir = std::path::Path::new(&out)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let stem = std::path::Path::new(&out)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "ainl_prog".to_string());
    let c_path = match keep_c {
        Some(p) => p.to_string(),
        None => {
            let tmp = out_dir.join(format!(".{stem}.ainl-codegen.c"));
            match tmp.to_str() {
                Some(s) => s.to_string(),
                None => {
                    eprintln!("output path is not valid UTF-8");
                    return ExitCode::FAILURE;
                }
            }
        }
    };
    if let Err(e) = std::fs::write(&c_path, &c) {
        eprintln!("cannot write {c_path}: {e}");
        return ExitCode::FAILURE;
    }
    // `-lm` is not implied by libc: on glibc (Linux) fmod() and friends live
    // in libm, so without it the link fails with "undefined reference to
    // `fmod'" — the generated runtime's float formatting needs it. macOS
    // folds libm into libSystem, which is why this only shows up on Linux.
    // It is a no-op where the flag is redundant.
    let status = std::process::Command::new("cc")
        .args(["-O2", "-o", &out, &c_path, "-lm"])
        .status();
    if keep_c.is_none() {
        let _ = std::fs::remove_file(&c_path);
    }
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

/// `ainl repl [--stdin]`.
///
/// `--stdin` suppresses the banner and the `λ` prompt and is what a script or
/// a test uses; without it the same loop runs interactively. It is one flag
/// rather than a second implementation on purpose — a REPL whose tested path
/// and shipped path differ is a REPL whose tests prove nothing.
fn cmd_repl(rest: &[String]) -> ExitCode {
    let mut stdin_mode = false;
    for arg in rest {
        match arg.as_str() {
            "--stdin" => stdin_mode = true,
            other => {
                eprintln!("unknown flag '{other}' (supported: --stdin)");
                return ExitCode::FAILURE;
            }
        }
    }
    repl::run(stdin_mode)
}

/// `ainl test [path] [--quiet]`.
///
/// The path defaults to `tests` — the directory a checkout has one of, and the
/// one a CI step should be able to name without being told. Unknown flags are
/// rejected rather than ignored, so a typo cannot silently run *more* than the
/// author intended.
fn cmd_test(rest: &[String]) -> ExitCode {
    let mut quiet = false;
    let mut path: Option<&str> = None;
    for arg in rest {
        match arg.as_str() {
            "--quiet" | "-q" => quiet = true,
            flag if flag.starts_with('-') => {
                eprintln!("unknown flag '{flag}' (supported: --quiet, -q)");
                return ExitCode::FAILURE;
            }
            other => path = Some(other),
        }
    }
    test_runner::run(path.unwrap_or("tests"), quiet)
}
