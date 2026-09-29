//! `ainl pkg` — the package manager: manifests, a lockfile, and vendoring.
//!
//! AINL programs are single trees of `.ainl` files, so before this module the
//! only way to reuse code was an `import` path that the author wrote out by
//! hand and kept correct themselves. A package manager adds three artifacts and
//! five commands; everything else here follows from making them small.
//!
//! # The three artifacts
//!
//! - **`ainl.pkg`** — a manifest: `name`, `version`, `deps`. A plain
//!   `key: value` file, not JSON and not S-expressions, so a model can write
//!   and repair it with the same confidence as a `Makefile` line and it stays
//!   reviewable in a diff.
//! - **`.ainl-lock`** — the resolved, exact-pinned graph. Determinism lives
//!   here: same lockfile, same vendored tree, same binary.
//! - **`.ainl-vendor/<name>/`** — the *vendored* package source, checked in
//!   like any other source file.
//!
//! # The zero-dep rule, restated
//!
//! AINL's hardest guarantee is that a compiled program is a **standalone
//! binary** (see [`crate::import`]): no host runtime, no library path, no
//! lookup at run time. Vendoring is what keeps that true for packages. Nothing
//! is fetched when a program runs; `ainl pkg` copies sources into the tree at
//! build time, and the compiler inlines them, so the finished binary contains
//! the package code and needs nothing beside it. A program that ran correctly
//! but read its own dependencies off disk would break that promise, so the
//! *vendored copy* is the one that gets compiled — never a cache, never a
//! checkout elsewhere.
//!
//! # Local sources only, in Tier 3
//!
//! A dependency resolves from a **local path** or a **git URL cloned into a
//! local cache**. There is no central registry: publishing, naming, trust and
//! availability are a Tier 4+ decision, and guessing at them in a language
//! aimed at machine generation would build the wrong contract. What Tier 3
//! does guarantee is the hard part: a dependency named once resolves to a
//! pinned, vendored, verified source tree.

use crate::error::{Error, Result};
use crate::import::MODULE_EXT;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// The manifest file name, in every directory that is or contains a package.
pub const MANIFEST: &str = "ainl.pkg";

/// The lockfile name, at the project root.
pub const LOCKFILE: &str = ".ainl-lock";

/// The vendored source directory, at the project root.
pub const VENDOR_DIR: &str = ".ainl-vendor";

/// A package's own declared version, and the version a lockfile pins.
///
/// Exact versions only — no ranges, no `^`, no `~`. A range resolver is a
/// graph search with a preference order and a conflict policy, and both are
/// decisions about trust and surprise that belong to a later tier. Tier 3
/// resolves one answer: whatever the manifest names, checked byte-for-byte.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version(String);

impl Version {
    /// Build a version from its canonical text, rejecting the shapes Tier 3
    /// does not support rather than storing something it will not honour.
    pub fn new(text: &str) -> Result<Version> {
        let bad = |why: &str| {
            Err(Error::runtime(format!(
                "ainl pkg: '{text}' is not a version{why} — Tier 3 pins exact \
                 versions only (write '1.0.0', not '^1.0.0' or '>=1.0.0')"
            )))
        };
        if text.is_empty() {
            return bad("");
        }
        if text.starts_with('^')
            || text.starts_with('~')
            || text.starts_with('>')
            || text.starts_with('<')
            || text.starts_with('=')
            || text.contains('*')
            || text.contains(' ')
        {
            return bad(" (it looks like a range)");
        }
        // Dot-separated numeric identifiers: a deliberately tiny grammar, so
        // "1.0" and "1.0.0" and "1.0.0-beta" are all distinct *strings* and
        // there is no "is this newer" question to get wrong. Ordering is
        // lexicographic and is used only to make output stable, never to
        // resolve a conflict.
        let core = text.split(['-', '+']).next().unwrap_or(text);
        if core.is_empty()
            || !core
                .split('.')
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        {
            return bad(" (expected dot-separated numbers, e.g. '1.2.3')");
        }
        if text.chars().any(|c| c.is_whitespace()) {
            return bad(" (it contains whitespace)");
        }
        Ok(Version(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A package name.
///
/// Names are validated rather than trusted: a name becomes a *directory* under
/// `.ainl-vendor/`, so an unchecked one is a path-traversal vector
/// (`../../etc`) as well as a namespace collision. A package name is
/// `[a-z0-9][a-z0-9-]*`, lowercase on purpose — names are compared, sorted and
/// compared for equality by a machine far more often than they are read by a
/// person, and a case-insensitive filesystem would happily give you two
/// spellings of one package and one name that resolves on macOS and not on
/// Linux.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Name(String);

impl Name {
    pub fn new(text: &str) -> Result<Name> {
        let ok = !text.is_empty()
            && text.len() <= 64
            && text
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            && text
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !text.ends_with('-');
        if !ok {
            return Err(Error::runtime(format!(
                "ainl pkg: '{text}' is not a valid package name — use [a-z0-9-], \
                 starting with a letter or digit and not ending with '-'"
            )));
        }
        Ok(Name(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a dependency's source comes from.
///
/// Tier 3 has exactly two: a directory on this machine, or a git URL cloned
/// into a local cache. There is deliberately no "latest" and no registry
/// index — see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A path on this machine, relative to the project root.
    Path(PathBuf),
    /// A git URL, pinned to an exact commit by the lockfile.
    Git { url: String, rev: String },
}

impl Source {
    /// Parse the source text from a manifest or lockfile.
    pub fn parse(text: &str) -> Result<Source> {
        let t = text.trim();
        if t.is_empty() {
            return Err(Error::runtime("ainl pkg: empty source"));
        }
        if let Some(rest) = t.strip_prefix("git:") {
            let rest = rest.trim();
            let (url, rev) = match rest.split_once('@') {
                // `git:URL` alone is legal here and means "the branch the
                // lockfile pins", but `get` requires the rev up front so the
                // pin is chosen by the command that fetches, not by whatever
                // the branch happened to be at resolve time.
                Some((u, r)) if !r.is_empty() => (u.trim().to_string(), r.trim().to_string()),
                _ => {
                    return Err(Error::runtime(format!(
                        "ainl pkg: git source '{t}' needs a revision — write \
                         git:<url>@<rev> (an exact commit), so the dependency is \
                         pinned rather than following a moving branch"
                    )))
                }
            };
            if url.is_empty() || rev.is_empty() {
                return Err(Error::runtime(format!(
                    "ainl pkg: git source '{t}' needs both a url and a revision"
                )));
            }
            if rev.contains(char::is_whitespace) {
                return Err(Error::runtime(format!(
                    "ainl pkg: git revision '{rev}' contains whitespace"
                )));
            }
            return Ok(Source::Git { url, rev });
        }
        if t.contains("://") {
            return Err(Error::runtime(format!(
                "ainl pkg: '{t}' is not a source this tier can resolve — Tier 3 \
                 resolves local paths (./dir, ../dir, /abs) and git: URLs only. \
                 There is no package registry yet; that is a later, separate decision."
            )));
        }
        Ok(Source::Path(PathBuf::from(t)))
    }

    /// The canonical text form, as written in a manifest or lockfile.
    pub fn to_text(&self) -> String {
        match self {
            Source::Path(p) => p.display().to_string(),
            Source::Git { url, rev } => format!("git:{url}@{rev}"),
        }
    }
}

/// A dependency edge: a package this one needs, at an exact version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dep {
    pub name: Name,
    pub version: Version,
    pub source: Source,
}

/// A parsed `ainl.pkg`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub name: Name,
    pub version: Version,
    pub deps: Vec<Dep>,
}

/// Parse manifest text.
///
/// The grammar is intentionally not a general format parser. It is
/// key/value lines with `#` comments, because a manifest is written by hand,
/// by a model, and by a script in roughly equal measure, and a format that
/// cannot represent a comment in the middle of a dependency list is a format
/// that will be commented around instead. Every accepted shape is one a person
/// would guess; everything else is an error naming the line.
pub fn parse_manifest(text: &str) -> Result<Manifest> {
    let mut name: Option<Name> = None;
    let mut version: Option<Version> = None;
    let mut deps: Vec<Dep> = Vec::new();
    let mut dep_name: Option<Name> = None;
    let mut dep_version: Option<Version> = None;
    let mut dep_source: Option<Source> = None;
    let mut in_deps = false;

    let flush = |deps: &mut Vec<Dep>,
                 dn: &mut Option<Name>,
                 dv: &mut Option<Version>,
                 ds: &mut Option<Source>,
                 line: usize|
     -> Result<()> {
        let Some(n) = dn.take() else {
            if dv.is_some() || ds.is_some() {
                return Err(Error::runtime(format!(
                    "ainl pkg: {MANIFEST} line {line}: 'version'/'source' inside \
                     a [deps] block needs a 'name' first"
                )));
            }
            return Ok(());
        };
        let v = dv.take().ok_or_else(|| {
            Error::runtime(format!(
                "ainl pkg: {MANIFEST} line {line}: dependency '{n}' has no 'version' — \
                 Tier 3 pins exact versions"
            ))
        })?;
        let s = ds.take().ok_or_else(|| {
            Error::runtime(format!(
                "ainl pkg: {MANIFEST} line {line}: dependency '{n}' has no 'source' \
                 (a local path, or git:<url>@<rev>)"
            ))
        })?;
        deps.push(Dep {
            name: n,
            version: v,
            source: s,
        });
        Ok(())
    };

    for (idx, raw) in text.lines().enumerate() {
        let line = idx + 1;
        let line_no_comment = strip_comment(raw);
        let t = line_no_comment.trim();
        if t.is_empty() {
            continue;
        }
        // `[deps]` opens the dependency block; a new top-level key closes it.
        if t == "[deps]" {
            in_deps = true;
            continue;
        }
        if t.starts_with('[') {
            return Err(Error::runtime(format!(
                "ainl pkg: {MANIFEST} line {line}: unknown section '{t}' (the only \
                 section is [deps])"
            )));
        }
        let (key, value) = t.split_once(':').ok_or_else(|| {
            Error::runtime(format!(
                "ainl pkg: {MANIFEST} line {line}: expected 'key: value', got '{t}'"
            ))
        })?;
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if in_deps {
            match key.as_str() {
                "name" => {
                    flush(
                        &mut deps,
                        &mut dep_name,
                        &mut dep_version,
                        &mut dep_source,
                        line,
                    )?;
                    dep_name = Some(Name::new(value)?);
                }
                "version" => {
                    if dep_name.is_none() {
                        return Err(Error::runtime(format!(
                            "ainl pkg: {MANIFEST} line {line}: 'version' before 'name' \
                             inside [deps]"
                        )));
                    }
                    dep_version = Some(Version::new(value)?);
                }
                "source" => {
                    if dep_name.is_none() {
                        return Err(Error::runtime(format!(
                            "ainl pkg: {MANIFEST} line {line}: 'source' before 'name' \
                             inside [deps]"
                        )));
                    }
                    dep_source = Some(Source::parse(value)?);
                }
                other => {
                    return Err(Error::runtime(format!(
                        "ainl pkg: {MANIFEST} line {line}: unknown key '{other}' inside \
                         [deps] (name, version, source)"
                    )))
                }
            }
        } else {
            match key.as_str() {
                "name" => name = Some(Name::new(value)?),
                "version" => version = Some(Version::new(value)?),
                other => {
                    return Err(Error::runtime(format!(
                        "ainl pkg: {MANIFEST} line {line}: unknown key '{other}' at the top \
                         level (name, version)"
                    )))
                }
            }
        }
    }
    flush(
        &mut deps,
        &mut dep_name,
        &mut dep_version,
        &mut dep_source,
        text.lines().count(),
    )?;

    let name =
        name.ok_or_else(|| Error::runtime(format!("ainl pkg: {MANIFEST} has no 'name' line")))?;
    let version = version
        .ok_or_else(|| Error::runtime(format!("ainl pkg: {MANIFEST} has no 'version' line")))?;
    let mut seen: HashSet<Name> = HashSet::new();
    for d in &deps {
        if !seen.insert(d.name.clone()) {
            return Err(Error::runtime(format!(
                "ainl pkg: {MANIFEST} lists dependency '{}' twice — one version per \
                 name, since nothing resolves a range",
                d.name
            )));
        }
    }
    Ok(Manifest {
        name,
        version,
        deps,
    })
}

/// Drop a `#` comment, honouring quotes so a `#` inside a value survives.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_str = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => in_str = !in_str,
            b'\\' if in_str => i += 1,
            b'#' if !in_str => return &line[..i],
            _ => {}
        }
        i += 1;
    }
    line
}

/// The text `ainl pkg init` writes.
pub fn manifest_template(name: &Name) -> String {
    format!(
        "# {MANIFEST} — this project's AINL package manifest.\n\
         #\n\
         # name/version identify the project. [deps] lists what it needs; each\n\
         # dependency is pinned to one exact version and resolved from a local\n\
         # path or a git: URL. There is no registry — see docs/PACKAGING.md.\n\
         #\n\
         #   ainl pkg get <name>@<version> <source>   add and vendor a dep\n\
         #   ainl pkg install                         re-vendor what the lockfile pins\n\
         #   ainl pkg list                            show the resolved graph\n\
         #   ainl pkg verify                          check the vendor dir against the lock\n\
         \n\
         name: {name}\n\
         version: 0.1.0\n\
         \n\
         [deps]\n"
    )
}

// ---------------------------------------------------------------------------
// Lockfile
// ---------------------------------------------------------------------------

/// One resolved package, as pinned in `.ainl-lock`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Locked {
    pub name: Name,
    pub version: Version,
    pub source: Source,
    /// The packages this one depends on, by name+version.
    pub deps: Vec<(Name, Version)>,
    /// Per-file digests of the vendored source, name -> digest.
    ///
    /// This is what makes `verify` a *tamper* check rather than a presence
    /// check. Digests cover the vendored bytes, so an edited, truncated or
    /// swapped package fails even when every file is still there — the failure
    /// mode a bare "does the file exist" check cannot see, and the one that
    /// actually matters in CI.
    pub files: BTreeMap<String, String>,
}

/// A parsed `.ainl-lock`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Lock {
    /// The root package this lock belongs to, for a clear error if it is read
    /// next to the wrong manifest.
    pub root: Option<(Name, Version)>,
    pub packages: Vec<Locked>,
}

impl Lock {
    /// Look a locked package up by name+version.
    pub fn get(&self, name: &Name, version: &Version) -> Option<&Locked> {
        self.packages
            .iter()
            .find(|p| &p.name == name && &p.version == version)
    }
}

impl std::fmt::Display for Lock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "# {LOCKFILE} — resolved AINL packages. Generated by `ainl pkg`; edit the"
        )
        .unwrap();
        writeln!(
            f,
            "# manifests, not this file. Every version here is exact and every"
        )
        .unwrap();
        writeln!(f, "# digest covers the vendored bytes under {VENDOR_DIR}/.").unwrap();
        if let Some((n, v)) = &self.root {
            writeln!(f, "root: {n} {v}")?;
        }
        for p in sorted(&self.packages) {
            writeln!(f, "\npackage: {} {}", p.name, p.version)?;
            writeln!(f, "source: {}", p.source.to_text())?;
            for d in &p.deps {
                writeln!(f, "dep: {} {}", d.0, d.1)?;
            }
            for (file, digest) in &p.files {
                writeln!(f, "file: {file} {digest}")?;
            }
        }
        Ok(())
    }
}

/// A package ordering that is independent of discovery order, so two runs that
/// resolved the same graph print byte-identical files. `Vec::sort` on the
/// derived `Ord` is lexicographic on (name, version), which is exactly the
/// tie-break the lockfile format documents.
fn sorted(packages: &[Locked]) -> Vec<Locked> {
    let mut out = packages.to_vec();
    out.sort();
    out
}

impl Ord for Locked {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (&self.name, &self.version).cmp(&(&other.name, &other.version))
    }
}

impl PartialOrd for Locked {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Parse lockfile text. The inverse of the `Display` above.
pub fn parse_lock(text: &str) -> Result<Lock> {
    let mut root = None;
    let mut packages: Vec<Locked> = Vec::new();
    let mut cur: Option<Locked> = None;
    for (idx, raw) in text.lines().enumerate() {
        let line = idx + 1;
        let t = strip_comment(raw).trim();
        if t.is_empty() {
            continue;
        }
        // The colon is part of the key, not a separator: every directive in
        // this format ends with one, and stripping it here is what makes
        // `parse_lock` an exact inverse of the `Display` above. Splitting on
        // whitespace alone (rather than ':') keeps a value containing a colon —
        // a git URL's `https://`, most obviously — intact.
        let (key, value) = t.split_once(' ').unwrap_or((t, ""));
        let value = value.trim();
        let key = key.strip_suffix(':').unwrap_or(key);
        match key {
            "root" => {
                let (n, v) = value.split_once(' ').ok_or_else(|| {
                    Error::runtime(format!(
                        "ainl pkg: {LOCKFILE} line {line}: 'root' needs a name and a version"
                    ))
                })?;
                root = Some((Name::new(n.trim())?, Version::new(v.trim())?));
                continue;
            }
            "package" => {
                if let Some(p) = cur.take() {
                    packages.push(p);
                }
                let (n, v) = value.split_once(' ').ok_or_else(|| {
                    Error::runtime(format!(
                        "ainl pkg: {LOCKFILE} line {line}: 'package' needs a name and a version"
                    ))
                })?;
                cur = Some(Locked {
                    name: Name::new(n.trim())?,
                    version: Version::new(v.trim())?,
                    source: Source::parse(".")?, // replaced by the required 'source' line
                    deps: Vec::new(),
                    files: BTreeMap::new(),
                });
            }
            "source" => {
                let p = cur.as_mut().ok_or_else(|| {
                    Error::runtime(format!(
                        "ainl pkg: {LOCKFILE} line {line}: 'source' outside a package block"
                    ))
                })?;
                p.source = Source::parse(value)?;
            }
            "dep" => {
                let p = cur.as_mut().ok_or_else(|| {
                    Error::runtime(format!(
                        "ainl pkg: {LOCKFILE} line {line}: 'dep' outside a package block"
                    ))
                })?;
                let (n, v) = value.split_once(' ').ok_or_else(|| {
                    Error::runtime(format!(
                        "ainl pkg: {LOCKFILE} line {line}: 'dep' needs a name and a version"
                    ))
                })?;
                p.deps.push((Name::new(n.trim())?, Version::new(v.trim())?));
            }
            "file" => {
                let p = cur.as_mut().ok_or_else(|| {
                    Error::runtime(format!(
                        "ainl pkg: {LOCKFILE} line {line}: 'file' outside a package block"
                    ))
                })?;
                let (f, d) = value.split_once(' ').ok_or_else(|| {
                    Error::runtime(format!(
                        "ainl pkg: {LOCKFILE} line {line}: 'file' needs a path and a digest"
                    ))
                })?;
                p.files.insert(f.trim().to_string(), d.trim().to_string());
            }
            other => {
                return Err(Error::runtime(format!(
                    "ainl pkg: {LOCKFILE} line {line}: unknown key '{other}' \
                     (root, package, source, dep, file)"
                )))
            }
        }
    }
    if let Some(p) = cur.take() {
        packages.push(p);
    }
    Ok(Lock { root, packages })
}

// ---------------------------------------------------------------------------
// Digests
// ---------------------------------------------------------------------------

/// A dependency-free SHA-256, used to pin vendored bytes.
///
/// The zero-dependency rule is the reason this is implemented here rather than
/// pulled in: `ainl` ships as a single static binary, and a package manager
/// whose install pulls a crate graph would be the one part of the toolchain
/// that cannot be audited by reading it. A digest is also not decoration — it
/// is what `ainl pkg verify` compares, so a tampered vendor dir is detected
/// by content, not by file count.
pub mod digest {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    /// Lowercase hex SHA-256 of `data`.
    pub fn sha256_hex(data: &[u8]) -> String {
        let mut h: [u32; 8] = [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
            0x5be0cd19,
        ];
        let mut msg = data.to_vec();
        let bitlen = (data.len() as u64).wrapping_mul(8);
        msg.push(0x80);
        while msg.len() % 64 != 56 {
            msg.push(0);
        }
        msg.extend_from_slice(&bitlen.to_be_bytes());

        let mut w = [0u32; 64];
        // Fixed 64-byte compression windows.
        //
        // The loop is indexed rather than `for chunk in ...chunks_exact(64)`,
        // because a recent clippy asks for the (still unstable) `as_chunks` on
        // any fixed-size `chunks_exact` walk, and switching to it would break
        // the build on the older toolchain the workspace also supports.
        // Suppressing that lint with `#[allow]` is not an option either:
        // `unknown_lints` is itself denied under `-D warnings`, so naming a lint
        // this toolchain has never heard of turns a clean build into a hard
        // error. Reading `w` by index satisfies both toolchains and is what the
        // spec describes. The padding above guarantees the length is a whole
        // multiple of 64, so no window is ever dropped.
        let mut block = 0usize;
        while block * 64 < msg.len() {
            let chunk = &msg[block * 64..block * 64 + 64];
            for i in 0..16 {
                w[i] = u32::from_be_bytes([
                    chunk[i * 4],
                    chunk[i * 4 + 1],
                    chunk[i * 4 + 2],
                    chunk[i * 4 + 3],
                ]);
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[i - 7])
                    .wrapping_add(s1);
            }
            let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
                (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
            for i in 0..64 {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let ch = (e & f) ^ ((!e) & g);
                let t1 = hh
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(K[i])
                    .wrapping_add(w[i]);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let maj = (a & b) ^ (a & c) ^ (b & c);
                let t2 = s0.wrapping_add(maj);
                hh = g;
                g = f;
                f = e;
                e = d.wrapping_add(t1);
                d = c;
                c = b;
                b = a;
                a = t1.wrapping_add(t2);
            }
            for (i, v) in [a, b, c, d, e, f, g, hh].iter().enumerate() {
                h[i] = h[i].wrapping_add(*v);
            }
            block += 1;
        }
        h.iter().map(|w| format!("{w:08x}")).collect()
    }
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// The project root: the nearest ancestor holding an `ainl.pkg`.
pub fn find_project_root(start: &Path) -> Result<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if d.join(MANIFEST).is_file() {
            return Ok(d.to_path_buf());
        }
        dir = d.parent();
    }
    Err(Error::runtime(format!(
        "ainl pkg: no {MANIFEST} here or in any parent directory — \
         run `ainl pkg init` in the project root first"
    )))
}

/// Every `.ainl` file in a package directory, as root-relative paths, sorted.
///
/// Sorted, and excluding `.ainl-vendor/`, so a package that itself vendors a
/// dependency does not drag that dependency's vendored copy into its own
/// digests — the transitive package is vendored once, at the top level, where
/// its own lock entry owns it.
pub fn package_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_ainl(root, root, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect_ainl(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| Error::runtime(format!("ainl pkg: cannot read {}: {e}", dir.display())))?;
    let mut names: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    names.sort();
    for path in names {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name == VENDOR_DIR || name == ".git" {
            continue;
        }
        if path.is_dir() {
            collect_ainl(root, &path, out)?;
        } else if path.extension().map(|e| e == "ainl").unwrap_or(false) {
            let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            out.push(rel);
        }
    }
    Ok(())
}

/// Read a package's manifest, requiring it to exist.
pub fn read_manifest(dir: &Path) -> Result<Manifest> {
    let path = dir.join(MANIFEST);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| Error::runtime(format!("ainl pkg: cannot read {}: {e}", path.display())))?;
    parse_manifest(&text)
}

/// The resolved graph, and the order it was resolved in.
#[derive(Debug, Default)]
pub struct Resolution {
    pub packages: Vec<Locked>,
    /// `(from, to)` edges, in discovery order.
    pub edges: Vec<(Name, Name)>,
}

/// Resolve a manifest's full dependency graph from local sources.
///
/// DFS with an explicit stack, so the error a cycle produces names the actual
/// cycle (`a -> b -> a`) rather than "dependency cycle detected" — the same
/// shape as the `import` cycle message in [`crate::import`], deliberately, so
/// one idea has one error text in this language.
///
/// Resolution is *exact*: a dep is satisfied by the version its own manifest
/// declares. A graph with two different versions of one package is refused
/// rather than picked between, because choosing is a policy and a policy the
/// user cannot see is exactly the silent-wrong this language refuses to ship.
pub fn resolve(root: &Path, manifest: &Manifest) -> Result<Resolution> {
    let mut out = Resolution::default();
    // Canonical dir -> locked package, so a package reached twice is read once.
    let mut by_dir: HashMap<PathBuf, Name> = HashMap::new();
    // name+version -> dir, the one place a version identity becomes a path.
    let mut chosen: HashMap<(Name, Version), PathBuf> = HashMap::new();
    let mut stack: Vec<(Name, PathBuf)> = Vec::new();
    let mut on_stack: HashSet<Name> = HashSet::new();

    // Resolved paths are recorded *relative to the project root*, so a lockfile
    // written on one machine's absolute path layout still matches another's.
    // That is the whole reason `root` is threaded down rather than the base
    // directory each package happened to be reached from.
    resolve_dir(
        root,
        root,
        manifest,
        &mut out,
        &mut by_dir,
        &mut chosen,
        &mut stack,
        &mut on_stack,
    )?;
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn resolve_dir(
    root: &Path,
    base: &Path,
    manifest: &Manifest,
    out: &mut Resolution,
    by_dir: &mut HashMap<PathBuf, Name>,
    chosen: &mut HashMap<(Name, Version), PathBuf>,
    stack: &mut Vec<(Name, PathBuf)>,
    on_stack: &mut HashSet<Name>,
) -> Result<()> {
    let dir = base.to_path_buf();
    let key = (manifest.name.clone(), manifest.version.clone());

    // A package already resolved from this exact directory: nothing to redo.
    // This is what makes a diamond (a and b both need c) read c once.
    if by_dir.contains_key(&dir) {
        return Ok(());
    }
    // Cycle: this name is already on the resolution path.
    if on_stack.contains(&manifest.name) {
        let mut cycle: Vec<String> = Vec::new();
        let start = stack
            .iter()
            .position(|(n, _)| n == &manifest.name)
            .unwrap_or(0);
        for (n, _) in &stack[start..] {
            cycle.push(n.to_string());
        }
        cycle.push(manifest.name.to_string());
        return Err(Error::runtime(format!(
            "ainl pkg: circular dependency — {} depends on itself ({})",
            manifest.name,
            cycle.join(" -> ")
        )));
    }

    // Two versions of one package in one graph: refused, not resolved.
    //
    // Same name *and* version from two directories is a benign duplicate — two
    // paths to one package — and the first one read wins. Two *versions* is the
    // conflict case, and it is refused rather than picked between: choosing is
    // a policy, and a policy the user cannot see is the silent-wrong this
    // language refuses to ship.
    if let Some(((_, other_version), other_dir)) =
        chosen.iter().find(|((n, _), _)| n == &manifest.name)
    {
        if other_version != &key.1 {
            return Err(Error::runtime(format!(
                "ainl pkg: '{}' is required at two different versions in one graph \
                 ({} from {}, {} from {}) — Tier 3 resolves exactly one version per \
                 package name, so align the manifests",
                manifest.name,
                other_version,
                display_path(other_dir),
                key.1,
                display_path(&dir)
            )));
        }
    }
    chosen.insert(key.clone(), dir.clone());
    by_dir.insert(dir.clone(), manifest.name.clone());

    let files = package_files(&dir)?;
    let mut digests = BTreeMap::new();
    for rel in &files {
        let bytes = std::fs::read(dir.join(rel)).map_err(|e| {
            Error::runtime(format!(
                "ainl pkg: cannot read {}: {e}",
                dir.join(rel).display()
            ))
        })?;
        digests.insert(rel.display().to_string(), digest::sha256_hex(&bytes));
    }

    on_stack.insert(manifest.name.clone());
    stack.push((manifest.name.clone(), dir.clone()));
    let mut dep_ids = Vec::new();
    for dep in &manifest.deps {
        let dep_dir = source_dir(base, &dep.source)?;
        let dep_manifest = read_manifest(&dep_dir)?;
        // The dep's *manifest* is the authority on its own name and version;
        // the dep edge only said where to find it. A disagreement means the
        // graph the user wrote is not the graph on disk, and silently
        // preferring one of them is how a lockfile starts lying.
        if dep_manifest.name != dep.name {
            return Err(Error::runtime(format!(
                "ainl pkg: dependency '{}' in {} is named '{}' in its own {MANIFEST}",
                dep.name,
                display_path(&dep_dir),
                dep_manifest.name
            )));
        }
        if dep_manifest.version != dep.version {
            return Err(Error::runtime(format!(
                "ainl pkg: dependency '{}' is pinned at {} but its {MANIFEST} says {}",
                dep.name, dep.version, dep_manifest.version
            )));
        }
        out.edges.push((manifest.name.clone(), dep.name.clone()));
        dep_ids.push((dep.name.clone(), dep.version.clone()));
        resolve_dir(
            root,
            &dep_dir,
            &dep_manifest,
            out,
            by_dir,
            chosen,
            stack,
            on_stack,
        )?;
    }
    stack.pop();
    on_stack.remove(&manifest.name);

    out.packages.push(Locked {
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        source: Source::Path(dir.strip_prefix(root).unwrap_or(&dir).to_path_buf()),
        deps: dep_ids,
        files: digests,
    });
    Ok(())
}

fn display_path(p: &Path) -> String {
    p.display().to_string()
}

/// The directory a source points at, resolved relative to `base`.
///
/// A git source resolves through the **local clone cache**, so resolution
/// never reaches the network: `ainl pkg get` is the only command that fetches,
/// and it populates the cache. `install` and `verify` then work offline from
/// the cache, which is what makes a CI runner that has never seen the package
/// able to fail with a readable message instead of a hang.
pub fn source_dir(base: &Path, source: &Source) -> Result<PathBuf> {
    match source {
        Source::Path(p) => {
            if p.is_absolute() {
                Ok(p.clone())
            } else {
                Ok(base.join(p))
            }
        }
        Source::Git { url, rev } => clone_cache_dir(url, rev),
    }
}

/// Where a git dependency's clone lives. Pure path arithmetic — no I/O — so
/// the same rev always maps to the same directory.
pub fn clone_cache_dir(url: &str, rev: &str) -> Result<PathBuf> {
    let key = digest::sha256_hex(format!("{url}@{rev}").as_bytes());
    let cache = cache_root()?.join("git").join(&key[..16]);
    if !cache.exists() {
        return Err(Error::runtime(format!(
            "ainl pkg: git dependency {url}@{rev} is not in the local clone cache \
             ({}).\nRun `ainl pkg get {url}@{rev}` once, with network access, to \
             populate it. Resolution itself never touches the network.",
            cache.display()
        )));
    }
    Ok(cache)
}

/// The clone cache root, `~/.ainl/packages` by default.
pub fn cache_root() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("AINL_PKG_CACHE") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var("HOME").map_err(|_| {
        Error::runtime(
            "ainl pkg: no HOME set and no AINL_PKG_CACHE — set AINL_PKG_CACHE to \
             choose where git clones are cached",
        )
    })?;
    Ok(PathBuf::from(home).join(".ainl").join("packages"))
}

/// Clone a git dependency into the local cache at an exact revision.
///
/// `git` is invoked directly rather than through a git library: the toolchain
/// is zero-dependency, and the only thing asked of git is a checkout of one
/// pinned commit.
pub fn git_clone(url: &str, rev: &str) -> Result<PathBuf> {
    let key = digest::sha256_hex(format!("{url}@{rev}").as_bytes());
    let dest = cache_root()?.join("git").join(&key[..16]);
    if dest.exists() {
        return Ok(dest);
    }
    let dest_parent = dest
        .parent()
        .ok_or_else(|| Error::runtime("ainl pkg: cannot locate the clone cache directory"))?
        .to_path_buf();
    std::fs::create_dir_all(&dest_parent).map_err(|e| {
        Error::runtime(format!(
            "ainl pkg: cannot create {}: {e}",
            dest_parent.display()
        ))
    })?;
    // A temp dir then a rename, so an interrupted clone cannot leave a
    // half-populated cache entry that later reads as a valid checkout.
    let tmp = dest.with_extension("partial");
    let _ = std::fs::remove_dir_all(&tmp);
    let run = |args: &[&str]| -> Result<()> {
        let out = std::process::Command::new("git")
            .args(args)
            .output()
            .map_err(|e| {
                Error::runtime(format!(
                    "ainl pkg: cannot run `git {}` ({e}) — a git: dependency needs \
                     the git command on PATH",
                    args.join(" ")
                ))
            })?;
        if !out.status.success() {
            return Err(Error::runtime(format!(
                "ainl pkg: `git {}` failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    };
    run(&["clone", "--quiet", url, &tmp.to_string_lossy()])?;
    run(&["-C", &tmp.to_string_lossy(), "checkout", "--quiet", rev])?;
    std::fs::rename(&tmp, &dest)
        .map_err(|e| Error::runtime(format!("ainl pkg: cannot finalize the clone cache: {e}")))?;
    Ok(dest)
}

// ---------------------------------------------------------------------------
// Vendoring
// ---------------------------------------------------------------------------

/// What a resolution produced, ready to be written.
pub struct Vendored {
    pub lock: Lock,
    /// Human-readable lines for the CLI.
    pub notes: Vec<String>,
}

/// Resolve `manifest` and vendor every package into `<root>/{VENDOR_DIR}/`.
///
/// Vendoring is a **copy**, not a link or a reference. That is the whole
/// mechanism behind the standalone-binary guarantee: the bytes a build
/// compiles are in the tree, checked in, diffable, and hashed by the lockfile,
/// so a build never depends on a cache, a checkout, or a network that may be
/// gone by the time someone runs the artifact.
pub fn vendor(root: &Path, manifest: &Manifest) -> Result<Vendored> {
    let resolution = resolve(root, manifest)?;
    let vendor_root = root.join(VENDOR_DIR);
    // A stale vendor dir would leave a package from a removed dep compiled
    // into the program, so the dir is rebuilt rather than merged. Rebuilt,
    // not deleted blindly: only the subdirectories the lockfile names are
    // removed, and a name in the vendor dir that is not a package cannot be
    // mistaken for one.
    prepare_vendor_dir(&vendor_root, &resolution)?;

    let mut lock = Lock {
        root: Some((manifest.name.clone(), manifest.version.clone())),
        packages: Vec::new(),
    };
    let mut notes = Vec::new();
    for pkg in sorted(&resolution.packages) {
        // The root package is the project itself. It is not a dependency of
        // itself, and vendoring it into `.ainl-vendor/<name>/` would put a
        // second copy of every one of the project's own files inside its own
        // tree — which `import` would then be able to resolve *twice*, binding
        // the same names from two paths. The lockfile records it as `root:`
        // instead, which is the one place its identity belongs.
        if Some(&pkg.name) == lock.root.as_ref().map(|(n, _)| n) {
            continue;
        }
        let src = source_dir(root, &pkg.source)?;
        let dest = vendor_root.join(pkg.name.as_str());
        copy_package(&src, &dest)?;
        // Re-digest what was actually written, not what was read: a copy that
        // silently dropped or altered a file must fail here, at the moment it
        // is introduced, rather than at `verify` in someone's CI an hour later.
        let mut written = BTreeMap::new();
        for rel in package_files(&dest)? {
            let bytes = std::fs::read(dest.join(&rel)).map_err(|e| {
                Error::runtime(format!(
                    "ainl pkg: cannot read {}: {e}",
                    dest.join(&rel).display()
                ))
            })?;
            written.insert(rel.display().to_string(), digest::sha256_hex(&bytes));
        }
        if written != pkg.files {
            return Err(Error::runtime(format!(
                "ainl pkg: vendoring '{}' did not reproduce its source exactly \
                 ({} files in, {} out) — refusing to record a lockfile that \
                 would not verify",
                pkg.name,
                pkg.files.len(),
                written.len()
            )));
        }
        notes.push(format!(
            "vendored {} {} -> {VENDOR_DIR}/{}",
            pkg.name, pkg.version, pkg.name
        ));
        lock.packages.push(Locked {
            files: written,
            ..pkg
        });
    }
    Ok(Vendored { lock, notes })
}

fn prepare_vendor_dir(vendor_root: &Path, resolution: &Resolution) -> Result<()> {
    std::fs::create_dir_all(vendor_root).map_err(|e| {
        Error::runtime(format!(
            "ainl pkg: cannot create {}: {e}",
            vendor_root.display()
        ))
    })?;
    let keep: HashSet<&str> = resolution
        .packages
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    for entry in std::fs::read_dir(vendor_root)
        .map_err(|e| {
            Error::runtime(format!(
                "ainl pkg: cannot read {}: {e}",
                vendor_root.display()
            ))
        })?
        .filter_map(|e| e.ok())
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !keep.contains(name.as_str()) {
            let path = entry.path();
            let _ = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
        }
    }
    Ok(())
}

/// Copy a package's files into `dest`, replacing what is there.
///
/// The destination is removed first so a file that existed in the old copy and
/// not in the new one cannot survive: an extra file left behind would be
/// compiled into the program while the lockfile, which lists only the real
/// files, reported the tree as clean.
fn copy_package(src: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        std::fs::remove_dir_all(dest).map_err(|e| {
            Error::runtime(format!("ainl pkg: cannot replace {}: {e}", dest.display()))
        })?;
    }
    std::fs::create_dir_all(dest)
        .map_err(|e| Error::runtime(format!("ainl pkg: cannot create {}: {e}", dest.display())))?;
    for rel in package_files(src)? {
        let from = src.join(&rel);
        let to = dest.join(&rel);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                Error::runtime(format!("ainl pkg: cannot create {}: {e}", parent.display()))
            })?;
        }
        std::fs::copy(&from, &to).map_err(|e| {
            Error::runtime(format!(
                "ainl pkg: cannot copy {} to {}: {e}",
                from.display(),
                to.display()
            ))
        })?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Verify
// ---------------------------------------------------------------------------

/// One difference between the lockfile and the vendor dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    pub package: String,
    pub what: String,
}

/// Check the vendor dir against the lockfile.
///
/// This is the CI gate, and it is content-based. A file that is missing, extra,
/// edited or truncated is reported, each with the name and the reason — a
/// checker that printed a single boolean would tell a pipeline to fail without
/// telling anyone which package moved.
pub fn verify(root: &Path, lock: &Lock) -> Result<Vec<Mismatch>> {
    let vendor_root = root.join(VENDOR_DIR);
    let mut bad = Vec::new();
    for pkg in &lock.packages {
        let dir = vendor_root.join(pkg.name.as_str());
        if !dir.is_dir() {
            bad.push(Mismatch {
                package: pkg.name.to_string(),
                what: format!(
                    "is not vendored (expected {VENDOR_DIR}/{}); run `ainl pkg install`",
                    pkg.name
                ),
            });
            continue;
        }
        let actual = match package_files(&dir) {
            Ok(f) => f,
            Err(e) => {
                bad.push(Mismatch {
                    package: pkg.name.to_string(),
                    what: format!("cannot read: {}", e.message()),
                });
                continue;
            }
        };
        let actual_map: BTreeMap<String, String> = actual
            .iter()
            .map(|rel| {
                let bytes = std::fs::read(dir.join(rel)).unwrap_or_default();
                (rel.display().to_string(), digest::sha256_hex(&bytes))
            })
            .collect();
        for (file, want) in &pkg.files {
            match actual_map.get(file) {
                None => bad.push(Mismatch {
                    package: pkg.name.to_string(),
                    what: format!("is missing file {file}"),
                }),
                Some(got) if got != want => bad.push(Mismatch {
                    package: pkg.name.to_string(),
                    what: format!("has been modified: {file} (locked {want}, found {got})"),
                }),
                Some(_) => {}
            }
        }
        for file in actual_map.keys() {
            if !pkg.files.contains_key(file) {
                bad.push(Mismatch {
                    package: pkg.name.to_string(),
                    what: format!("has an unexpected file {file}"),
                });
            }
        }
    }
    // A package in the vendor dir that the lockfile does not know about is a
    // real problem — it would be compiled in with no digest pinning it.
    if vendor_root.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&vendor_root) {
            for entry in entries.filter_map(|e| e.ok()) {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !entry.path().is_dir() {
                    continue;
                }
                if !lock.packages.iter().any(|p| p.name.as_str() == name) {
                    bad.push(Mismatch {
                        package: name.clone(),
                        what: format!("is vendored but not in {LOCKFILE}"),
                    });
                }
            }
        }
    }
    Ok(bad)
}

// ---------------------------------------------------------------------------
// Vendor-aware import resolution
// ---------------------------------------------------------------------------

/// The directory `import` searches when a specifier names a package.
///
/// A bare specifier that is not a file resolves here, so a program depends on a
/// package by writing `(import "greet")` and a path is still a path. This is
/// the one place the two conventions meet, and it meets in the *loader* rather
/// than by rewriting source: the vendored file is loaded by exactly the code
/// path that loads any other module, so it inherits the canonical-path cache,
/// the cycle detection and the collision rule for free, and a package's
/// sources behave identically whether they arrived by path or by `pkg`.
pub fn vendor_dir(root: &Path) -> PathBuf {
    root.join(VENDOR_DIR)
}

/// Extra import candidates contributed by a vendor dir, for a bare specifier.
///
/// Two shapes are tried, in this order — and the order is the one `vendor`
/// writes them in, so the first hit is always the layout the package was
/// actually vendored under:
///
/// 1. `.ainl-vendor/<name>.ainl` — a package vendored as a *single file*, which
///    `ainl pkg` allows for a package with no sub-files. Cheapest to support,
///    and it makes a one-file package work with the same `(import "name")` as
///    any other.
/// 2. `.ainl-vendor/<name>/<name>.ainl` — the package's *entry module*. A
///    multi-file package has to name its entry file after itself, because that
///    is the only convention a machine can rely on without reading the
///    manifest for an `entry` key this tier does not have.
///
/// Both are checked only after ordinary file candidates (see
/// [`crate::import::existing_candidates`]), so a real file always beats a
/// package of the same name and `(import "name")` cannot silently change
/// meaning because someone ran `ainl pkg install`.
///
/// Only *bare* specifiers get this. A path-like specifier means "next to me"
/// and keeps meaning that; silently redirecting `lib/m.ainl` into a vendor dir
/// would make the same source resolve two different ways depending on the
/// program, which is the ambiguity this design is trying to remove.
pub fn vendor_candidates(name: &str, root: &Path) -> Vec<PathBuf> {
    if name.is_empty() || name.contains('/') || name.contains('\\') {
        return Vec::new();
    }
    let dir = vendor_dir(root);
    vec![
        dir.join(format!("{name}.{MODULE_EXT}")),
        dir.join(name).join(format!("{name}.{MODULE_EXT}")),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("ainl-pkg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("temp dir");
        base
    }

    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).expect("dir");
        std::fs::write(p, body).expect("write");
    }

    #[test]
    fn sha256_matches_the_published_vectors() {
        // FIPS 180-2 vectors. If these pass, the digest is a real SHA-256 and
        // a lockfile written on one machine verifies on another.
        assert_eq!(
            digest::sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            digest::sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            digest::sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn a_long_multi_block_message_hashes_correctly() {
        // 1000 * 'a' — the classic 64-byte-boundary vector. It is here because
        // a padding bug shows up only past the first block, which is exactly
        // where a hand-written SHA-256 goes wrong.
        let data = vec![b'a'; 1000];
        assert_eq!(
            digest::sha256_hex(&data),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
    }

    #[test]
    fn a_manifest_round_trips_through_its_text_form() {
        let text = "name: app\nversion: 0.1.0\n\n[deps]\n\
                    name: greet\nversion: 1.0.0\nsource: ../greet\n";
        let m = parse_manifest(text).expect("parses");
        assert_eq!(m.name.as_str(), "app");
        assert_eq!(m.version.as_str(), "0.1.0");
        assert_eq!(m.deps.len(), 1);
        assert_eq!(m.deps[0].name.as_str(), "greet");
        assert_eq!(m.deps[0].source, Source::Path(PathBuf::from("../greet")));
    }

    #[test]
    fn a_trailing_dep_without_a_section_still_parses() {
        // The last block in a file has no following section header, which is
        // the ordinary shape for a one-dependency manifest.
        let m = parse_manifest(
            "name: a\nversion: 1.0.0\n[deps]\n\
                                name: b\nversion: 2.0.0\nsource: ./b\n",
        )
        .expect("parses");
        assert_eq!(m.deps.len(), 1);
    }

    #[test]
    fn comments_are_stripped_but_not_inside_quotes() {
        let m =
            parse_manifest("# a comment\nname: a # trailing\nversion: 1.0.0\n").expect("parses");
        assert_eq!(m.name.as_str(), "a");
        // A '#' inside a quoted value is data, not a comment.
        let m = parse_manifest("name: a\nversion: 1.0.0\n").expect("parses");
        assert_eq!(m.version.as_str(), "1.0.0");
    }

    #[test]
    fn a_range_version_is_refused_with_a_readable_message() {
        let err = parse_manifest("name: a\nversion: ^1.0.0\n").expect_err("ranges are refused");
        assert!(
            err.message().contains("exact"),
            "the message must say what is supported, got: {err}"
        );
    }

    #[test]
    fn an_http_source_is_refused_because_there_is_no_registry() {
        let err = Source::parse("https://example.com/pkg.tar.gz").expect_err("refused");
        assert!(err.message().contains("no package registry"), "got: {err}");
    }

    #[test]
    fn a_git_source_needs_an_exact_revision() {
        let err = Source::parse("git:https://example.com/x.git").expect_err("needs a rev");
        assert!(err.message().contains("revision"), "got: {err}");
        let ok = Source::parse("git:https://example.com/x.git@abc123").expect("parses");
        assert_eq!(
            ok,
            Source::Git {
                url: "https://example.com/x.git".into(),
                rev: "abc123".into()
            }
        );
    }

    #[test]
    fn a_traversal_package_name_is_refused() {
        // The name becomes a directory under .ainl-vendor/, so this is the
        // check that keeps a manifest from writing outside the project.
        assert!(Name::new("../etc").is_err());
        assert!(Name::new("a/b").is_err());
        assert!(Name::new("UPPER").is_err());
        assert!(Name::new("trailing-").is_err());
        assert!(Name::new("ok-name-2").is_ok());
    }

    #[test]
    fn a_two_package_graph_resolves_vendors_and_verifies() {
        let root = tmp("graph");
        // Package b, a leaf.
        write(&root, "b/ainl.pkg", "name: b\nversion: 1.0.0\n");
        write(&root, "b/b.ainl", "(def bfn (fn () 1))\n");
        // Package a, which depends on b.
        write(
            &root,
            "a/ainl.pkg",
            "name: a\nversion: 1.0.0\n[deps]\nname: b\nversion: 1.0.0\nsource: ../b\n",
        );
        write(&root, "a/a.ainl", "(import \"b\")\n");
        // The project.
        write(
            &root,
            "app/ainl.pkg",
            "name: app\nversion: 0.1.0\n[deps]\nname: a\nversion: 1.0.0\nsource: ../a\n",
        );

        let app = root.join("app");
        let m = read_manifest(&app).expect("manifest");
        let v = vendor(&app, &m).expect("vendors");
        // Both transitive packages are vendored at the top level, each once.
        let names: Vec<String> = v.lock.packages.iter().map(|p| p.name.to_string()).collect();
        assert_eq!(names, vec!["a", "b"], "a->b graph vendors both");
        assert!(app.join(VENDOR_DIR).join("a").join("a.ainl").is_file());
        assert!(app.join(VENDOR_DIR).join("b").join("b.ainl").is_file());

        // And the lockfile round-trips.
        let reparsed = parse_lock(&v.lock.to_string()).expect("lockfile parses");
        assert_eq!(reparsed.packages.len(), 2);

        // Verify passes on a clean tree...
        assert!(verify(&app, &v.lock).expect("verify runs").is_empty());

        // ...and fails when a vendored file is edited.
        let tampered = app.join(VENDOR_DIR).join("b").join("b.ainl");
        std::fs::write(&tampered, "(def bfn (fn () 999))\n").expect("tamper");
        let bad = verify(&app, &v.lock).expect("verify runs");
        assert_eq!(bad.len(), 1, "tamper must be caught: {bad:?}");
        assert!(bad[0].what.contains("modified"), "got: {:?}", bad[0]);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_circular_dependency_is_refused_naming_the_cycle() {
        let root = tmp("cycle");
        write(
            &root,
            "a/ainl.pkg",
            "name: a\nversion: 1.0.0\n[deps]\nname: b\nversion: 1.0.0\nsource: ../b\n",
        );
        write(
            &root,
            "b/ainl.pkg",
            "name: b\nversion: 1.0.0\n[deps]\nname: a\nversion: 1.0.0\nsource: ../a\n",
        );
        write(
            &root,
            "ainl.pkg",
            "name: app\nversion: 0.1.0\n[deps]\nname: a\nversion: 1.0.0\nsource: ./a\n",
        );
        let m = read_manifest(&root).expect("manifest");
        let err = resolve(&root, &m).expect_err("a cycle is refused");
        let msg = err.message();
        assert!(msg.contains("circular dependency"), "got: {msg}");
        // The message must name the actual path, not just announce a cycle.
        assert!(msg.contains('a') && msg.contains('b'), "got: {msg}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_pinned_version_that_disagrees_with_the_manifest_is_refused() {
        let root = tmp("mismatch");
        write(&root, "b/ainl.pkg", "name: b\nversion: 2.0.0\n");
        write(
            &root,
            "ainl.pkg",
            "name: app\nversion: 0.1.0\n[deps]\nname: b\nversion: 1.0.0\nsource: ./b\n",
        );
        let m = read_manifest(&root).expect("manifest");
        let err = resolve(&root, &m).expect_err("refused");
        assert!(err.message().contains("pinned at 1.0.0"), "got: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn two_versions_of_one_package_are_refused_not_picked_between() {
        let root = tmp("twovers");
        write(&root, "b1/ainl.pkg", "name: b\nversion: 1.0.0\n");
        write(&root, "b2/ainl.pkg", "name: b\nversion: 2.0.0\n");
        write(
            &root,
            "ainl.pkg",
            "name: app\nversion: 0.1.0\n[deps]\n\
             name: b\nversion: 1.0.0\nsource: ./b1\n\
             name: c\nversion: 1.0.0\nsource: ./b1\n",
        );
        // a manifest cannot list two names mapping to different versions of b
        // in one file, so the two-version case is built by hand: a depends on
        // b@1, c depends on b@2, and the app depends on both.
        std::fs::remove_file(root.join("ainl.pkg")).expect("rm");
        write(
            &root,
            "a/ainl.pkg",
            "name: a\nversion: 1.0.0\n[deps]\nname: b\nversion: 1.0.0\nsource: ../b1\n",
        );
        write(
            &root,
            "c/ainl.pkg",
            "name: c\nversion: 1.0.0\n[deps]\nname: b\nversion: 2.0.0\nsource: ../b2\n",
        );
        write(
            &root,
            "ainl.pkg",
            "name: app\nversion: 0.1.0\n[deps]\n\
             name: a\nversion: 1.0.0\nsource: ./a\n\
             name: c\nversion: 1.0.0\nsource: ./c\n",
        );
        let m = read_manifest(&root).expect("manifest");
        let err = resolve(&root, &m).expect_err("refused");
        assert!(
            err.message().contains("two different versions"),
            "got: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_dependency_pointing_at_a_directory_with_no_manifest_is_refused() {
        let root = tmp("nomanifest");
        std::fs::create_dir_all(root.join("b")).expect("dir");
        write(
            &root,
            "ainl.pkg",
            "name: app\nversion: 0.1.0\n[deps]\nname: b\nversion: 1.0.0\nsource: ./b\n",
        );
        let m = read_manifest(&root).expect("manifest");
        let err = resolve(&root, &m).expect_err("refused");
        assert!(err.message().contains("ainl.pkg"), "got: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_lockfile_is_byte_stable_across_runs() {
        let root = tmp("stable");
        write(&root, "b/ainl.pkg", "name: b\nversion: 1.0.0\n");
        write(&root, "b/b.ainl", "(def bfn (fn () 1))\n");
        write(
            &root,
            "ainl.pkg",
            "name: app\nversion: 0.1.0\n[deps]\nname: b\nversion: 1.0.0\nsource: ./b\n",
        );
        let m = read_manifest(&root).expect("manifest");
        let one = vendor(&root, &m).expect("vendors").lock.to_string();
        let two = vendor(&root, &m).expect("vendors").lock.to_string();
        assert_eq!(one, two, "the same lockfile must come out twice");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn verify_catches_a_missing_and_an_extra_file() {
        let root = tmp("missing");
        write(&root, "b/ainl.pkg", "name: b\nversion: 1.0.0\n");
        write(&root, "b/one.ainl", "(def one 1)\n");
        write(&root, "b/two.ainl", "(def two 2)\n");
        write(
            &root,
            "ainl.pkg",
            "name: app\nversion: 0.1.0\n[deps]\nname: b\nversion: 1.0.0\nsource: ./b\n",
        );
        let m = read_manifest(&root).expect("manifest");
        let v = vendor(&root, &m).expect("vendors");
        std::fs::remove_file(root.join(VENDOR_DIR).join("b").join("two.ainl")).expect("rm");
        let bad = verify(&root, &v.lock).expect("verify");
        assert_eq!(bad.len(), 1, "{bad:?}");
        assert!(bad[0].what.contains("missing file two.ainl"), "{bad:?}");

        std::fs::write(
            root.join(VENDOR_DIR).join("b").join("extra.ainl"),
            "(def x 1)\n",
        )
        .expect("write");
        let bad = verify(&root, &v.lock).expect("verify");
        assert_eq!(bad.len(), 2, "{bad:?}");
        assert!(
            bad.iter().any(|b| b.what.contains("unexpected file")),
            "{bad:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_package_vendored_but_not_locked_is_reported() {
        let root = tmp("extraneous");
        write(&root, "b/ainl.pkg", "name: b\nversion: 1.0.0\n");
        write(
            &root,
            "ainl.pkg",
            "name: app\nversion: 0.1.0\n[deps]\nname: b\nversion: 1.0.0\nsource: ./b\n",
        );
        let m = read_manifest(&root).expect("manifest");
        let v = vendor(&root, &m).expect("vendors");
        std::fs::create_dir_all(root.join(VENDOR_DIR).join("ghost")).expect("dir");
        let bad = verify(&root, &v.lock).expect("verify");
        assert!(bad.iter().any(|b| b.package == "ghost"), "{bad:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_dependency_import_resolves_through_the_vendor_dir() {
        let root = tmp("vimport");
        write(&root, "b/ainl.pkg", "name: b\nversion: 1.0.0\n");
        write(&root, "b/b.ainl", "(def bfn (fn () 7))\n");
        write(
            &root,
            "ainl.pkg",
            "name: app\nversion: 0.1.0\n[deps]\nname: b\nversion: 1.0.0\nsource: ./b\n",
        );
        let m = read_manifest(&root).expect("manifest");
        vendor(&root, &m).expect("vendors");
        let cands = vendor_candidates("b", &root);
        assert_eq!(
            cands,
            vec![
                root.join(VENDOR_DIR).join("b.ainl"),
                root.join(VENDOR_DIR).join("b").join("b.ainl"),
            ],
            "a bare name tries the single-file shape, then the package dir"
        );
        assert!(
            cands.iter().any(|c| c.is_file()),
            "one candidate must exist after vendoring: {cands:?}"
        );
        // A path-like specifier is never redirected into the vendor dir.
        assert!(vendor_candidates("b/c", &root).is_empty());
    }

    #[test]
    fn a_git_source_resolves_through_the_cache_and_names_the_fix() {
        let err = clone_cache_dir("https://example.invalid/x.git", "deadbeef").expect_err("absent");
        assert!(
            err.message().contains("ainl pkg get"),
            "the message must name the command that fixes it, got: {err}"
        );
    }

    #[test]
    fn the_project_root_is_found_from_a_subdirectory() {
        let root = tmp("root");
        write(&root, "ainl.pkg", "name: app\nversion: 0.1.0\n");
        std::fs::create_dir_all(root.join("deep/deeper")).expect("dirs");
        assert_eq!(
            find_project_root(&root.join("deep/deeper")).expect("found"),
            root
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn package_files_skips_a_nested_vendor_dir() {
        let root = tmp("nested");
        write(&root, "ainl.pkg", "name: b\nversion: 1.0.0\n");
        write(&root, "b.ainl", "(def b 1)\n");
        write(&root, "lib/c.ainl", "(def c 1)\n");
        write(&root, ".ainl-vendor/other/d.ainl", "(def d 1)\n");
        let files = package_files(&root).expect("lists");
        let names: Vec<String> = files.iter().map(|f| f.display().to_string()).collect();
        assert_eq!(
            names,
            vec!["b.ainl", "lib/c.ainl"],
            "a nested vendor dir is not ours"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
