//! JavaScript projection of the Tier 3 file-system builtins.
//!
//! The behaviour contract is checked by `scripts/check-fs-builtins.sh`. This
//! file checks the *projection*: that each builtin reaches the host's `fs`
//! module the way the C runtime reaches POSIX, and that the three places where
//! Node's default behaviour is wrong for AINL are guarded.
//!
//! Node is the worst of the four hosts here, because `fs.renameSync` silently
//! overwrites (like Python and Ruby) and `fs.statSync` follows symlinks while
//! `fs.lstatSync` does not. Every helper therefore uses `lstatSync` through the
//! shared `_fs_probe`, and `_rename` pre-checks the destination.

use ainl_transpile::transpile_js_src;

fn js(src: &str) -> String {
    transpile_js_src(src).unwrap_or_else(|e| panic!("transpile failed for `{src}`: {e}"))
}

#[test]
fn file_system_builtins_map_to_their_helpers() {
    let out = js(r#"(do (mkdir "d" ":recursive")
             (rename "a" "b")
             (copy "a" "b")
             (is-dir "d")
             (file-size "a"))"#);
    for call in [
        "_mkdir(\"d\", \":recursive\")",
        "_rename(\"a\", \"b\")",
        "_copy(\"a\", \"b\")",
        "_is_dir(\"d\")",
        "_file_size(\"a\")",
    ] {
        assert!(out.contains(call), "missing {call}:\n{out}");
    }
    for helper in [
        "_mkdir",
        "_rename",
        "_copy",
        "_is_dir",
        "_file_size",
        "_fs_probe",
    ] {
        assert!(
            out.contains(&format!("function {helper}(")),
            "missing helper {helper}:\n{out}"
        );
    }
}

#[test]
fn rename_refuses_an_existing_destination_before_calling_rename_sync() {
    // fs.renameSync overwrites. The destination check must therefore precede
    // the call, not merely be present in the file.
    let out = js(r#"(rename "a" "b")"#);
    let check = out
        .find("fs.lstatSync(_fs_probe(dst));")
        .expect("the destination check");
    let call = out
        .find("fs.renameSync(src, dst);")
        .expect("the rename call");
    assert!(check < call, "the check must precede renameSync:\n{out}");
    assert!(out.contains("`rename: cannot move '${src}': '${dst}' exists`"));
    // The source is checked too, so a missing source has its own message
    // rather than a generic one.
    assert!(out.contains("it does not exist"), "got:\n{out}");
    // EXDEV is named rather than collapsed, so a cross-device move is
    // diagnosable instead of reading as a permissions problem.
    assert!(out.contains("\"EXDEV\""), "EXDEV must be named:\n{out}");
    assert!(out.contains("different filesystems"), "got:\n{out}");
}

#[test]
fn mkdir_uses_mkdir_sync_with_the_recursive_flag_and_checks_first() {
    // Node's mkdirSync({recursive:true}) is *quiet* on an existing path where
    // AINL errors, so the lstat check must run first.
    let out = js(r#"(mkdir "d" ":recursive")"#);
    let check = out
        .find("fs.lstatSync(_fs_probe(path));")
        .expect("the existence check");
    let call = out
        .find("fs.mkdirSync(path, opt === \":recursive\"")
        .expect("mkdirSync");
    assert!(check < call, "the check must precede mkdirSync:\n{out}");
    assert!(
        out.contains("`mkdir: cannot create '${path}': it exists`"),
        "got:\n{out}"
    );
    // A non-recursive mkdir passes `undefined`, not `{recursive:false}`, so
    // the plain and recursive forms are genuinely different syscalls.
    assert!(
        out.contains("? { recursive: true } : undefined"),
        "the flag must be omitted, not false:\n{out}"
    );
}

#[test]
fn the_five_builtins_all_probe_through_lstat_not_stat() {
    // fs.statSync follows a symlink; the interpreter's symlink_metadata is an
    // lstat. A symlink to a directory would read as a directory here and as nil
    // there, so none of the five may use statSync or existsSync.
    let out =
        js(r#"(do (mkdir "d") (rename "a" "b") (copy "a" "b") (is-dir "p") (file-size "p"))"#);
    for forbidden in ["fs.statSync", "fs.existsSync"] {
        assert!(
            !out.contains(forbidden),
            "{forbidden} follows a symlink and must not be used:\n{out}"
        );
    }
    // _fs_probe trims a trailing separator, because "f/" is not a legal way to
    // name a non-directory on any host. The guard keeps "/" intact.
    assert!(
        out.contains("p.length > 1 && p.endsWith(\"/\")"),
        "got:\n{out}"
    );
}

#[test]
fn is_dir_returns_nil_not_false() {
    // AINL has one falsy value, nil. Returning `false` would make `(if
    // (is-dir p) ...)` behave the same but `(= (is-dir p) nil)` false, so a
    // caller could not distinguish "not a directory" from "no such path".
    let out = js(r#"(is-dir "p")"#);
    assert!(
        out.contains("return null;"),
        "a missing path or a non-directory is nil:\n{out}"
    );
    assert!(out.contains(".isDirectory() ? true : null"), "got:\n{out}");
}

#[test]
fn file_size_uses_the_lstat_size_and_refuses_a_directory() {
    // POSIX reports a directory's *inode* size — 4096 on ext4, 60 on APFS, 0
    // on tmpfs — so a naive getsizeSync would answer a host-specific number
    // rather than an error.
    let out = js(r#"(file-size "p")"#);
    assert!(
        out.contains("`file-size: cannot read '${path}': it is a directory`"),
        "a directory must be refused:\n{out}"
    );
    assert!(
        out.contains("return st.size;"),
        "the size is the lstat size:\n{out}"
    );
    // And it is the size in bytes, so a multi-byte string is longer than its
    // character count — which the parity fixture checks with "héllo".
}

#[test]
fn arity_and_type_failures_are_ainl_errors_not_host_throws() {
    // A JS TypeError is not an AINL error, so it would escape an AINL `catch`
    // and print a JS stack trace to stderr. Every check goes through _error.
    let out =
        js(r#"(do (mkdir "d") (rename "a" "b") (copy "a" "b") (is-dir "p") (file-size "p"))"#);
    for helper in ["_mkdir", "_rename", "_copy", "_is_dir", "_file_size"] {
        assert!(
            out.contains(&format!("function {helper}(...args)")),
            "{helper} must take ...args so arity is AINL's error:\n{out}"
        );
    }
    assert!(
        out.contains("function _error("),
        "_error must be emitted:\n{out}"
    );
    // The rendered type name is AINL's, not the host's `number`.
    assert!(
        out.contains("_ainl_tname"),
        "the type name must be AINL's:\n{out}"
    );
    for who in [
        "mkdir: cannot create",
        "rename: cannot move",
        "copy: cannot copy",
        "file-size: cannot read",
    ] {
        assert!(
            out.contains(&format!("{who} '")),
            "the {who} message must be AINL's own:\n{out}"
        );
    }
}

#[test]
fn copy_uses_copy_file_sync_and_refuses_a_directory() {
    // fs.copyFileSync copies the bytes and preserves the mode, and is the
    // single-syscall equivalent of read -> write. It fails on a directory
    // anyway, but with a host-specific message, so the check comes first.
    let out = js(r#"(copy "a" "b")"#);
    assert!(out.contains("fs.copyFileSync(p, dst);"), "got:\n{out}");
    assert!(
        out.contains("`copy: cannot copy '${src}': it is a directory`"),
        "a directory copy must be refused by name:\n{out}"
    );
    assert!(out.contains("it does not exist"), "got:\n{out}");
}
