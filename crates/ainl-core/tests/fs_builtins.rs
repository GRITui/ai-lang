//! The five file-system builtins — the two evaluators, and the edges.
//!
//! The 4-backend rule makes this a *parity* test first and a *behaviour* test
//! second. `run_str` is the bytecode VM and `run_in_tree_walk` is the
//! tree-walking evaluator; both reach the same `BuiltinFn` through different
//! entry points, so a bug in either is invisible to a single-backend test. The
//! AOT and transpiler halves live in the sibling crates
//! (`aot_stdlib.rs`, the `*_stdlib.rs` transpile tests) and in
//! `scripts/check-fs-builtins.sh`.
//!
//! Two things make this suite unlike the string one, and both are because
//! these builtins *change the filesystem* rather than inspect it:
//!
//! 1. **Every test gets its own directory.** They create, move and delete
//!    real paths, so a fixed name would make the tests interfere with each
//!    other and with the developer's working tree. `scratch` makes a unique
//!    directory under the system temp dir and removes it afterwards.
//!
//! 2. **The hosts disagree, so the interesting cases are the edges.** The
//!    rules are in docs/SYNTAX.md §3h; what these tests protect is that the
//!    interpreter actually implements them, including the two that a natural
//!    implementation would get wrong: `rename` must *refuse* an existing
//!    destination (os.rename and File.rename clobber it silently), and
//!    `mkdir` must refuse an existing path even in `:recursive` mode
//!    (os.makedirs(exist_ok=True) and mkdir -p do not).

use ainl_core::{run_in_tree_walk, run_str};
use std::path::{Path, PathBuf};

/// A unique scratch directory for one test, removed when it drops.
///
/// Named after the test rather than shared, so two tests never contend for a
/// path and a failure names the directory that caused it.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        // The pid keeps parallel test threads apart; the tag keeps the two
        // evaluators' runs in one test from sharing a directory.
        let mut p = std::env::temp_dir();
        p.push(format!(
            "ainl-fs-{}-{}-{:?}",
            std::process::id(),
            tag,
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("create scratch dir");
        Scratch { path: p }
    }

    fn join(&self, rel: &str) -> String {
        self.path.join(rel).to_string_lossy().into_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn vm(src: &str) -> String {
    match run_str(src) {
        Ok(v) => v.to_string(),
        Err(e) => format!("ERR: {}", e.message()),
    }
}

fn tree(src: &str) -> String {
    match run_in_tree_walk(src) {
        Ok(v) => v.to_string(),
        Err(e) => format!("ERR: {}", e.message()),
    }
}

/// Assert both evaluators produce the same printed value AND that it is
/// `want`, so a test cannot pass by both backends being wrong together.
fn both(src: &str, want: &str) {
    assert_eq!(vm(src), want, "VM for `{src}`");
    assert_eq!(tree(src), want, "tree-walk for `{src}`");
}

/// Run a program that **changes the filesystem** under both evaluators, each
/// against its own freshly-built tree, and assert both produced the expected
/// result.
///
/// The `both` helper above cannot be used for these builtins: they are not
/// idempotent, so the VM run performs the move and the tree-walk run then fails
/// on a source that is genuinely gone — correct behaviour, not a bug. The two
/// evaluators reach the same `BuiltinFn` through different entry points, so
/// the honest test is that *both can* perform the operation.
///
/// `setup` builds the tree and `body(root)` returns the AINL source, where
/// `root` is the scratch directory. `want` is the exact rendered value, and it
/// is checked per evaluator rather than by comparing the two outputs: each
/// evaluator gets a *differently named* directory, so the two outputs would
/// necessarily differ in their embedded paths even when the behaviour is
/// identical. Comparing against a fixed `want` is the stronger assertion — it
/// pins the value, not just the agreement.
fn both_fs(tag: &str, setup: impl Fn(&Path), body: impl Fn(&str) -> String, want: &str) {
    for (label, run) in [
        ("VM", vm as fn(&str) -> String),
        ("tree-walk", tree as fn(&str) -> String),
    ] {
        let s = Scratch::new(&format!("{tag}-{label}"));
        setup(&s.path);
        let out = run(&body(&s.path.to_string_lossy()));
        assert_eq!(out, want, "{label} for {tag}");
    }
}

// ---- mkdir ----------------------------------------------------------------

#[test]
fn mkdir_creates_one_directory() {
    both_fs(
        "mkdir1",
        |_| {},
        |root| format!("(str (mkdir \"{root}/d\") \" \" (str (is-dir \"{root}/d\")))"),
        "nil true",
    );
}

#[test]
fn mkdir_recursive_creates_every_missing_parent() {
    // Three levels deep and none of them exist. This is the call the e2e
    // organizer could not make before, and the one whose host spellings all
    // differ (create_dir, create_dir_all, {recursive:true}, FileUtils.mkdir_p).
    both_fs(
        "mkdir-r",
        |_| {},
        |root| {
            format!(
                "(str (mkdir \"{root}/a/b/c\" \":recursive\") \" \" \
                 (str (is-dir \"{root}/a\") \" \" (is-dir \"{root}/a/b\") \" \" (is-dir \"{root}/a/b/c\")))"
            )
        },
        "nil true true true",
    );
}

#[test]
fn mkdir_without_recursive_fails_on_a_missing_parent() {
    let s = Scratch::new("mkdir-noparent");
    // The error names the path the *caller* wrote, not the missing parent —
    // a rule, because the caller did not name the parent.
    let p = s.join("absent/child");
    for out in [
        vm(&format!("(mkdir \"{p}\")")),
        tree(&format!("(mkdir \"{p}\")")),
    ] {
        assert_eq!(out, format!("ERR: mkdir: cannot create '{p}'"));
    }
    // Neither run may have created the parent: a non-recursive mkdir that
    // half-succeeded would be the worst outcome, since the caller would then
    // find a directory it never asked for.
    assert!(!s.path.join("absent").exists());
}

#[test]
fn mkdir_on_an_existing_path_is_an_error_in_both_modes() {
    let s = Scratch::new("mkdir-exists");
    let d = s.join("d");
    std::fs::create_dir_all(&d).unwrap();
    // Plain, and then recursive: mkdir -p and os.makedirs(exist_ok=True) both
    // treat the second as a success, so this is the assertion that pins AINL's
    // stricter rule. A caller creating a directory and then writing into it
    // must be able to tell whether *it* created it.
    let want = format!("ERR: mkdir: cannot create '{d}': it exists");
    assert_eq!(vm(&format!("(mkdir \"{d}\")")), want);
    assert_eq!(tree(&format!("(mkdir \"{d}\")")), want);
    // The recursive form is the interesting one, and the second run is safe
    // because the first changed nothing.
    let want = format!("ERR: mkdir: cannot create '{d}': it exists");
    assert_eq!(vm(&format!("(mkdir \"{d}\" \":recursive\")")), want);
    assert_eq!(tree(&format!("(mkdir \"{d}\" \":recursive\")")), want);
}

#[test]
fn mkdir_rejects_an_unknown_option() {
    let s = Scratch::new("mkdir-opt");
    both(
        &format!("(mkdir \"{}\" \":parents\")", s.join("d")),
        "ERR: mkdir: unknown option ':parents'",
    );
    assert!(!s.path.join("d").exists());
}

// ---- rename ---------------------------------------------------------------

#[test]
fn rename_moves_a_file_and_its_content_goes_with_it() {
    both_fs(
        "rename-file",
        |root| {
            std::fs::write(root.join("a.txt"), "payload").unwrap();
        },
        |root| {
            format!(
                "(str (rename \"{root}/a.txt\" \"{root}/b.txt\") \" \" \
                 (str (file-exists \"{root}/a.txt\") \" \" (read-file \"{root}/b.txt\")))"
            )
        },
        // nil = the rename returned nil, nil = the source is gone, payload =
        // the content arrived with the move.
        "nil nil payload",
    );
}

#[test]
fn rename_moves_a_whole_directory_subtree() {
    // The case read -> write -> delete cannot express at all: a rename never
    // reads a byte, so a large tree costs one syscall rather than a full copy,
    // and a crash mid-move cannot leave two half-copies.
    both_fs(
        "rename-dir",
        |root| {
            std::fs::create_dir_all(root.join("src/inner")).unwrap();
            std::fs::write(root.join("src/inner/deep.txt"), "x").unwrap();
        },
        |root| {
            format!(
                "(str (rename \"{root}/src\" \"{root}/dst\") \" \" \
                 (str (is-dir \"{root}/dst\") \" \" (is-dir \"{root}/dst/inner\") \" \" \
                      (read-file \"{root}/dst/inner/deep.txt\")))"
            )
        },
        "nil true true x",
    );
}

#[test]
fn rename_refuses_an_existing_destination_instead_of_clobbering_it() {
    let s = Scratch::new("rename-clobber");
    std::fs::write(s.path.join("src.txt"), "SOURCE").unwrap();
    std::fs::write(s.path.join("dst.txt"), "DESTINATION").unwrap();
    // This is the sharpest edge in the card. os.rename, fs.renameSync and
    // File.rename all OVERWRITE the destination silently; POSIX rename(2) does
    // not. AINL pins the refusal so the same program cannot destroy a file on
    // some backends and preserve it on others. A *refused* rename changes
    // nothing, so both evaluators can safely run against this one tree.
    let call = format!(
        "(rename \"{}\" \"{}\")",
        s.join("src.txt"),
        s.join("dst.txt")
    );
    let want = format!(
        "ERR: rename: cannot move '{}': '{}' exists",
        s.join("src.txt"),
        s.join("dst.txt")
    );
    assert_eq!(vm(&call), want);
    assert_eq!(tree(&call), want);
    assert_eq!(
        std::fs::read_to_string(s.path.join("dst.txt")).unwrap(),
        "DESTINATION",
        "the destination must be untouched"
    );
    assert_eq!(
        std::fs::read_to_string(s.path.join("src.txt")).unwrap(),
        "SOURCE",
        "the source must be untouched"
    );
}

#[test]
fn rename_reports_a_missing_source() {
    let s = Scratch::new("rename-missing");
    both(
        &format!("(rename \"{}\" \"{}\")", s.join("ghost"), s.join("x")),
        &format!(
            "ERR: rename: cannot move '{}': it does not exist",
            s.join("ghost")
        ),
    );
}

// ---- copy -----------------------------------------------------------------

#[test]
fn copy_preserves_content_byte_for_byte_and_the_copies_diverge() {
    let s = Scratch::new("copy");
    // 6 bytes, 5 characters: a copy that went through a str round-trip could
    // not preserve this by accident, and the byte count is asserted below.
    std::fs::write(s.path.join("a.txt"), "héllo").unwrap();
    both(
        &format!("(copy \"{}\" \"{}\")", s.join("a.txt"), s.join("b.txt")),
        "nil",
    );
    assert_eq!(std::fs::read(s.path.join("b.txt")).unwrap().len(), 6);
    // Divergence is the documented difference from rename: a copy is a full
    // read + write, not a hardlink, so writing to one must not touch the other.
    std::fs::write(s.path.join("b.txt"), "changed").unwrap();
    assert_eq!(
        std::fs::read_to_string(s.path.join("a.txt")).unwrap(),
        "héllo"
    );
}

#[test]
fn copy_of_an_empty_file_stays_empty() {
    let s = Scratch::new("copy-empty");
    std::fs::write(s.path.join("empty"), "").unwrap();
    both(
        &format!("(copy \"{}\" \"{}\")", s.join("empty"), s.join("empty2")),
        "nil",
    );
    assert!(s.path.join("empty2").is_file());
    both(&format!("(file-size \"{}\")", s.join("empty2")), "0");
}

#[test]
fn copy_refuses_a_directory() {
    let s = Scratch::new("copy-dir");
    std::fs::create_dir_all(s.path.join("d")).unwrap();
    both(
        &format!("(copy \"{}\" \"{}\")", s.join("d"), s.join("x")),
        &format!(
            "ERR: copy: cannot copy '{}': it is a directory",
            s.join("d")
        ),
    );
    assert!(!s.path.join("x").exists());
}

// ---- is-dir ---------------------------------------------------------------

#[test]
fn is_dir_is_true_only_for_a_directory() {
    let s = Scratch::new("isdir");
    std::fs::create_dir_all(s.path.join("d")).unwrap();
    std::fs::write(s.path.join("f.txt"), "x").unwrap();
    both(&format!("(is-dir \"{}\")", s.join("d")), "true");
    both(&format!("(is-dir \"{}\")", s.join("f.txt")), "nil");
    // A missing path is nil, NOT an error: "is this a directory?" has one
    // negative answer, matching file-exists, so `(= (is-dir p) nil)` is the
    // absence test a program writes.
    both(&format!("(is-dir \"{}\")", s.join("ghost")), "nil");
}

#[test]
fn is_dir_replaces_the_trailing_dot_trick() {
    let s = Scratch::new("isdir-trick");
    std::fs::create_dir_all(s.path.join("d")).unwrap();
    std::fs::write(s.path.join("f.txt"), "x").unwrap();
    // The old idiom the e2e organizer used, asserted here to be *equivalent*
    // for a real directory so the new builtin is a drop-in for it.
    both(
        &format!("(file-exists (path-join \"{}\" \".\"))", s.join("d")),
        "true",
    );
    both(&format!("(is-dir \"{}\")", s.join("d")), "true");
    // And this is where the trick was misleading: on a *file* the old idiom
    // also answered true, because a trailing "." is kept as a component and
    // "f.txt/." still stats successfully on some hosts. The real primitive
    // does not have that gap.
    both(&format!("(is-dir \"{}\")", s.join("f.txt")), "nil");
}

#[test]
fn is_dir_ignores_a_trailing_separator() {
    let s = Scratch::new("isdir-slash");
    std::fs::create_dir_all(s.path.join("d")).unwrap();
    std::fs::write(s.path.join("f.txt"), "x").unwrap();
    // The hosts split on whether "f/" is a legal way to name a non-directory
    // (ENOTDIR in C, NotADirectoryError in Python, a throw in Node,
    // Errno::ENOTDIR in Ruby), so the trailing separator is trimmed before the
    // query. Both spellings must answer the same.
    both(&format!("(is-dir \"{}/\")", s.join("d")), "true");
    both(&format!("(is-dir \"{}/\")", s.join("f.txt")), "nil");
}

// ---- file-size ------------------------------------------------------------

#[test]
fn file_size_counts_bytes_not_characters() {
    let s = Scratch::new("size");
    // "héllo" is 5 characters and 6 bytes. `len` on the string says 5, so
    // `(len (read-file p))` cannot be used to size a file — which is exactly
    // why this builtin exists alongside the byte-indexed string primitives.
    std::fs::write(s.path.join("a.txt"), "héllo").unwrap();
    both(&format!("(file-size \"{}\")", s.join("a.txt")), "6");
    both(&format!("(len (read-file \"{}\"))", s.join("a.txt")), "5");
}

#[test]
fn file_size_of_an_empty_file_is_zero_and_of_a_directory_is_an_error() {
    let s = Scratch::new("size-dir");
    std::fs::create_dir_all(s.path.join("d")).unwrap();
    std::fs::write(s.path.join("empty"), "").unwrap();
    both(&format!("(file-size \"{}\")", s.join("empty")), "0");
    // A directory has no portable size: POSIX reports the inode's own size
    // (4096 on ext4, 60 on APFS, 0 on tmpfs), so answering would report a
    // filesystem implementation detail as a language value.
    both(
        &format!("(file-size \"{}\")", s.join("d")),
        &format!(
            "ERR: file-size: cannot read '{}': it is a directory",
            s.join("d")
        ),
    );
}

#[test]
fn file_size_reports_a_missing_path_rather_than_zero() {
    let s = Scratch::new("size-missing");
    // The same reason read-file errors instead of returning "": sizing a
    // typo'd path must not look like a zero-byte file.
    both(
        &format!("(file-size \"{}\")", s.join("ghost")),
        &format!("ERR: file-size: cannot read '{}'", s.join("ghost")),
    );
}

// ---- rmdir ----------------------------------------------------------------

#[test]
fn rmdir_removes_an_empty_directory() {
    let s = Scratch::new("rmdir-empty");
    let d = s.join("d");
    both_fs(
        "rmdir-empty",
        |_root| {},
        |root| {
            format!(
                "(str (mkdir \"{root}/d\") \" \" (rmdir \"{root}/d\") \" \" (is-dir \"{root}/d\"))"
            )
        },
        "nil nil nil",
    );
    assert!(!Path::new(&d).exists(), "the directory should be gone");
}

#[test]
fn rmdir_refuses_a_non_empty_directory_and_names_the_entry() {
    // The name is the load-bearing part. rmdir(2) says only ENOTEMPTY and the
    // hosts raise four differently-named errors, so a port that delegated would
    // produce a different message per backend — or none at all. AINL names the
    // first entry in byte order instead, which is both identical everywhere and
    // something the caller can act on.
    for (label, run) in [
        ("VM", vm as fn(&str) -> String),
        ("tree-walk", tree as fn(&str) -> String),
    ] {
        let s = Scratch::new(&format!("rmdir-full-{label}"));
        std::fs::create_dir_all(s.path.join("d")).unwrap();
        // "aaa.txt" < "zzz.txt" in byte order, so it is the one named even
        // though "zzz.txt" was created first — readdir order is not defined, so
        // the test would be flaky if the message used it.
        std::fs::write(s.path.join("d/zzz.txt"), "z").unwrap();
        std::fs::write(s.path.join("d/aaa.txt"), "a").unwrap();
        let out = run(&format!("(rmdir \"{}\")", s.join("d")));
        assert_eq!(
            out,
            format!(
                "ERR: rmdir: cannot remove '{}': it is not empty (aaa.txt)",
                s.join("d")
            ),
            "{label} rmdir on a non-empty directory"
        );
        // The refusal must not have deleted anything.
        assert!(s.path.join("d/aaa.txt").is_file(), "{label}: file survived");
        assert!(s.path.join("d/zzz.txt").is_file(), "{label}: file survived");
    }
}

#[test]
fn rmdir_reports_a_missing_path_rather_than_pretending_success() {
    // An *action*, not a question: is-dir answers nil for this path, but
    // "delete this" against a typo must not look like it worked.
    let s = Scratch::new("rmdir-missing");
    let want = format!(
        "ERR: rmdir: cannot remove '{}': it does not exist",
        s.join("ghost")
    );
    assert_eq!(vm(&format!("(rmdir \"{}\")", s.join("ghost"))), want);
    assert_eq!(tree(&format!("(rmdir \"{}\")", s.join("ghost"))), want);
    // And ":recursive" is not a licence to invent a directory either.
    let want_r = format!(
        "ERR: rmdir: cannot remove '{}': it does not exist",
        s.join("ghost")
    );
    assert_eq!(
        vm(&format!("(rmdir \"{}\" \":recursive\")", s.join("ghost"))),
        want_r
    );
    assert_eq!(
        tree(&format!("(rmdir \"{}\" \":recursive\")", s.join("ghost"))),
        want_r
    );
}

#[test]
fn rmdir_refuses_a_file_and_leaves_it_alone() {
    for (label, run) in [
        ("VM", vm as fn(&str) -> String),
        ("tree-walk", tree as fn(&str) -> String),
    ] {
        let s = Scratch::new(&format!("rmdir-file-{label}"));
        std::fs::write(s.path.join("f.txt"), "A").unwrap();
        let out = run(&format!("(rmdir \"{}\")", s.join("f.txt")));
        assert_eq!(
            out,
            format!(
                "ERR: rmdir: cannot remove '{}': it is not a directory",
                s.join("f.txt")
            ),
            "{label} rmdir on a file"
        );
        assert_eq!(
            std::fs::read_to_string(s.path.join("f.txt")).unwrap(),
            "A",
            "{label}: the file must survive"
        );
    }
}

#[test]
fn rmdir_recursive_removes_the_whole_subtree() {
    for (label, run) in [
        ("VM", vm as fn(&str) -> String),
        ("tree-walk", tree as fn(&str) -> String),
    ] {
        let s = Scratch::new(&format!("rmdir-rec-{label}"));
        let root = s.path.to_string_lossy().into_owned();
        let out = run(&format!(
            r#"(do
                (mkdir "{root}/tree/x/y" ":recursive")
                (write-file "{root}/tree/x/y/deep.txt" "deep")
                (write-file "{root}/tree/top.txt" "top")
                (rmdir "{root}/tree" ":recursive"))"#
        ));
        assert_eq!(out, "nil", "{label} recursive rmdir");
        // The claim is on the filesystem, not in the return value: a delete that
        // removed only the top directory would also return nil.
        assert!(!s.path.join("tree").exists(), "{label}: subtree gone");
    }
}

#[test]
fn rmdir_recursive_unlinks_a_symlink_instead_of_following_it() {
    // The worst thing a recursive delete can do. AINL has no symlink builtin, so
    // the link is made by the test; the point is that every backend's *own*
    // recursive delete disagrees here (shutil.rmtree refuses a symlinked top
    // directory but not a link inside the tree, fs.rmSync follows, rm_rf does
    // not), which is why the rule is explicit rather than delegated.
    let s = Scratch::new("rmdir-symlink");
    std::fs::create_dir_all(s.path.join("tree")).unwrap();
    std::fs::create_dir_all(s.path.join("precious")).unwrap();
    std::fs::write(s.path.join("precious/keep.txt"), "keep").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(s.path.join("precious"), s.path.join("tree/link")).unwrap();

    let out = vm(&format!("(rmdir \"{}\" \":recursive\")", s.join("tree")));
    assert_eq!(out, "nil");
    assert!(!s.path.join("tree").exists(), "the tree itself is gone");
    assert!(
        s.path.join("precious/keep.txt").is_file(),
        "the symlink target must survive: rmdir must unlink the link, not follow it"
    );
}

#[test]
fn rmdir_rejects_an_unknown_option() {
    let s = Scratch::new("rmdir-opt");
    let want = "ERR: rmdir: unknown option ':parents'";
    assert_eq!(
        vm(&format!("(rmdir \"{}\" \":parents\")", s.join("d"))),
        want
    );
    assert_eq!(
        tree(&format!("(rmdir \"{}\" \":parents\")", s.join("d"))),
        want
    );
}

#[test]
fn rmdir_arity_and_type_errors_match_mkdirs_shape() {
    for src in ["(rmdir)", "(rmdir \"a\" \"b\" \"c\")"] {
        let want = "ERR: rmdir expects (rmdir path) or (rmdir path option)";
        assert_eq!(vm(src), want, "VM for {src}");
        assert_eq!(tree(src), want, "tree-walk for {src}");
    }
    both("(rmdir 1)", "ERR: rmdir expects a str path, got int");
    both(
        "(rmdir \"d\" 5)",
        "ERR: rmdir expects a str option, got int",
    );
}

// ---- the composed organizer ----------------------------------------------

#[test]
fn mkdir_plus_rename_organizes_a_tree() {
    // The e2e organizer, in the form it could not previously be written: make
    // the target directories with `mkdir ":recursive"`, then *move* each file
    // into place with `rename`, and detect the directories to skip with
    // `is-dir`. Before this card the same program had to read -> write ->
    // delete (which copies every byte and cannot move a directory at all) and
    // needed the target subdirs to already exist.
    let setup = |root: &Path| {
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        for (name, body) in [
            ("a.txt", "1"),
            ("b.txt", "2"),
            ("c.md", "3"),
            ("d.jpg", "4"),
        ] {
            std::fs::write(src.join(name), body).unwrap();
        }
        // A pre-existing directory that the organizer must skip rather than
        // descend into — the case `is-dir` exists for. The file inside it must
        // survive untouched, which is what makes the skip observable.
        std::fs::create_dir_all(src.join("txt")).unwrap();
        std::fs::write(src.join("txt/keep.txt"), "keep").unwrap();
    };

    let organizer = |src_str: &str| {
        format!(
            r#"
(def SRC "{src_str}")
; AINL has a hash, so the extension -> subdir mapping is one and `get` looks
; the extension up in it — no list walking, no six `(= e "txt")` comparisons.
; (The old organizer used a chain of `or`/`=` because it had no way to build
; this table; `hash` predates it, but the mapping was written as comparisons
; anyway. This is the card's other small win.)
(def RULES (hash "txt" "txt" "md" "md"))
(def last-of (fn (lst)
  (let ((cur lst))
    (while (not (= (rest cur) (list)))
      (def cur (rest cur)))
    (first cur))))
(def ext-of (fn (name)
  (let ((parts (split name ".")))
    (if (= (len parts) 1) "" (last-of parts)))))
(def subdir-for (fn (ext)
  (let ((hit (get RULES ext)))
    (if (= hit nil) "" hit))))
; Create the targets up front. ":recursive" means the organizer never has to
; check whether the *parents* exist, and never has to know the extension set
; first. AINL's mkdir still refuses a path that already exists (see §3h), which
; is the right default but means a re-run of an organizer over a
; half-finished tree has to guard the call — so the guard is `file-exists`,
; which is what it is for. The first run creates both directories.
(if (not (file-exists (path-join SRC "txt")))
    (mkdir (path-join SRC "txt") ":recursive"))
(if (not (file-exists (path-join SRC "md")))
    (mkdir (path-join SRC "md") ":recursive"))
(def moved (list))
(def skipped (list))
(def cur (list-dir SRC))
(while (not (= cur (list)))
  (def name (first cur))
  (def p (path-join SRC name))
  (def e (ext-of name))
  (def sub (subdir-for e))
  (if (is-dir p)
      (def skipped (push skipped (str name " (dir)")))
      (if (= sub "")
          (def skipped (push skipped (str name " (ext=" e ")")))
          (do (rename p (path-join (path-join SRC sub) name))
              (def moved (push moved (str name " -> " sub "/"))))))
  (def cur (rest cur)))
"#
        )
    };

    for (label, run) in [
        ("VM", vm as fn(&str) -> String),
        ("tree-walk", tree as fn(&str) -> String),
    ] {
        let s = Scratch::new(&format!("organize-{label}"));
        setup(&s.path);
        let src = s.path.join("src");
        let full = format!(
            "{}\n(list (list-dir SRC) (list-dir (path-join SRC \"txt\")) (list-dir (path-join SRC \"md\")) moved skipped)\n",
            organizer(&src.to_string_lossy())
        );
        let out = run(&full);
        assert!(!out.starts_with("ERR: "), "{label} organizer failed: {out}");

        // The organizer moved the two text files and left everything else alone.
        assert!(src.join("txt/a.txt").is_file(), "{label}: a.txt -> txt/");
        assert!(src.join("txt/b.txt").is_file(), "{label}: b.txt -> txt/");
        assert!(src.join("md/c.md").is_file(), "{label}: c.md -> md/");
        assert!(
            src.join("d.jpg").is_file(),
            "{label}: an unknown extension stays put"
        );
        assert!(
            src.join("txt/keep.txt").is_file(),
            "{label}: a pre-existing file is untouched"
        );
        // The content moved with the file — rename is a move, not a rewrite,
        // which is exactly what the read->write->delete version could not be.
        assert_eq!(
            std::fs::read_to_string(src.join("txt/a.txt")).unwrap(),
            "1",
            "{label}: content survived the move"
        );
        // And the report says so, in sorted order because list-dir sorts.
        assert!(out.contains("a.txt -> txt/"), "{label} moved report: {out}");
        assert!(
            out.contains("d.jpg (ext=jpg)"),
            "{label} skipped report: {out}"
        );
        assert!(
            out.contains("txt (dir)"),
            "{label}: the directory is skipped, not descended into: {out}"
        );
    }
}
