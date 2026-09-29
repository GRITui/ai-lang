//! What each backend does with an `import`.
//!
//! # The contract changed, and this file records why
//!
//! Until this tier, `import` was **interpreter-only**: the AOT backend refused
//! it because the generated `main` has no load phase, and the three
//! transpilers refused it because `import` is a *keyword* in Python, Ruby and
//! JavaScript — so an unhandled `(import "m")` would lower into a call to the
//! host's own import machinery, and the resulting program would compile
//! cleanly, link cleanly, and do something else entirely.
//!
//! The AOT half no longer applies, and the reason is worth writing down
//! because it is not "we got around to it":
//!
//! **`import` is load-time, and load-time is exactly what a compiler already
//! does.** A backend that inlines the resolved module graph into the emitted C
//! never needs a runtime load phase at all — the module's code *is* in the
//! binary, so the program cannot tell that its sources were deleted. That is
//! what makes `ainl pkg` safe: vendoring puts the source in the tree at build
//! time, and inlining puts it in the binary.
//!
//! So the AOT backend now resolves and inlines (`generate_program`), and the
//! three transpilers **still refuse**, because their problem was never
//! load-time resolution — it is that the target language has its own `import`
//! keyword with unrelated semantics, and a single-file emission has nowhere to
//! put a loader. That half of the original contract is unchanged, and the
//! cases below pin it.
//!
//! Two rules keep the inlined program equivalent to the interpreted one, and
//! both are asserted here rather than assumed:
//!
//! 1. a module is inlined **once**, so a diamond does not bind its defs twice;
//! 2. two modules exporting the same name is an **error**, matching the
//!    loader's collision rule rather than silently shadowing.

use ainl_core::parse;
use std::path::{Path, PathBuf};

/// A scratch project directory that cleans itself up.
///
/// The thread id is in the name because `cargo test` runs these in parallel:
/// two cases writing the same `/tmp/ainl-imp-x` would interleave, and one
/// would read the other's half-written fixture.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let base = std::env::temp_dir().join(format!(
            "ainl-imp-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("temp dir");
        Scratch(base)
    }

    fn write(&self, rel: &str, body: &str) -> PathBuf {
        let p = self.0.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).expect("dir");
        std::fs::write(&p, body).expect("write");
        p
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Compile `src` as if it lived at `path`, resolving its imports.
fn compile_at(path: &Path, src: &str) -> ainl_core::Result<String> {
    let forms = parse(src).expect("the fixture itself must parse");
    ainl_cc::generate_program(&forms, Some(path), Some(src))
}

// ---------------------------------------------------------------------------
// The AOT backend inlines
// ---------------------------------------------------------------------------

#[test]
fn aot_inlines_a_top_level_import() {
    let s = Scratch::new("top");
    s.write("lib/math.ainl", "(def answer 42)\n");
    let src = "(import \"lib/math.ainl\")\n(print answer)\n";
    let main = s.write("main.ainl", src);
    let c = compile_at(&main, src).expect("compiles");
    assert!(
        c.contains("answer"),
        "the imported def must be emitted into the C"
    );
}

/// The load-bearing property: **no runtime file lookup**. A program that read
/// its module at run time would be a wrapper, not a standalone binary — the
/// exact failure the vendoring design exists to prevent.
#[test]
fn the_inlined_program_never_opens_a_source_file_at_run_time() {
    let s = Scratch::new("standalone");
    s.write("lib/math.ainl", "(def answer 42)\n");
    let src = "(import \"lib/math.ainl\")\n(print answer)\n";
    let main = s.write("main.ainl", src);
    let c = compile_at(&main, src).expect("compiles");
    // The specifier must not survive into the binary: if it did, something
    // would be looking it up.
    assert!(
        !c.contains("lib/math.ainl"),
        "the module specifier must not appear in the emitted program — \
         nothing at run time can read a source file"
    );
}

#[test]
fn aot_inlines_a_namespaced_import() {
    // `(import "m" as ns)` binds one map, not the module's exports. The
    // inlined form has to build that map, or the alias reads as nil at run
    // time — a program that compiles and then fails, which is worse than one
    // that never compiled.
    let s = Scratch::new("ns");
    s.write("m.ainl", "(def one 1)\n");
    let src = "(import \"m\" as ns)\n(print (get ns \"one\"))\n";
    let main = s.write("main.ainl", src);
    let c = compile_at(&main, src).expect("a namespaced import compiles");
    assert!(
        !c.contains("(import"),
        "the directive must be stripped, not emitted as data"
    );
}

/// The diamond: `a` imports `c`, and the entry imports both `a` and `c`. Both
/// routes are the same canonical file, so `c` must be inlined **once**.
///
/// Twice would bind every one of its defs twice, and the second binding would
/// shadow the first — the loader's collision rule firing on a program that is
/// perfectly legal under the interpreter. This is the case where "inlining is
/// easy" and "inlining is *correct*" come apart, so it is asserted rather than
/// assumed.
#[test]
fn a_module_reached_by_two_routes_is_inlined_once() {
    let s = Scratch::new("diamond");
    s.write("c.ainl", "(def shared 1)\n");
    s.write("a.ainl", "(import \"c.ainl\")\n(def av (fn () shared))\n");
    let src = "(import \"a.ainl\")\n(import \"c.ainl\")\n(print (av))\n";
    let main = s.write("main.ainl", src);
    // A duplicate would be refused as a collision, so *compiling at all* is
    // the primary assertion: the inliner folds the diamond by canonical path.
    let c = compile_at(&main, src).expect("a diamond inlines once, not twice");
    assert!(c.contains("shared"), "the shared def must be present");
}

/// A module that calls an import **at load time** must be emitted after it.
///
/// This is the case that separates "the graph resolved" from "the graph is in
/// an order a flat concatenation can execute". `b.ainl` below ends with a
/// top-level `(def doubled (twice 21))`, which calls `a`'s `twice` while the
/// module body runs. Resolution cannot emit `b` first — it reserves a module's
/// slot before recursing so a diamond reuses one index — so the emitted order
/// has to be fixed up afterwards, and until it is, the flattened program calls
/// a function that is still nil.
///
/// The interpreter never sees this: it *evaluates* each module as it
/// finishes, so its order is depth-first by construction. Only the
/// concatenated form, which is what AOT emits, was ever at risk.
#[test]
fn a_module_calling_an_import_at_load_time_is_emitted_after_it() {
    let s = Scratch::new("loadorder");
    s.write("a.ainl", "(def twice (fn (x) (* x 2)))\n");
    s.write("b.ainl", "(import \"a.ainl\")\n(def doubled (twice 21))\n");
    let src = "(import \"a.ainl\")\n(import \"b.ainl\")\n(print doubled)\n";
    let main = s.write("main.ainl", src);
    let c = compile_at(&main, src).expect("a load-time cross-module call compiles");
    // `doubled` is defined *after* `twice` in the emitted C. If the order were
    // wrong, `twice` would be undeclared at that point and the C would not
    // build — or would bind nil and fail at run time.
    let i_twice = c.find("\"twice\"").expect("twice is emitted");
    let i_doubled = c.find("\"doubled\"").expect("doubled is emitted");
    assert!(
        i_twice < i_doubled,
        "the dependency must be emitted before the module that calls it \
         (twice at {i_twice}, doubled at {i_doubled})"
    );
}

/// `(import "m" as ns)` binds a *value* — a map from the module's exports to
/// their contents — and flattening the graph cannot reconstruct that for free.
///
/// This is the one import form the inliner has to actively rebuild rather than
/// merely satisfy: the module's `def`s land in the program either way, but
/// nothing in a flat concatenation constructs the namespace map. The bug it
/// guards against is the worst shape — the directive is stripped, the program
/// compiles cleanly, every static check passes, and then it dies at run time
/// with `unbound symbol 'ns'`.
#[test]
fn a_namespaced_import_still_binds_its_map_after_inlining() {
    let s = Scratch::new("nsbind");
    s.write("m.ainl", "(def one 1)\n(def two 2)\n");
    let src = "(import \"m.ainl\" as ns)\n(print ((get ns \"one\")))\n";
    let main = s.write("main.ainl", src);
    let c = compile_at(&main, src).expect("a namespaced import compiles");
    assert!(
        !c.contains("(import"),
        "the directive must be stripped, not emitted as data"
    );
    // The alias has to be DEFINED, not merely referenced — the reference alone
    // is what the interpreter-only version of this bug looked like.
    assert!(
        c.contains("\"ns\""),
        "the alias must be defined as a name in the emitted program"
    );
}

/// A *nested* module's namespaced import has to work too. Its alias is bound in
/// that module's scope, which flattening destroys along with the scope itself,
/// so it is the case a fix that only handled the entry file would miss.
#[test]
fn a_nested_module_can_import_its_own_dependency_namespaced() {
    let s = Scratch::new("nsnested");
    s.write("leaf.ainl", "(def v 7)\n");
    s.write(
        "mid.ainl",
        "(import \"leaf.ainl\" as l)\n(def w ((get l \"v\")))\n",
    );
    let src = "(import \"mid.ainl\")\n(print w)\n";
    let main = s.write("main.ainl", src);
    compile_at(&main, src).expect("a nested namespaced import compiles");
}

#[test]
fn two_modules_exporting_the_same_name_is_refused() {
    // Inlining flattens both modules into one namespace. The interpreter
    // refuses this at load time; codegen has no such error, so it is checked
    // here — otherwise the second `def` silently wins, which is the
    // silent-wrong the loader's collision rule exists to prevent.
    let s = Scratch::new("collide");
    s.write("a.ainl", "(def shared 1)\n");
    s.write("b.ainl", "(def shared 2)\n");
    let src = "(import \"a.ainl\")\n(import \"b.ainl\")\n(print shared)\n";
    let main = s.write("main.ainl", src);
    let err = compile_at(&main, src).expect_err("a name collision is refused");
    assert!(
        err.message().contains("already defined"),
        "the refusal must name the collision, got: {err}"
    );
    assert!(
        err.message().contains("shared"),
        "the refusal must name the NAME that collided, got: {err}"
    );
}

#[test]
fn a_name_redefined_inside_one_module_is_not_a_collision() {
    // The rule is about *cross-module* collisions. Two `def`s of one name in a
    // single file is ordinary AINL — the last one wins — and refusing it here
    // would make the AOT backend reject programs the interpreter runs.
    let s = Scratch::new("redef");
    s.write("m.ainl", "(def x 1)\n(def x 2)\n");
    let src = "(import \"m.ainl\")\n(print x)\n";
    let main = s.write("main.ainl", src);
    compile_at(&main, src).expect("a redefinition inside one module is legal");
}

#[test]
fn a_missing_module_is_refused_with_the_candidates_it_tried() {
    let s = Scratch::new("missing");
    let src = "(import \"nope.ainl\")\n";
    let main = s.write("main.ainl", src);
    let err = compile_at(&main, src).expect_err("a missing module is refused");
    assert!(err.message().contains("cannot find module"), "got: {err}");
    assert!(
        err.message().contains("Tried:"),
        "the refusal must list what it tried, got: {err}"
    );
}

#[test]
fn a_circular_import_is_refused_naming_the_cycle() {
    let s = Scratch::new("cycle");
    s.write("a.ainl", "(import \"b.ainl\")\n");
    s.write("b.ainl", "(import \"a.ainl\")\n");
    let src = "(import \"a.ainl\")\n";
    let main = s.write("main.ainl", src);
    let err = compile_at(&main, src).expect_err("a cycle is refused");
    let msg = err.message();
    assert!(msg.contains("circular import"), "got: {msg}");
    assert!(
        msg.contains("a.ainl") && msg.contains("b.ainl"),
        "got: {msg}"
    );
}

/// A nested import is illegal in the language itself — the interpreter refuses
/// it for being at the wrong level — and every backend must agree.
#[test]
fn a_nested_import_is_still_refused() {
    let s = Scratch::new("nested");
    s.write("m.ainl", "(def x 1)\n");
    let src = "(def f (fn () (import \"m.ainl\")))\n";
    let main = s.write("main.ainl", src);
    let err = compile_at(&main, src).expect_err("a nested import is refused");
    assert!(
        err.message().contains("top level") || err.message().contains("interpreter-only"),
        "got: {err}"
    );
}

/// Quoted data is data. A program that *talks about* an import is an ordinary
/// program and must still compile. Refusing it would make `generate` unable to
/// describe a language feature.
#[test]
fn aot_allows_a_quoted_import() {
    let forms = parse(r#"(print (quote (import "m.ainl")))"#).expect("parses");
    let c = ainl_cc::generate(&forms).expect("quoted import is data, not a directive");
    assert!(
        c.contains("import"),
        "the generated C should contain the quoted text"
    );
}

// ---------------------------------------------------------------------------
// Transpilers: still refused
// ---------------------------------------------------------------------------
//
// The transpilers' problem was never load-time resolution — it is that
// `import` is a keyword in the target language. `Target` is an enum, so this
// list is the only place the coverage is written down: a backend added to the
// enum has to be added here too, or the loops quietly stop covering it.

const HOSTS: &[ainl_transpile::Target] = &[
    ainl_transpile::Target::Python,
    ainl_transpile::Target::JavaScript,
    ainl_transpile::Target::Ruby,
];

/// Transpile with an explicit target, so a failure names the backend rather
/// than reporting three identical errors.
fn transpiles(src: &str, target: ainl_transpile::Target) -> ainl_core::Result<String> {
    let forms = parse(src).expect("parses");
    ainl_transpile::transpile(target, &forms, src)
}

#[test]
fn every_transpiler_still_refuses_an_import() {
    for &t in HOSTS {
        let msg = transpiles(r#"(import "lib/math.ainl")"#, t)
            .expect_err("must refuse an import")
            .to_string();
        assert!(
            msg.contains("interpreter-only"),
            "{}: refusal must be explicit, got: {msg}",
            t.label()
        );
    }
}

#[test]
fn every_transpiler_still_refuses_a_namespaced_import() {
    for &t in HOSTS {
        let msg = transpiles(r#"(import "lib/math.ainl" as m)"#, t)
            .expect_err("must refuse a namespaced import")
            .to_string();
        assert!(
            msg.contains("interpreter-only"),
            "{}: got: {msg}",
            t.label()
        );
    }
}

#[test]
fn every_transpiler_still_refuses_a_nested_import() {
    for &t in HOSTS {
        let msg = transpiles(r#"(def f (fn () (import "m.ainl")))"#, t)
            .expect_err("must refuse a nested import")
            .to_string();
        assert!(
            msg.contains("interpreter-only"),
            "{}: got: {msg}",
            t.label()
        );
    }
}

/// A program with no `import` must be completely unaffected.
#[test]
fn every_transpiler_is_untouched_without_an_import() {
    let src = r#"(def f (fn (x) (* x 2)))
(print (f 21))"#;
    for &t in HOSTS {
        let out = transpiles(src, t).expect("a program with no import is unaffected");
        // Sanity: it really did generate something host-shaped, so this test
        // cannot pass by returning an empty string on both sides.
        assert!(!out.trim().is_empty(), "{} produced nothing", t.label());
    }
    let forms = parse(src).expect("parses");
    assert!(ainl_cc::generate(&forms).is_ok(), "AOT unaffected");
}

/// `generate` — the single-file entry point, called with a bare AST and no
/// path — still refuses. There is no honest way to guess which directory a
/// relative specifier is relative to, and guessing would resolve the same
/// source differently in two builds.
#[test]
fn the_pathless_entry_point_still_refuses_an_import() {
    let forms = parse(r#"(import "m.ainl")"#).expect("parses");
    let err = ainl_cc::generate(&forms).expect_err("no path, no way to resolve");
    assert!(err.message().contains("interpreter-only"), "got: {err}");
}

/// `()` is legal AINL — an empty `fn` parameter list is the everyday case —
/// and every backend's scan walks subforms. An empty list has no head to skip,
/// so slicing the head off it panics. That crash is reachable from ordinary
/// source, so it is pinned here rather than left to chance.
#[test]
fn an_empty_parameter_list_does_not_crash_the_scan() {
    for src in [
        "(def f (fn () 1))",
        "(def f (fn () (import \"m.ainl\")))",
        "()",
        "(list ())",
    ] {
        let forms = parse(src).expect("parses");
        let _ = ainl_cc::generate(&forms);
        for &t in HOSTS {
            let _ = transpiles(src, t);
        }
    }
}
