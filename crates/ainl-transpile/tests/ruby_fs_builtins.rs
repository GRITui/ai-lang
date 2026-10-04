//! Ruby projection of the Tier 3 file-system builtins.
//!
//! The behaviour contract is checked by `scripts/check-fs-builtins.sh`. This
//! file checks the projection, and Ruby's projection is the most interesting
//! of the three because the obvious mappings are all *almost* right:
//!
//! * `File.rename` silently overwrites where AINL refuses.
//! * `FileUtils.mkdir_p` is quiet on an existing leaf where AINL errors — and
//!   it is a `require`, which the transpiler does not emit, so `_mkdir` builds
//!   the parents itself with `_fs_mkdir_p`.
//! * `File.lstat` is the right primitive (Ruby spells the non-following stat
//!   "lstat", not "lstatSync"), so the symlink rules come out for free.

use ainl_transpile::transpile_ruby_src;

fn rb(src: &str) -> String {
    transpile_ruby_src(src).unwrap_or_else(|e| panic!("transpile failed for `{src}`: {e}"))
}

#[test]
fn file_system_builtins_map_to_their_helpers() {
    let out = rb(r#"(do (mkdir "d" ":recursive")
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
        "_fs_mkdir_p",
    ] {
        assert!(
            out.contains(&format!("def {helper}(")),
            "missing helper {helper}:\n{out}"
        );
    }
}

#[test]
fn void_file_builtins_return_nil_ruby_returns_its_last_expression() {
    // The Ruby-specific trap, and the one bug this projection actually had.
    //
    // Ruby returns the value of its last expression, and Ruby's filesystem
    // calls return something: `Dir.mkdir` and `File.rename` return 0,
    // `File.write` and `IO.copy_stream` return a byte count. Left as the last
    // expression of a helper, that value becomes the AINL program's answer —
    // so `(print (write-file "a.txt" "A"))` printed `1` on ruby and `nil` on the
    // interpreter, the AOT binary and the python and js ports.
    //
    // The existing parity program never *prints* the result of these builtins,
    // which is why the divergence survived: a void builtin's value is invisible
    // unless something asks for it. `scripts/check-rmdir.sh` and the fixtures
    // close that gap, and this test pins the fix at the source.
    // `file-size` is in the program only so its helper is emitted — it is the
    // one filesystem builtin that returns a value, and its exception below is
    // checked against the same output.
    let out = rb(
        r#"(do (mkdir "d") (write-file "a" "A") (append-file "a" "B")
             (rename "a" "b") (copy "b" "c") (delete-file "c") (rmdir "d")
             (file-size "b"))"#,
    );
    for helper in [
        "_write_file",
        "_append_file",
        "_mkdir",
        "_rename",
        "_copy",
        "_delete_file",
        "_rmdir",
    ] {
        let body = out
            .split(&format!("def {helper}("))
            .nth(1)
            .unwrap_or_else(|| panic!("missing helper {helper}:\n{out}"))
            // Stop at the next `def` so the trailing `nil` is checked in the
            // right body and not in whatever follows.
            .split("\ndef ")
            .next()
            .unwrap();
        // The last statement before the closing `end` must be a bare `nil`.
        // Not the last line: the body's final line is always `end`, and the
        // explicit nil sits above it. Comparing whole lines (rather than
        // `body.trim_end().ends_with("nil")`) is what makes "a bare nil" mean
        // what it says — a body ending in a call whose name happens to end in
        // "nil" would otherwise pass.
        let last = body
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty() && *l != "end")
            .unwrap_or("");
        assert_eq!(
            last, "nil",
            "{helper} must end in an explicit nil so Ruby's last-expression \
             return cannot leak a host value; its last statement is `{last}`:\n{body}"
        );
    }
    // The one exception, and it is not an oversight: file-size returns a number
    // on every backend, so its st.size must stay the last expression.
    //
    // Truncated at the next `def` for the same reason as the loop above —
    // otherwise the "last statement" is looked for in whatever helper follows.
    let size = out
        .split("def _file_size(")
        .nth(1)
        .expect("_file_size must be emitted")
        .split("\ndef ")
        .next()
        .unwrap();
    let size_last = size
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty() && *l != "end")
        .unwrap_or("");
    assert_eq!(
        size_last, "st.size",
        "file-size returns a value, so it must not end in nil:\n{size}"
    );
}

#[test]
fn rmdir_never_reaches_fileutils_and_names_the_blocking_entry() {
    // FileUtils.rm_rf is quiet about a failure it cannot perform, so a partial
    // delete would look complete — and it is a `require`, which the transpiler
    // does not emit. The walk is hand-rolled, and it lstats so a symlink is
    // unlinked rather than descended into.
    //
    // The refusal names the entry, sorted by `n.b` (byte order) — the same key
    // _list_dir uses, because Dir.children returns filesystem order.
    let out = rb(r#"(rmdir "d" ":recursive")"#);
    assert!(
        !out.contains("FileUtils"),
        "the host's recursive delete must not be reached:\n{out}"
    );
    assert!(
        out.contains("sort_by { |n| n.b }"),
        "the named entry must be the first in byte order:\n{out}"
    );
    assert!(
        out.contains("it is not empty (#{names[0]})"),
        "the refusal must name the entry:\n{out}"
    );
    // The walk is post-order, and `st.directory? && !st.symlink?` is what keeps
    // a link from being descended into.
    let walk = out
        .split("def _fs_rm_tree(")
        .nth(1)
        .expect("_fs_rm_tree must be emitted");
    assert!(
        walk.contains("File.lstat(c)"),
        "the walk must lstat, not stat:\n{out}"
    );
    assert!(
        walk.contains("st.directory? && !st.symlink?"),
        "a symlink must be unlinked, not descended into:\n{out}"
    );
    let recurse = walk.find("_fs_rm_tree(c)").expect("the recursion");
    let rmdir = walk.find("Dir.rmdir(p)").expect("the final rmdir");
    assert!(recurse < rmdir, "children before parents:\n{out}");
}

#[test]
fn rmdir_refuses_a_symlink_and_owns_its_arity() {
    // File.directory? follows a symlink, so `st.symlink? || !st.directory?` is
    // the whole difference between "unlink the link" and "delete the target".
    // And the arity guard is inside the helper with *args: a Ruby
    // ArgumentError is not an AINL error, so it would escape a `catch`.
    let out = rb(r#"(rmdir "d")"#);
    assert!(
        out.contains("st.symlink? || !st.directory?"),
        "a symlink must not be treated as a directory:\n{out}"
    );
    assert!(
        out.contains("def _rmdir(*args)"),
        "rmdir must take *args so arity is AINL's error:\n{out}"
    );
    assert!(
        out.contains("it does not exist") && out.contains("it is not a directory"),
        "both refusals must be AINL's own text:\n{out}"
    );
}

#[test]
fn rename_refuses_an_existing_destination_before_calling_file_rename() {
    // File.rename overwrites. The destination lstat must precede the call.
    let out = rb(r#"(rename "a" "b")"#);
    let check = out
        .find("File.lstat(_fs_probe(dst))")
        .expect("the destination check");
    let call = out.find("File.rename(src, dst)").expect("the rename call");
    assert!(check < call, "the check must precede File.rename:\n{out}");
    assert!(out.contains("\"rename: cannot move '#{src}': '#{dst}' exists\""));
    assert!(out.contains("it does not exist"), "got:\n{out}");
    // Errno::EXDEV gets its own message rather than the generic one.
    assert!(out.contains("rescue Errno::EXDEV"), "got:\n{out}");
    assert!(out.contains("different filesystems"), "got:\n{out}");
}

#[test]
fn mkdir_uses_dir_mkdir_and_never_file_utils() {
    // FileUtils.mkdir_p is quiet on an existing leaf, is a `require` the
    // transpiler does not emit, and would need the whole stdlib for one call.
    // `_fs_mkdir_p` builds only the *parents* — the leaf itself goes through
    // Dir.mkdir, so the leaf's refusal is the host's own.
    let out = rb(r#"(mkdir "d" ":recursive")"#);
    // The word may appear in a comment explaining why it is *not* used, so
    // what is asserted is that nothing calls it and that nothing requires it.
    for forbidden in [
        "FileUtils.",
        "FileUtils::",
        "require 'fileutils'",
        "require \"fileutils\"",
    ] {
        assert!(
            !out.contains(forbidden),
            "FileUtils must not be called or required ({forbidden}):\n{out}"
        );
    }
    assert!(
        out.contains("Dir.mkdir(path)"),
        "the leaf uses Dir.mkdir:\n{out}"
    );
    assert!(
        out.contains("_fs_mkdir_p(parent)"),
        "the parents are built by the local helper:\n{out}"
    );
    // And the existence check comes first, so mkdir_p is never reached on a
    // path that already exists.
    let check = out
        .find("File.lstat(_fs_probe(path))")
        .expect("the existence check");
    let parent = out.find("_fs_mkdir_p(parent)").expect("the parent build");
    assert!(check < parent, "the check must precede mkdir_p:\n{out}");
    assert!(out.contains("it exists"), "got:\n{out}");
}

#[test]
fn the_parents_helper_tolerates_a_race_but_not_an_error() {
    // _fs_mkdir_p creates one component at a time and swallows EEXIST only
    // when the component is in fact a directory. Anything else re-raises, so a
    // genuine permission failure is reported by the caller's rescue rather
    // than silently skipped.
    let out = rb(r#"(mkdir "d" ":recursive")"#);
    assert!(
        out.contains("raise unless File.directory?(cur)"),
        "got:\n{out}"
    );
    // A relative path starts from '', an absolute one from '/', and the guard
    // keeps a double slash out of the middle.
    assert!(
        out.contains("cur.start_with?('/') ? '/' : ''") || out.contains("path.start_with?('/')"),
        "the root must be handled:\n{out}"
    );
}

#[test]
fn the_five_builtins_all_use_lstat() {
    // File.lstat is the non-following stat. File.exist? and File.directory?
    // follow a symlink, so a symlink to a directory would read as a directory
    // here and as nil in the interpreter. File.lstat is used for the *probe*;
    // the answers come off the returned stat object, never from File.directory?
    // on the raw path.
    let out =
        rb(r#"(do (mkdir "d") (rename "a" "b") (copy "a" "b") (is-dir "p") (file-size "p"))"#);
    for who in ["_mkdir", "_rename", "_copy", "_is_dir", "_file_size"] {
        assert!(
            out.contains(&format!("def {who}(*")),
            "{who} must take *args so arity is AINL's error:\n{out}"
        );
    }
    assert!(
        !out.contains("File.exist?(path)"),
        "File.exist? follows a symlink:\n{out}"
    );
    assert!(
        out.contains("_fs_probe"),
        "the shared probe must be used:\n{out}"
    );
    // _fs_probe trims a trailing separator, guarding the root.
    assert!(out.contains("p.end_with?('/')"), "got:\n{out}");
    assert!(
        out.contains("p.length > 1"),
        "the root must survive:\n{out}"
    );
}

#[test]
fn is_dir_returns_nil_not_false() {
    // AINL has one falsy value, nil. `false` would make `(= (is-dir p) nil)`
    // false, so a caller could not tell "not a directory" from "no such path".
    let out = rb(r#"(is-dir "p")"#);
    assert!(out.contains("return nil"), "got:\n{out}");
    assert!(out.contains("st.directory? ? true : nil"), "got:\n{out}");
}

#[test]
fn file_size_uses_the_lstat_size_and_refuses_a_directory() {
    // POSIX reports a directory's inode size — 4096 on ext4, 60 on APFS, 0 on
    // tmpfs — so a naive File.size would answer a host-specific number instead
    // of an error.
    let out = rb(r#"(file-size "p")"#);
    assert!(
        out.contains("\"file-size: cannot read '#{path}': it is a directory\""),
        "a directory must be refused:\n{out}"
    );
    assert!(
        out.contains("st.size"),
        "the size comes from the lstat:\n{out}"
    );
}

#[test]
fn copy_streams_bytes_and_never_reads_them_into_a_string() {
    // IO.copy_stream is the byte-for-byte equivalent of the C runtime's
    // read/write loop, and keeps a large file out of memory. File.read would
    // also change nothing semantically here, but would blow up on a file
    // larger than memory — and, more importantly, would be a *reimplementation*
    // of file I/O rather than the host's idiom.
    let out = rb(r#"(copy "a" "b")"#);
    assert!(out.contains("IO.copy_stream(i, o)"), "got:\n{out}");
    assert!(out.contains("'rb'") && out.contains("'wb'"), "got:\n{out}");
    assert!(
        out.contains("\"copy: cannot copy '#{src}': it is a directory\""),
        "a directory copy must be refused by name:\n{out}"
    );
}

#[test]
fn arity_and_type_failures_are_ainl_errors_not_host_exceptions() {
    // A Ruby ArgumentError is not an AINL error, so it would escape an AINL
    // `catch` and print a Ruby backtrace to stderr, which the byte comparison
    // fails on.
    let out =
        rb(r#"(do (mkdir "d") (rename "a" "b") (copy "a" "b") (is-dir "p") (file-size "p"))"#);
    assert!(
        out.contains("def _error("),
        "_error must be emitted:\n{out}"
    );
    // The rendered type name is AINL's, not the host's Integer.
    assert!(out.contains("_ainl_tname"), "got:\n{out}");
    for who in [
        "mkdir: cannot create",
        "rename: cannot move",
        "copy: cannot copy",
        "file-size: cannot read",
    ] {
        assert!(
            out.contains(&format!("{who} '#{{")),
            "the {who} message must be AINL's own:\n{out}"
        );
    }
    // A host SystemCallError is caught and converted; an AINL error raised
    // inside the begin block must not be swallowed by the same rescue.
    assert!(
        out.contains("rescue SystemCallError"),
        "host errno must be caught:\n{out}"
    );
}
