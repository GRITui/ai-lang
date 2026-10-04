//! Python projection of the Tier 3 file-system builtins.
//!
//! The *behaviour* contract — byte-identical stdout and stderr against the
//! interpreter — is checked end-to-end by `scripts/check-fs-builtins.sh`, which
//! runs one program through all five runners. These tests cover the other half:
//! that each builtin maps to the *host's* idiom where that idiom is correct,
//! and to an explicit implementation where it is not.
//!
//! The three cases where Python's own answer is wrong are the reason this file
//! exists, and each is asserted here:
//!
//! * `os.rename` **silently overwrites** an existing destination where POSIX
//!   `rename(2)` refuses, so AINL pre-checks and must not call it unguarded.
//! * `os.makedirs(exist_ok=True)` is quiet on an existing path where AINL
//!   errors, so it may only ever be used for the *parents*.
//! * `os.path.isdir` and `os.path.getsize` **follow** a symlink where the
//!   interpreter's `lstat` does not, so `islink` is checked first.

use ainl_transpile::transpile_python_src;

fn py(src: &str) -> String {
    transpile_python_src(src).unwrap_or_else(|e| panic!("transpile failed for `{src}`: {e}"))
}

#[test]
fn file_system_builtins_map_to_their_helpers() {
    let out = py(r#"(do (mkdir "d" ":recursive")
             (rmdir "d" ":recursive")
             (rename "a" "b")
             (copy "a" "b")
             (is-dir "d")
             (file-size "a"))"#);
    for call in [
        "_mkdir(\"d\", \":recursive\")",
        "_rmdir(\"d\", \":recursive\")",
        "_rename(\"a\", \"b\")",
        "_copy(\"a\", \"b\")",
        "_is_dir(\"d\")",
        "_file_size(\"a\")",
    ] {
        assert!(out.contains(call), "missing {call}:\n{out}");
    }
    for helper in [
        "_mkdir",
        "_rmdir",
        "_rename",
        "_copy",
        "_is_dir",
        "_file_size",
        "_fs_probe",
    ] {
        assert!(
            out.contains(&format!("def {helper}(")),
            "missing helper {helper}:\n{out}"
        );
    }
    // The recursive walk is pulled in by _rmdir alone, so a program that only
    // renames does not carry it.
    let without = py(r#"(rename "a" "b")"#);
    assert!(
        !without.contains("def _fs_rm_tree("),
        "_fs_rm_tree must not be emitted unless rmdir is used:\n{without}"
    );
    assert!(
        out.contains("def _fs_rm_tree("),
        "_rmdir must bring its recursive walk:\n{out}"
    );
}

#[test]
fn rename_never_reaches_os_rename_unguarded() {
    // os.rename clobbers an existing destination silently. AINL refuses, so
    // both existence checks must be in the emitted helper — this is the
    // assertion that keeps a future "simplification" from reintroducing the
    // data loss on Python while the C runtime refuses it.
    let out = py(r#"(rename "a" "b")"#);
    assert!(
        out.contains("os.path.lexists(_fs_probe(src))"),
        "the source must be checked:\n{out}"
    );
    assert!(
        out.contains("os.path.lexists(_fs_probe(dst))"),
        "the destination must be checked:\n{out}"
    );
    // And the order matters: the destination check must come *before* the call,
    // or the clobber has already happened.
    let dst_check = out
        .find("os.path.lexists(_fs_probe(dst))")
        .expect("destination check");
    let call = out.find("os.rename(src, dst)").expect("the call");
    assert!(dst_check < call, "the check must precede os.rename:\n{out}");
    // The cross-device case is named, not collapsed.
    assert!(out.contains("errno.EXDEV"), "EXDEV must be named:\n{out}");
    assert!(
        out.contains("different filesystems"),
        "the cross-device message must be distinct:\n{out}"
    );
}

#[test]
fn mkdir_uses_makedirs_only_for_parents() {
    // os.makedirs(exist_ok=True) is silent on an existing path; AINL errors.
    // So the existence check runs first, and makedirs is reached only on the
    // recursive branch, only for the parent.
    let out = py(r#"(mkdir "d" ":recursive")"#);
    let exists = out
        .find("os.path.lexists(_fs_probe(path))")
        .expect("the existence check");
    let makedirs = out
        .find("os.makedirs(parent, exist_ok=True)")
        .expect("makedirs for the parents");
    assert!(
        exists < makedirs,
        "the existence check must precede makedirs:\n{out}"
    );
    // `os.mkdir` — not `os.makedirs` — creates the leaf, so the leaf's
    // refusal is the host's own EEXIST... which AINL never reaches because the
    // check above already caught it.
    assert!(
        out.contains("os.mkdir(path)"),
        "the leaf uses os.mkdir:\n{out}"
    );
    assert!(
        out.contains("it exists"),
        "the exists message must be AINL's:\n{out}"
    );
}

#[test]
fn is_dir_and_file_size_check_islink_before_the_host_call() {
    // os.path.isdir and os.path.getsize both follow a symlink; the
    // interpreter's lstat does not. Without the islink guard, a symlink to a
    // directory would read as a directory here and as nil there.
    let out = py(r#"(do (is-dir "p") (file-size "p"))"#);
    assert!(
        out.contains("os.path.islink(p)"),
        "_is_dir must check islink:\n{out}"
    );
    assert!(
        out.contains("os.path.islink(p)"),
        "_file_size must check islink:\n{out}"
    );
    assert!(
        out.contains("return None"),
        "a missing or linked path is nil, not false:\n{out}"
    );
    // A directory is refused rather than measured, because POSIX reports the
    // inode size (4096 on ext4, 60 on APFS, 0 on tmpfs).
    assert!(
        out.contains("it is a directory"),
        "a directory must be refused:\n{out}"
    );
}

#[test]
fn file_system_failures_go_through_error_not_a_host_exception() {
    // A host exception is not an _AinlError, so it would escape an AINL
    // `catch` and print a Python traceback to stderr — which the byte
    // comparison fails on. Every failure must be routed through `_error`.
    let out =
        py(r#"(do (mkdir "d") (rename "a" "b") (copy "a" "b") (is-dir "p") (file-size "p"))"#);
    assert!(
        out.contains("def _error("),
        "_error must be emitted:\n{out}"
    );
    for who in [
        "mkdir: cannot create",
        "rename: cannot move",
        "copy: cannot copy",
        "file-size: cannot read",
    ] {
        assert!(
            out.contains(&format!("_error(\"{who}")),
            "the {who} message must be AINL's own:\n{out}"
        );
    }
    // Arity is checked inside the helper with *args, not left to a Python
    // signature error, for the same reason.
    assert!(
        out.contains("def _mkdir(*args)"),
        "_mkdir must take *args so arity is AINL's error:\n{out}"
    );
    assert!(
        out.contains("def _rename(*args)") && out.contains("def _copy(*args)"),
        "the two-argument builtins must also own their arity:\n{out}"
    );
    // And the type name comes from the shared helper, so the text says
    // "got int" rather than Python's "<class 'int'>".
    assert!(
        out.contains("_ainl_tname"),
        "the type name must be AINL's:\n{out}"
    );
}

#[test]
fn rmdir_never_reaches_shutil_rmtree_and_names_the_blocking_entry() {
    // Two things, both of which a naive port gets wrong.
    //
    // 1. shutil.rmtree follows a symlinked *file* on some platforms and its
    //    error handling differs by version, so a program that deleted through a
    //    link would behave differently per host. The walk is hand-rolled.
    // 2. The refusal must NAME the entry, and in byte order — os.listdir returns
    //    filesystem order, so "the first entry" has to be chosen by a rule every
    //    backend can express, the same one list-dir already uses.
    let out = py(r#"(rmdir "d" ":recursive")"#);
    assert!(
        !out.contains("shutil.rmtree"),
        "the host's recursive delete must not be reached:\n{out}"
    );
    assert!(
        out.contains("names.sort(key=lambda s: s.encode('utf-8'))"),
        "the named entry must be the first in byte order:\n{out}"
    );
    assert!(
        out.contains("it is not empty (%s)"),
        "the refusal must name the entry:\n{out}"
    );
    // The walk is post-order and never through a link: islink is checked BEFORE
    // isdir, because a link to a directory would otherwise be descended into.
    let walk = out
        .split("def _fs_rm_tree(")
        .nth(1)
        .expect("_fs_rm_tree must be emitted");
    let islink = walk.find("os.path.islink(c)").expect("the islink guard");
    let isdir = walk.find("os.path.isdir(c)").expect("the isdir branch");
    assert!(islink < isdir, "islink must be checked first:\n{out}");
}

#[test]
fn rmdir_refuses_a_symlink_and_a_missing_path_in_own_words() {
    // A link to a directory is not a directory (the lstat rule), and a missing
    // path is an error rather than nil. Both messages are AINL's, raised
    // through _error, so a `catch` can intercept them.
    let out = py(r#"(rmdir "d")"#);
    assert!(
        out.contains("os.path.islink(p) or not os.path.isdir(p)"),
        "a symlink must not be treated as a directory:\n{out}"
    );
    assert!(
        out.contains("it does not exist") && out.contains("it is not a directory"),
        "both refusals must be AINL's own text:\n{out}"
    );
    assert!(
        out.contains("def _rmdir(*args)"),
        "rmdir must take *args so arity is AINL's error, not a Python one:\n{out}"
    );
    // The arity guard must come before the path is used, or a wrong-typed
    // option would be reported as a path error.
    let arity = out
        .find("if len(args) < 1 or len(args) > 2")
        .expect("arity");
    let path = out
        .find("_error('rmdir expects a str path")
        .expect("path check");
    assert!(arity < path, "arity must be checked first:\n{out}");
}

#[test]
fn the_trailing_separator_is_trimmed_by_the_shared_probe() {
    // "f/" is not a legal way to name a non-directory on any of the four
    // hosts, so the query path is trimmed. One helper, used by all six
    // builtins, is what keeps that consistent.
    let out = py(
        r#"(do (is-dir "p") (file-size "p") (mkdir "d") (rmdir "d") (rename "a" "b") (copy "a" "b"))"#,
    );
    assert!(
        out.contains("def _fs_probe(path):") && out.contains("path.endswith('/')"),
        "_fs_probe must strip a trailing separator:\n{out}"
    );
    // A lone "/" is the root and must survive, which is why the strip is
    // guarded on length.
    assert!(
        out.contains("len(path) > 1"),
        "the root must not be trimmed to empty:\n{out}"
    );
}
