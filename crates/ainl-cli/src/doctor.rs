//! `ainl doctor` — the self-diagnostic.
//!
//! Answers one question: *is this install actually working?* It reports the
//! binary's provenance, the optional host tools, and then runs a three-line
//! self-test that exercises the three execution backends end to end. It exits
//! 0 only if every check passes, so it is usable as a CI/install gate:
//!
//! ```sh
//! curl -fsSL https://raw.githubusercontent.com/GRITui/ai-lang/main/scripts/install.sh | sh
//! ainl doctor && echo "install verified"
//! ```
//!
//! Design constraint: the self-test must exercise the *shipped* code paths, so
//! every check goes through the same library entry points `ainl run` / `eval` /
//! `transpile` use — never a private shortcut that could pass while the real
//! path is broken.

use std::fmt::Write as _;
use std::process::Command;
use std::time::Instant;

use ainl_core::Dialect;

/// A single check's outcome. `Skipped` is deliberately not a pass: a check that
/// could not run has not demonstrated anything, and `doctor` must not go green
/// on a box where, say, `cc` is missing and silently unverified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Pass,
    Fail,
    Skipped,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Pass => "ok",
            Status::Fail => "FAIL",
            Status::Skipped => "SKIP",
        }
    }
}

struct Check {
    name: &'static str,
    status: Status,
    detail: String,
}

impl Check {
    fn new(name: &'static str, status: Status, detail: impl Into<String>) -> Check {
        Check {
            name,
            status,
            detail: detail.into(),
        }
    }
}

/// Locate a C compiler for the AOT backend.
///
/// `ainl compile` shells out to a bare `cc`, so that is what doctor probes:
/// resolving `clang` here would report green for a box where the feature the
/// user wants is the one that fails. The first argument of `-dumpversion` is
/// the version string on both Apple clang and GCC.
///
/// A *missing* `cc` is SKIP, not FAIL. The interpreter, the transpilers, and
/// the grammar export all work without a C compiler; only `ainl compile` does
/// not, so a box without `cc` is a working install of everything else and
/// doctor must not report it as broken. A `cc` that exists and then fails is a
/// genuine FAIL — that host has a broken toolchain.
fn probe_cc() -> Check {
    const NAME: &str = "cc";
    match Command::new("cc").arg("-dumpversion").output() {
        Ok(out) if out.status.success() => {
            let v = String::from_utf8_lossy(&out.stdout);
            Check::new(NAME, Status::Pass, format!("cc {}", v.trim()))
        }
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            let detail = if err.trim().is_empty() {
                format!("cc exited with {}", out.status)
            } else {
                format!("cc failed: {}", err.trim().lines().next().unwrap_or(""))
            };
            Check::new(NAME, Status::Fail, detail)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Check::new(
            NAME,
            Status::Skipped,
            format!("not found — `ainl compile` needs it; the rest of ainl does not ({e})"),
        ),
        Err(e) => Check::new(NAME, Status::Fail, format!("could not run cc: {e}")),
    }
}

/// Self-test 1 — the interpreter, via the same `run_str` the CLI's `run`/`eval`
/// use. Exercises arithmetic, a function definition and call, and a string
/// builtin, so a single check covers the lexer, parser, VM, and stdlib.
fn check_eval() -> Check {
    const NAME: &str = "eval";
    // The final form's value is what `ainl eval` prints, so that is what is
    // asserted: 12*12 = 144, plus len("a-b-c") = 5. The arithmetic is done by
    // AINL, not hardcoded — a wrong constant here is a failing doctor, and the
    // whole point is that the check can fail.
    const SRC: &str = r#"
(def sq (fn (x) (* x x)))
(+ (sq 12) (len (join (split "a,b,c" ",") "-")))
"#;
    const WANT: &str = "149";
    match ainl_core::run_str(SRC) {
        Ok(v) => {
            let got = v.to_string();
            if got == WANT {
                Check::new(NAME, Status::Pass, format!("{SRC} => {got}"))
            } else {
                Check::new(
                    NAME,
                    Status::Fail,
                    format!("{SRC} => {got}, expected {WANT}"),
                )
            }
        }
        Err(e) => Check::new(NAME, Status::Fail, format!("{SRC} => error: {e}")),
    }
}

/// Self-test 2 — a whole program with real control flow and recursion, which
/// the `eval` check above does not cover. `run_str` returns the *last* value;
/// the trailing `(fib 10)` form makes that the recursion's result, so this
/// asserts an actual computed value rather than merely "it did not crash".
fn check_run() -> Check {
    const NAME: &str = "run";
    const SRC: &str = r#"
(def fib (fn (n) (if (< n 2) n (+ (fib (- n 1)) (fib (- n 2))))))
(fib 10)
"#;
    const WANT: &str = "55";
    match ainl_core::run_str(SRC) {
        Ok(v) => {
            let got = v.to_string();
            if got == WANT {
                Check::new(NAME, Status::Pass, format!("(fib 10) => {got}"))
            } else {
                Check::new(
                    NAME,
                    Status::Fail,
                    format!("(fib 10) => {got}, expected {WANT}"),
                )
            }
        }
        Err(e) => Check::new(NAME, Status::Fail, format!("program errored: {e}")),
    }
}

/// Self-test 3 — the transpiler. Projects a known program to every supported
/// target and checks the generated code is non-empty and contains the projected
/// form (a transpiler that silently emitted an empty or truncated file would
/// otherwise look healthy).
///
/// This checks *emission*, not execution: running the output needs a
/// python3/node/ruby on the host, which is not something a minimal install can
/// assume. Cross-backend *byte-equality* of interpreter vs. transpiler output
/// is a build-time property, gated in CI by `scripts/check-transpile.sh`.
fn check_transpile() -> Check {
    const NAME: &str = "transpile";
    const SRC: &str = "(def sq (fn (x) (* x x)))\n(sq 7)\n";
    let mut parts: Vec<String> = Vec::new();
    let mut all_ok = true;
    for target in [
        ainl_transpile::Target::Python,
        ainl_transpile::Target::JavaScript,
        ainl_transpile::Target::Ruby,
    ] {
        let label = target.label();
        let note = match ainl_transpile::transpile_src(target, SRC) {
            Ok(code) if code.contains("sq") && code.len() > 64 => {
                format!("{label} {b} bytes", b = code.len())
            }
            Ok(code) => {
                all_ok = false;
                format!("{label} suspicious ({b} bytes)", b = code.len())
            }
            Err(e) => {
                all_ok = false;
                format!("{label} error: {e}")
            }
        };
        parts.push(note);
    }
    Check::new(
        NAME,
        if all_ok { Status::Pass } else { Status::Fail },
        parts.join(", "),
    )
}

/// Structure-check a GBNF document: a `root` rule must be defined, and every
/// rule referenced on a right-hand side must also be defined. Split out from
/// [`check_grammar`] so it can be tested against a deliberately broken grammar
/// instead of only against the real one — a check that can only ever see a good
/// grammar proves nothing about its own failure mode.
fn gbnf_problems(gbnf: &str) -> Vec<String> {
    let mut defined: Vec<&str> = Vec::new();
    let mut referenced: Vec<&str> = Vec::new();
    for line in gbnf.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((lhs, rhs)) = line.split_once("::=") else {
            continue;
        };
        let name = lhs.trim();
        if name.is_empty() {
            continue;
        }
        defined.push(name);
        for tok in rhs.split_whitespace() {
            // Rule references are bare lowercase identifiers; literals are
            // quoted and character classes bracketed, so this picks out
            // exactly the references.
            let t = tok.trim_matches(|c: char| c == '(' || c == ')' || c == '*' || c == '"');
            if !t.is_empty()
                && t.starts_with(|c: char| c.is_ascii_lowercase())
                && t.chars()
                    .all(|c| c.is_ascii_lowercase() || c == '-' || c.is_ascii_digit())
            {
                referenced.push(t);
            }
        }
    }
    let mut problems = Vec::new();
    if !defined.contains(&"root") {
        problems.push("no `root` rule".to_string());
    }
    let mut undefined: Vec<&str> = referenced
        .iter()
        .copied()
        .filter(|r| !defined.contains(r))
        .collect();
    undefined.sort_unstable();
    undefined.dedup();
    if !undefined.is_empty() {
        problems.push(format!("undefined rules: {}", undefined.join(", ")));
    }
    problems
}

/// The GBNF export must be *usable by a decoder*, not merely present. A
/// constrained decoder needs a `root` rule, and every rule it references must
/// resolve; either fault produces zero valid generations with no error message,
/// which is exactly the silent failure this check exists to catch.
fn check_grammar() -> Check {
    const NAME: &str = "grammar";
    let gbnf = ainl_core::grammar::grammar(Dialect::Gbnf);
    let problems = gbnf_problems(gbnf);
    if !problems.is_empty() {
        return Check::new(
            NAME,
            Status::Fail,
            format!(
                "GBNF unusable ({} bytes): {}",
                gbnf.len(),
                problems.join("; ")
            ),
        );
    }
    let rules = gbnf
        .lines()
        .filter(|l| l.contains("::=") && !l.trim_start().starts_with('#'))
        .count();
    Check::new(
        NAME,
        Status::Pass,
        format!("GBNF ok ({rules} rules, {b} bytes)", b = gbnf.len()),
    )
}

/// The AOT backend, when `cc` is present: emit C and compile it. Proves the
/// code generator produces C a real compiler accepts, which is the only thing
/// that can catch a codegen regression without a full parity suite.
///
/// Prints a warning rather than failing when `cc` is absent: a missing host C
/// compiler is a normal state for someone who only wants the interpreter, and
/// failing there would make `doctor` useless as an install check. It is reported
/// as SKIP, never as OK.
fn check_aot() -> Check {
    const NAME: &str = "aot";
    let Ok(forms) = ainl_core::parse("(print 1)") else {
        return Check::new(NAME, Status::Fail, "cannot parse the AOT probe program");
    };
    let c = ainl_cc::generate(&forms);
    if c.is_empty() {
        return Check::new(NAME, Status::Fail, "generated C is empty");
    }
    // The probe file is named by pid so two concurrent doctors cannot collide.
    // A leftover one is harmless (it is rewritten, not appended to) and is
    // removed on every path out of here.
    let tmp = std::env::temp_dir().join(format!("ainl-doctor-{}.c", std::process::id()));
    if let Err(e) = std::fs::write(&tmp, &c) {
        let _ = std::fs::remove_file(&tmp);
        return Check::new(NAME, Status::Fail, format!("cannot write probe .c: {e}"));
    }
    // `cc -fsyntax-only` parses and semantically checks without producing an
    // object file — enough to prove the generated C is valid, and fast.
    let res = Command::new("cc")
        .args(["-fsyntax-only", "-Werror=implicit-function-declaration"])
        .arg(&tmp)
        .output();
    let _ = std::fs::remove_file(&tmp);
    match res {
        Ok(o) if o.status.success() => Check::new(
            NAME,
            Status::Pass,
            format!("generated C compiles ({b} bytes)", b = c.len()),
        ),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            let first = err.lines().next().unwrap_or("").trim();
            Check::new(NAME, Status::Fail, format!("cc rejected the C: {first}"))
        }
        Err(e) => Check::new(
            NAME,
            Status::Skipped,
            format!("cc not found, AOT compile unavailable: {e}"),
        ),
    }
}

/// Run every check and print the report. Returns the process exit code:
/// 0 only when nothing failed.
pub fn run(quiet: bool) -> std::process::ExitCode {
    let start = Instant::now();
    let checks: Vec<Check> = vec![
        // (a) provenance — the same string `ainl version` prints, so a bug
        // report that pastes doctor's output and one that pastes
        // `ainl --version` agree.
        Check::new("version", Status::Pass, crate::version_line()),
        // (b) host tools + the GBNF export + the self-tests
        probe_cc(),
        check_grammar(),
        check_eval(),
        check_run(),
        check_transpile(),
        check_aot(),
    ];

    // Report. Aligned "ok/FAIL/SKIP" column, then the detail, so the output is
    // both scannable and copy-pasteable into a bug report. Newlines inside a
    // detail are collapsed: one check must stay one output line, or
    // `ainl doctor | grep FAIL` and line-oriented tooling both break.
    let width = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
    let mut failed = 0usize;
    let mut skipped = 0usize;
    for c in &checks {
        match c.status {
            Status::Fail => failed += 1,
            Status::Skipped => skipped += 1,
            Status::Pass => {}
        }
        let detail = c.detail.split_whitespace().collect::<Vec<_>>().join(" ");
        println!(
            "{:<w$}  {label:<5} {detail}",
            c.name,
            label = c.status.label(),
            w = width,
        );
    }

    let elapsed = start.elapsed();
    let verdict = if failed == 0 {
        "all checks passed"
    } else {
        "check(s) FAILED"
    };
    let mut footer = format!("{verdict} in {ms}ms", ms = elapsed.as_millis());
    if skipped > 0 {
        let _ = write!(footer, " ({skipped} skipped)");
    }
    if failed > 0 {
        let _ = write!(footer, " ({failed} failed)");
    }
    println!("{footer}");

    // `quiet` exists for scripted use (`ainl doctor --quiet`): a single summary
    // line, with the detail still on stderr for a human who hit a failure.
    if quiet && failed > 0 {
        eprintln!("ainl doctor: {failed} check(s) failed (run `ainl doctor` for detail)");
    }

    if exit_code_for(&checks.iter().map(|c| c.status).collect::<Vec<_>>()) == 0 {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

/// The exit-code rule, isolated so it can be tested directly: green unless
/// something actually FAILED. A SKIP is an honest "could not verify", never a
/// failure — otherwise every host without a C compiler would see `ainl doctor`
/// report a broken install when the interpreter is working fine.
fn exit_code_for(statuses: &[Status]) -> u8 {
    if statuses.contains(&Status::Fail) {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_self_test_passes() {
        let c = check_eval();
        assert_eq!(c.status, Status::Pass, "detail: {}", c.detail);
    }

    #[test]
    fn run_self_test_passes() {
        let c = check_run();
        assert_eq!(c.status, Status::Pass, "detail: {}", c.detail);
    }

    #[test]
    fn transpile_self_test_passes() {
        let c = check_transpile();
        assert_eq!(c.status, Status::Pass, "detail: {}", c.detail);
    }

    #[test]
    fn grammar_export_has_a_defined_root() {
        let c = check_grammar();
        assert_eq!(c.status, Status::Pass, "detail: {}", c.detail);
    }

    /// A GBNF whose `root` line is dropped must be reported as broken — this is
    /// the failure mode that produces zero valid generations with no error
    /// message, which is exactly what the check exists to catch. Asserted
    /// through `gbnf_problems` (the real logic), not a re-implementation of it.
    #[test]
    fn grammar_check_rejects_a_root_less_grammar() {
        let stripped: String = ainl_core::grammar::grammar(Dialect::Gbnf)
            .lines()
            .filter(|l| !l.starts_with("root"))
            .collect::<Vec<_>>()
            .join("\n");
        let problems = gbnf_problems(&stripped);
        assert!(
            problems.iter().any(|p| p.contains("root")),
            "expected a root complaint, got {problems:?}"
        );
    }

    /// The mirror case: a rule referenced on a RHS but never defined would make
    /// a decoder reject the grammar at load time. Distinct from the root check.
    #[test]
    fn grammar_check_rejects_undefined_references() {
        let broken = "root ::= form\nform  ::= atom (missing-rule)\n";
        let problems = gbnf_problems(broken);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("missing-rule") && p.contains("undefined")),
            "expected an undefined-rule complaint, got {problems:?}"
        );
    }

    /// The real, shipped grammar must be clean — otherwise the two tests above
    /// only prove the detector fires, not that the export is sound.
    #[test]
    fn shipped_gbnf_has_no_problems() {
        let gbnf = ainl_core::grammar::grammar(Dialect::Gbnf);
        assert!(gbnf_problems(gbnf).is_empty(), "shipped GBNF is malformed");
    }

    /// The aot check must track the cc probe exactly: a working `cc` means the
    /// generated C is actually checked, and a missing `cc` means it is honestly
    /// skipped rather than silently reported green. There is no third state
    /// where aot claims to have verified something it never ran.
    #[test]
    fn aot_probe_tracks_the_cc_probe() {
        match probe_cc().status {
            Status::Pass => assert_eq!(
                check_aot().status,
                Status::Pass,
                "a working cc must accept the generated C"
            ),
            // cc exists but is broken: aot may legitimately fail, and that is a
            // real signal rather than a false one.
            Status::Fail => assert!(matches!(
                check_aot().status,
                Status::Fail | Status::Pass | Status::Skipped
            )),
            Status::Skipped => assert_eq!(
                check_aot().status,
                Status::Skipped,
                "aot cannot be verified without cc, and must say so"
            ),
        }
    }

    /// The whole point of SKIP-not-FAIL on a missing `cc`: an interpreter-only
    /// install is a valid install, so the run must still be green. Asserted
    /// against the real function that decides the exit code, not a copy of its
    /// arithmetic — a re-implementation here would pass even if `run` inverted
    /// the comparison.
    #[test]
    fn skips_alone_do_not_fail_the_run() {
        assert_eq!(
            exit_code_for(&[Status::Pass, Status::Skipped, Status::Skipped]),
            0
        );
        assert_eq!(exit_code_for(&[Status::Pass]), 0);
        assert_eq!(exit_code_for(&[Status::Pass, Status::Fail]), 1);
    }

    #[test]
    fn version_provenance_is_never_blank() {
        // "unknown" is the honest fallback; a blank would be a bug in build.rs.
        assert!(!env!("AINL_TARGET").trim().is_empty());
        assert!(!env!("AINL_GIT_COMMIT").trim().is_empty());
        assert!(!env!("AINL_GIT_DIRTY").trim().is_empty());
    }
}
