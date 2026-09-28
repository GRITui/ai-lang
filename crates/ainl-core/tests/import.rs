//! `import` — file-based module composition.
//!
//! These tests drive the real loader against real files on disk in a temp
//! directory, because the feature is *about* locations: a program's meaning
//! depends on where it and its imports live. A resolver tested only against an
//! in-memory fixture table would be testing the wrong thing.
//!
//! Every case is asserted twice — once through the bytecode VM and once through
//! the tree-walking evaluator — because the loader runs module bodies on
//! whichever interpreter the caller selected. A loader that worked on one and
//! not the other would leave a program that runs or fails depending on which
//! path reached it, which is exactly the class of bug the two interpreters are
//! kept in lockstep to prevent.

use ainl_core::eval::Env;
use ainl_core::import::{self, Executor, Loader};
use ainl_core::parser::Node;
use ainl_core::Value;
use std::path::{Path, PathBuf};

/// A temp directory holding one test's module tree, removed on drop.
struct Tree {
    root: PathBuf,
}

impl Tree {
    /// A fresh tree named after the calling test, so a failing run leaves a
    /// readable directory instead of a mystery in a shared temp dir.
    fn new(name: &str) -> Tree {
        let root = std::env::temp_dir().join(format!("ainl-import-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create test tree");
        Tree { root }
    }

    /// Write a file, creating parent directories as needed. Returns its path.
    fn write(&self, rel: &str, src: &str) -> PathBuf {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        std::fs::write(&path, src).expect("write module");
        path
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.root.join(rel)).expect("read file")
    }

    fn dir(&self) -> &Path {
        &self.root
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Run a program file on the VM, returning the final value.
fn run_vm(src: &str, path: &Path) -> ainl_core::Result<Value> {
    ainl_core::run_named_in(src, path, &Env::with_prelude())
}

/// Run the same program on the tree-walk, returning the final value.
///
/// A comparison helper rather than a test on its own: every test that runs a
/// program also proves the two interpreters agree about modules, which is a
/// property of the feature that no single-backend test can show.
fn run_both(src: &str, path: &Path) -> (ainl_core::Result<Value>, ainl_core::Result<Value>) {
    (
        run_vm(src, path),
        ainl_core::run_named_tree_walk_in(src, path, &Env::with_prelude()),
    )
}

/// Assert both interpreters succeed and agree on the result.
#[track_caller]
fn assert_both(src: &str, path: &Path, expected: &str) {
    let (vm, tw) = run_both(src, path);
    let vm = vm.unwrap_or_else(|e| panic!("VM failed: {e}"));
    let tw = tw.unwrap_or_else(|e| panic!("tree-walk failed: {e}"));
    assert_eq!(vm.to_string(), expected, "VM value mismatch");
    assert_eq!(
        tw.to_string(),
        expected,
        "the two interpreters disagree about a program with imports"
    );
}

/// Assert both interpreters fail, and that the message contains `needle`.
///
/// The message is checked on both: a loader that only reports its errors on one
/// interpreter is a loader whose errors cannot be trusted by a caller that
/// picked the other.
#[track_caller]
fn assert_both_err(src: &str, path: &Path, needle: &str) {
    let (vm, tw) = run_both(src, path);
    let vm_err = vm.expect_err("VM unexpectedly succeeded").to_string();
    let tw_err = tw
        .expect_err("tree-walk unexpectedly succeeded")
        .to_string();
    assert!(
        vm_err.contains(needle),
        "VM error lacks {needle:?}: {vm_err}"
    );
    assert!(
        tw_err.contains(needle),
        "tree-walk error lacks {needle:?}: {tw_err}"
    );
}

// ---------------------------------------------------------------------------
// The two import forms
// ---------------------------------------------------------------------------

#[test]
fn a_flat_import_binds_every_top_level_def() {
    let t = Tree::new("flat");
    t.write(
        "lib/math.ainl",
        "(def square (fn (n) (* n n)))\n(def PI 3)\n",
    );
    let p = t.write(
        "main.ainl",
        r#"
(import "lib/math.ainl")
(list (square 7) PI)
"#,
    );
    assert_both(&t.read("main.ainl"), &p, "(49 3)");
}

#[test]
fn a_namespaced_import_binds_exactly_one_name() {
    let t = Tree::new("ns");
    t.write(
        "lib/math.ainl",
        "(def square (fn (n) (* n n)))\n(def PI 3)\n",
    );
    let p = t.write(
        "main.ainl",
        r#"
(import "lib/math.ainl" as m)
(list ((get m "square") 7) (has m "square") (has m "nosuch"))
"#,
    );
    assert_both(&t.read("main.ainl"), &p, "(49 true false)");
}

#[test]
fn a_namespaced_import_is_reachable_through_the_ordinary_map_builtins() {
    // The namespace is a plain AINL map, so it needs no access syntax of its
    // own — the whole point of choosing a map over a new lookup form.
    let t = Tree::new("ns-map");
    t.write("m.ainl", "(def a 1)\n(def b 2)\n");
    let p = t.write(
        "main.ainl",
        r#"
(import "m" as ns)
(list (keys ns) (len (keys ns)) (get ns "a") (get ns "b"))
"#,
    );
    assert_both(&t.read("main.ainl"), &p, r#"(("a" "b") 2 1 2)"#);
}

#[test]
fn a_namespaced_import_does_not_pollute_the_importer_scope() {
    let t = Tree::new("ns-isolated");
    t.write("m.ainl", "(def secret 1)\n");
    let p = t.write(
        "main.ainl",
        r#"
(import "m" as ns)
secret
"#,
    );
    // `secret` is only reachable through `ns`; the bare name is unbound.
    assert_both_err(&t.read("main.ainl"), &p, "unbound symbol 'secret'");
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

#[test]
fn a_path_like_specifier_resolves_against_the_importing_files_directory() {
    // Run from an unrelated working directory (the test process's cwd, which is
    // the crate root, not the temp tree) — so only the importer-relative
    // candidate can resolve. This is the property that makes a program's
    // location part of its meaning, and it is the reason `ainl run` hands the
    // file's path down to the loader instead of calling `run_str`.
    let t = Tree::new("relpath");
    t.write("lib/math.ainl", "(def v 42)\n");
    let p = t.write("sub/main.ainl", "(import \"../lib/math.ainl\")\nv\n");
    assert_both(&t.read("sub/main.ainl"), &p, "42");
}

#[test]
fn a_bare_name_falls_back_to_the_importing_files_directory() {
    // A bare name is tried against the working directory first, then the
    // importer's own directory. The module lives only next to the importer, so
    // this exercises the second candidate.
    //
    // The cwd is pinned to a directory where `helper.ainl` cannot resolve, so
    // the assertion is about the fallback rather than about wherever the test
    // process happened to start. It also takes the cwd lock, because the
    // process working directory is global and sibling tests change it.
    let t = Tree::new("bare-fallback");
    t.write("sub/helper.ainl", "(def v 7)\n");
    t.write("sub/main.ainl", "(import \"helper\")\nv\n");
    let restore = Cwd::set(t.dir().join("sub").parent().unwrap().to_path_buf());
    let result = run_both(&t.read("sub/main.ainl"), &t.dir().join("sub/main.ainl"));
    drop(restore);
    assert_eq!(
        result.0.expect("VM").to_string(),
        "7",
        "the importer-relative candidate must resolve"
    );
    assert_eq!(result.1.expect("tree-walk").to_string(), "7");
}

#[test]
fn a_bare_name_prefers_the_working_directory_over_the_importers_directory() {
    // The card's stated order: working directory first, importer's directory
    // second. The two candidates are made to disagree, and the test changes the
    // *process* working directory to the directory holding the cwd candidate —
    // an in-process test cannot rely on the ambient cwd, and asserting the
    // preference is the whole point of the rule.
    let t = Tree::new("bare-order");
    t.write("helper.ainl", "(def which \"cwd\")\n");
    t.write("sub/helper.ainl", "(def which \"importer\")\n");
    let p = t.write("sub/main.ainl", "(import \"helper\")\nwhich\n");

    let restore = Cwd::set(t.dir().to_path_buf());
    assert_both(&t.read("sub/main.ainl"), &p, "cwd");
    drop(restore);
    // With the cwd elsewhere the importer's candidate is the one that resolves.
    let restore = Cwd::set(std::env::temp_dir());
    assert_both(&t.read("sub/main.ainl"), &p, "importer");
    drop(restore);
}

/// Switches the process working directory, restoring it on drop.
///
/// The cwd is process-global, so a test that changes it would otherwise race
/// every sibling test in this file. The guard is carried as a field purely to
/// hold the lock for the guard's lifetime, which is why the field is named
/// `_lock` and the type is a dedicated newtype: it holds no data to read, only
/// the lock, and dropping `Cwd` drops it.
struct Cwd {
    previous: PathBuf,
    _lock: CwdLock,
}

static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A held lock, kept alive only for its `Drop`. It exposes nothing on purpose.
struct CwdLock(
    #[expect(dead_code, reason = "held for its Drop, never read")]
    std::sync::MutexGuard<'static, ()>,
);

impl Cwd {
    fn set(dir: PathBuf) -> Cwd {
        let lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::current_dir().expect("current dir");
        std::env::set_current_dir(&dir).expect("set current dir");
        Cwd {
            previous,
            _lock: CwdLock(lock),
        }
    }
}

impl Drop for Cwd {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.previous);
    }
}

#[test]
fn a_path_like_specifier_prefers_the_importers_directory_over_the_working_directory() {
    // The mirror of the bare-name rule, and the one that actually bites: a
    // module in a subdirectory asking for a *sibling by path* means "next to
    // me", even when a same-named file also sits in the working directory.
    // Both candidates exist and disagree, so this asserts the preference rather
    // than merely that one of them resolved.
    //
    // The specifier must actually contain a separator: a bare name resolves
    // working-directory-first by design, so reusing a bare name here would be
    // testing the other rule with the other rule's expectation.
    let t = Tree::new("pathlike-order");
    t.write("sub/dep/shared.ainl", "(def which \"importer\")\n");
    t.write("shared.ainl", "(def which \"cwd\")\n");
    let p = t.write("sub/dep/main.ainl", "(import \"./shared\")\nwhich\n");

    // The cwd holds a decoy `shared.ainl`; the importer's must win.
    let restore = Cwd::set(t.dir().to_path_buf());
    assert_both(&t.read("sub/dep/main.ainl"), &p, "importer");
    drop(restore);
}

#[test]
fn an_extensionless_specifier_gets_dot_ainl() {
    let t = Tree::new("no-ext");
    t.write("m.ainl", "(def v 1)\n");
    let p = t.write("main.ainl", "(import \"m\")\nv\n");
    assert_both(&t.read("main.ainl"), &p, "1");
}

#[test]
fn an_explicit_extension_is_respected_as_written() {
    // A specifier that names its own extension must not be second-guessed, or
    // `(import "notes.txt")` would silently read `notes.txt.ainl`.
    let t = Tree::new("ext-kept");
    t.write("data.txt", "(def v 99)\n");
    let p = t.write("main.ainl", "(import \"data.txt\")\nv\n");
    assert_both(&t.read("main.ainl"), &p, "99");
}

#[test]
fn an_absolute_path_is_used_as_is() {
    let t = Tree::new("abs");
    let module = t.write("m.ainl", "(def v 5)\n");
    let p = t.write(
        "main.ainl",
        &format!("(import \"{}\")\nv\n", module.display()),
    );
    assert_both(&t.read("main.ainl"), &p, "5");
}

#[test]
fn a_missing_module_lists_every_candidate_it_tried() {
    // The error is the documentation: a user who cannot resolve a module must
    // be able to fix it from the message without reading the resolver.
    let t = Tree::new("missing");
    let p = t.write("sub/main.ainl", "(import \"nope\")\n");
    let err = run_vm(&t.read("sub/main.ainl"), &p)
        .expect_err("missing module must fail")
        .to_string();
    assert!(err.contains("cannot find module 'nope'"), "{err}");
    assert!(err.contains("Tried:"), "{err}");
    // Both bases must be named: the one that failed and the one that would
    // have worked had the file been there.
    assert!(err.contains("working directory"), "{err}");
    assert!(err.contains("sub"), "{err}");
}

// ---------------------------------------------------------------------------
// The collision rule
// ---------------------------------------------------------------------------

#[test]
fn a_flat_import_may_not_take_a_name_the_file_defines_itself() {
    let t = Tree::new("collide-def");
    t.write("m.ainl", "(def v 1)\n");
    let p = t.write("main.ainl", "(import \"m\")\n(def v 2)\n");
    assert_both_err(&t.read("main.ainl"), &p, "already defined");
}

#[test]
fn a_def_after_the_import_still_collides() {
    // The program's own `def` names are reserved before any import resolves, so
    // the collision is found regardless of textual order — the same
    // forward-reference allowance the compiler already makes for `def`s.
    let t = Tree::new("collide-order");
    t.write("m.ainl", "(def v 1)\n");
    let p = t.write("main.ainl", "(def v 2)\n(import \"m\")\n");
    assert_both_err(&t.read("main.ainl"), &p, "already defined");
}

#[test]
fn a_flat_import_may_not_shadow_a_builtin() {
    // A module exporting `len` would otherwise make every later `len` call mean
    // something other than what it looks like.
    let t = Tree::new("collide-builtin");
    t.write("m.ainl", "(def len (fn () 1))\n");
    let p = t.write("main.ainl", "(import \"m\")\n(len (list 1 2 3))\n");
    let err = run_vm(&t.read("main.ainl"), &p)
        .expect_err("must fail")
        .to_string();
    assert!(err.contains("already defined"), "{err}");
    assert!(err.contains("prelude"), "the origin should be named: {err}");
}

#[test]
fn two_modules_exporting_the_same_name_collide() {
    let t = Tree::new("collide-two");
    t.write("a.ainl", "(def same 1)\n");
    t.write("b.ainl", "(def same 2)\n");
    let p = t.write("main.ainl", "(import \"a\")\n(import \"b\")\n");
    assert_both_err(&t.read("main.ainl"), &p, "'same' is already defined");
}

#[test]
fn a_collision_leaves_nothing_partially_applied() {
    // The error is raised before *any* of the second module's names is defined,
    // so a failure never leaves a half-imported scope behind.
    let t = Tree::new("collide-atomic");
    t.write("a.ainl", "(def first 1)\n(def same 2)\n");
    t.write("b.ainl", "(def same 3)\n");
    let p = t.write("main.ainl", "(import \"a\")\n(import \"b\")\n");
    assert_both_err(&t.read("main.ainl"), &p, "already defined");
}

#[test]
fn the_as_form_is_the_documented_escape_from_a_collision() {
    // The error names `as`; this proves the advice actually works.
    let t = Tree::new("collide-escape");
    t.write("m.ainl", "(def v 10)\n");
    let p = t.write(
        "main.ainl",
        r#"
(import "m" as helper)
(def v 20)
(+ v (get helper "v"))
"#,
    );
    assert_both(&t.read("main.ainl"), &p, "30");
}

#[test]
fn a_namespaced_alias_may_not_shadow_a_builtin_either() {
    let t = Tree::new("collide-alias");
    t.write("m.ainl", "(def v 1)\n");
    let p = t.write("main.ainl", "(import \"m\" as len)\n");
    assert_both_err(&t.read("main.ainl"), &p, "already defined");
}

// ---------------------------------------------------------------------------
// Module structure
// ---------------------------------------------------------------------------

#[test]
fn a_module_may_import_another_module() {
    let t = Tree::new("transitive");
    t.write("base.ainl", "(def base 10)\n");
    t.write("mid.ainl", "(import \"base\")\n(def mid (+ base 5))\n");
    let p = t.write("main.ainl", "(import \"mid\")\nmid\n");
    assert_both(&t.read("main.ainl"), &p, "15");
}

#[test]
fn a_module_does_not_re_export_what_it_imported() {
    // The export rule is "the names this file `def`s", not "everything in its
    // scope" — otherwise a module's public surface would silently depend on its
    // own dependencies, and `base` would leak through `mid`.
    let t = Tree::new("no-reexport");
    t.write("base.ainl", "(def base 10)\n");
    t.write("mid.ainl", "(import \"base\")\n(def mid 1)\n");
    let p = t.write("main.ainl", "(import \"mid\")\nbase\n");
    assert_both_err(&t.read("main.ainl"), &p, "unbound symbol 'base'");
}

#[test]
fn a_module_cannot_see_the_importers_names() {
    let t = Tree::new("no-reverse-visibility");
    t.write("m.ainl", "(def v shared)\n");
    let p = t.write("main.ainl", "(def shared 1)\n(import \"m\")\nv\n");
    // The module is evaluated in isolation, so `shared` is unbound there. The
    // failure is raised while loading the module, which is what makes it an
    // isolation bug rather than a missing binding in the importer.
    assert_both_err(&t.read("main.ainl"), &p, "unbound symbol 'shared'");
}

#[test]
fn a_diamond_evaluates_the_shared_module_exactly_once() {
    // Cached by canonical path, so a module reached by two routes is read and
    // evaluated once. Asserted through a side effect (a file append) rather
    // than through a value, because a value would look the same either way.
    //
    // `run_vm` alone, not `assert_both`: the two interpreters are two separate
    // *runs*, each with its own loader and therefore its own module cache, so
    // the side effect correctly happens once per run and twice over the pair.
    // What this asserts is that a single run evaluates the shared module once —
    // which is the property a program's author actually depends on.
    let t = Tree::new("diamond");
    let marker = t.dir().join("loads.txt");
    let marker_path = marker.display().to_string();
    t.write(
        "c.ainl",
        &format!("(def once 0)\n(append-file \"{marker_path}\" \"x\")\n"),
    );
    t.write("a.ainl", "(import \"c\")\n(def fromA 1)\n");
    t.write("b.ainl", "(import \"c\")\n(def fromB 1)\n");
    let p = t.write(
        "main.ainl",
        "(import \"a\")\n(import \"b\")\n(list fromA fromB)\n",
    );
    let value = run_vm(&t.read("main.ainl"), &p).expect("run");
    assert_eq!(value.to_string(), "(1 1)");
    let loads = std::fs::read(&marker).expect("marker file");
    assert_eq!(
        loads.len(),
        1,
        "the shared module ran {} times in one run, want exactly 1",
        loads.len()
    );
}

#[test]
fn a_direct_import_is_not_a_second_collision_with_a_transitive_one() {
    // main imports both `a` and `base`; `a` also imports `base`. `base`'s names
    // are bound once into main's scope, so the second import is a cache hit
    // rather than a collision.
    let t = Tree::new("diamond-names");
    t.write("base.ainl", "(def base 10)\n");
    t.write("a.ainl", "(import \"base\")\n(def fromA 1)\n");
    let p = t.write(
        "main.ainl",
        "(import \"a\")\n(import \"base\")\n(+ base fromA)\n",
    );
    assert_both(&t.read("main.ainl"), &p, "11");
}

#[test]
fn the_same_file_reached_by_two_routes_is_one_module() {
    // `lib/m.ainl` and `sub/../lib/m.ainl` are the same file, so importing both
    // must not be a collision. This is the canonicalization contract.
    let t = Tree::new("same-file");
    t.write("lib/m.ainl", "(def v 1)\n");
    let p = t.write(
        "sub/main.ainl",
        "(import \"../lib/m.ainl\")\n(import \"../lib/m\")\nv\n",
    );
    assert_both(&t.read("sub/main.ainl"), &p, "1");
}

#[test]
fn a_cycle_is_reported_with_its_chain() {
    let t = Tree::new("cycle");
    t.write("a.ainl", "(import \"b\")\n");
    t.write("b.ainl", "(import \"a\")\n");
    let p = t.write("main.ainl", "(import \"a\")\n");
    let err = run_vm(&t.read("main.ainl"), &p)
        .expect_err("cycle must fail")
        .to_string();
    assert!(err.contains("circular import"), "{err}");
    assert!(err.contains("a.ainl"), "{err}");
    assert!(err.contains("b.ainl"), "{err}");
}

#[test]
fn a_self_import_is_a_cycle() {
    let t = Tree::new("self-cycle");
    t.write("a.ainl", "(import \"a\")\n");
    let p = t.write("main.ainl", "(import \"a\")\n");
    assert_both_err(&t.read("main.ainl"), &p, "circular import");
}

#[test]
fn a_failed_import_does_not_poison_a_later_unrelated_one() {
    // The cycle stack is popped on the error path. If it were not, a failed
    // import would leave a phantom entry and a later import of the same file
    // would look cyclic when it is not.
    let t = Tree::new("poison");
    t.write("bad.ainl", "(nosuchfunction)\n");
    t.write("good.ainl", "(def v 1)\n");
    let p = t.write("main.ainl", "(import \"bad\")\n(import \"good\")\n");
    let err = run_vm(&t.read("main.ainl"), &p)
        .expect_err("the bad module must fail")
        .to_string();
    assert!(err.contains("nosuchfunction"), "{err}");

    // A second, independent program importing the same pair behaves the same.
    let p2 = t.write("main2.ainl", "(import \"bad\")\n(import \"good\")\n");
    let err2 = run_vm(&t.read("main2.ainl"), &p2)
        .expect_err("the bad module must fail again")
        .to_string();
    assert!(err2.contains("nosuchfunction"), "{err2}");
    assert!(!err2.contains("circular"), "not a cycle: {err2}");
}

// ---------------------------------------------------------------------------
// Rejected shapes
// ---------------------------------------------------------------------------

#[test]
fn an_import_inside_a_function_is_rejected() {
    // A nested import would bind names at an arbitrary point during a call,
    // which is the dynamic semantics this design deliberately does not have.
    let t = Tree::new("nested-fn");
    t.write("m.ainl", "(def v 1)\n");
    let p = t.write("main.ainl", "(def f (fn () (import \"m\")))\n");
    assert_both_err(&t.read("main.ainl"), &p, "only allowed at the top level");
}

#[test]
fn an_import_inside_a_do_block_is_rejected() {
    let t = Tree::new("nested-do");
    t.write("m.ainl", "(def v 1)\n");
    let p = t.write("main.ainl", "(do (import \"m\"))\n");
    assert_both_err(&t.read("main.ainl"), &p, "only allowed at the top level");
}

#[test]
fn an_import_inside_a_let_is_rejected() {
    let t = Tree::new("nested-let");
    t.write("m.ainl", "(def v 1)\n");
    let p = t.write("main.ainl", "(let () (import \"m\"))\n");
    assert_both_err(&t.read("main.ainl"), &p, "only allowed at the top level");
}

#[test]
fn an_import_under_quote_is_data_and_not_a_directive() {
    // `quote` never evaluates its argument, so an `import` there is a symbol in
    // a list. Rejecting it would be wrong — it is legal data.
    let t = Tree::new("quoted");
    let p = t.write("main.ainl", "(quote (import \"m.ainl\"))\n");
    assert_both(&t.read("main.ainl"), &p, "(import \"m.ainl\")");
}

#[test]
fn a_malformed_directive_is_reported_as_such() {
    // Without a dedicated check these would all be "unbound symbol 'import'",
    // which tells a user nothing about the mistake they made in the right form.
    let cases = [
        ("(import)\n", "expected a quoted path string"),
        ("(import \"\")\n", "the path is empty"),
        ("(import m)\n", "expected a quoted path string"),
        ("(import \"m\" as)\n", "`as` must be followed"),
        ("(import \"m\" as 1)\n", "`as` must be followed"),
        ("(import \"m\" as a b)\n", "`as` must be followed"),
        ("(import \"m\" of x)\n", "expected `as`"),
    ];
    let t = Tree::new("malformed");
    for (i, (src, needle)) in cases.iter().enumerate() {
        let p = t.write(&format!("bad{i}.ainl"), src);
        assert_both_err(src, &p, needle);
    }
}

// ---------------------------------------------------------------------------
// Resolution internals
// ---------------------------------------------------------------------------

#[test]
fn the_candidate_order_is_exactly_as_documented() {
    // Pinned directly rather than inferred from behavior, because the order is
    // part of the contract a user writes paths against.
    let d = Path::new("/proj/lib");
    let names = |v: Vec<(PathBuf, String)>| -> Vec<String> {
        v.into_iter()
            .map(|(p, _)| p.display().to_string())
            .collect()
    };

    // A bare name: working directory first, then the importer's directory.
    assert_eq!(
        names(import::candidates("math", d)),
        vec!["math.ainl", "/proj/lib/math.ainl"]
    );
    // A path-like specifier: the importer's directory first, then the cwd.
    assert_eq!(
        names(import::candidates("sub/math", d)),
        vec!["/proj/lib/sub/math.ainl", "sub/math.ainl"]
    );
    // An absolute path: exactly one candidate, used as-is.
    let abs = import::candidates("/opt/m.ainl", d);
    assert_eq!(abs.len(), 1);
    assert_eq!(abs[0].0, PathBuf::from("/opt/m.ainl"));
    // An extension is added only when absent.
    assert_eq!(
        names(import::candidates("m.txt", d)),
        vec!["m.txt", "/proj/lib/m.txt"]
    );
}

#[test]
fn the_loader_evaluates_modules_with_the_requested_executor() {
    // A closure made by one interpreter cannot be called by the other, so the
    // loader has to run a module's body on the same interpreter the caller
    // will use. Proven by defining a function in a module and calling it
    // through each interpreter: a mixed executor would fail one of them.
    let t = Tree::new("executor");
    t.write(
        "m.ainl",
        "(def twice (fn (f x) (f (f x))))\n(def inc (fn (n) (+ n 1)))\n",
    );
    let p = t.write("main.ainl", "(import \"m\")\n(twice inc 5)\n");
    assert_both(&t.read("main.ainl"), &p, "7");
}

#[test]
fn a_module_may_define_a_recursive_function() {
    // A module body is an ordinary program, so the closure the module makes is
    // created by the executing interpreter and must be self-consistent.
    let t = Tree::new("recursive");
    t.write(
        "m.ainl",
        "(def fact (fn (n) (if (< n 2) 1 (* n (fact (- n 1))))))\n",
    );
    let p = t.write("main.ainl", "(import \"m\")\n(fact 5)\n");
    assert_both(&t.read("main.ainl"), &p, "120");
}

#[test]
fn strip_imports_leaves_every_other_form_untouched() {
    // The loader removes directives before an interpreter sees the program, so
    // the surviving forms must be the *same nodes* — same values, same spans,
    // same order. Spans still point into the original source, so the
    // comparison is made per form against the node at the same index of the
    // full parse: removing a directive must not renumber anything.
    let src = "(def a 1)\n(import \"m\")\n(print a)\n";
    let forms = ainl_core::parse(src).expect("parse");
    let stripped = import::strip_imports(&forms);
    assert_eq!(stripped.len(), 2, "only the directive is removed");
    assert_eq!(stripped[0], forms[0], "the first form is untouched");
    assert_eq!(stripped[1], forms[2], "the last form is untouched");
    // And the removed node really was the directive.
    let Node::List(items, _) = &forms[1] else {
        panic!("expected the middle form to be a list");
    };
    assert!(matches!(items.first(), Some(Node::Sym(s, _)) if s == "import"));
}

#[test]
fn a_loader_built_for_one_executor_works_with_either() {
    // Constructing a loader directly, since the public runners hide it.
    let t = Tree::new("loader-direct");
    t.write("m.ainl", "(def v 3)\n");
    let module = import::load("m", t.dir(), &Loader::new(Executor::Vm)).expect("load on VM");
    assert_eq!(module.exports.len(), 1);
    assert_eq!(module.exports[0].0, "v");

    let module = import::load("m", t.dir(), &Loader::new(Executor::TreeWalk)).expect("tree-walk");
    assert_eq!(module.exports.len(), 1);
    assert_eq!(module.exports[0].0, "v");
}

#[test]
fn a_module_error_names_the_module_file() {
    // The message must point at the file with the bug, not at the importer.
    let t = Tree::new("err-names");
    t.write("lib/m.ainl", "(def v (nosuchfunction))\n");
    let p = t.write("main.ainl", "(import \"lib/m\")\n");
    let err = run_vm(&t.read("main.ainl"), &p)
        .expect_err("must fail")
        .to_string();
    assert!(err.contains("nosuchfunction"), "{err}");
    assert!(err.contains("m.ainl"), "the module must be named: {err}");
}
