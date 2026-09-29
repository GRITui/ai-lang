//! `ainl pkg` — the package manager commands.
//!
//! Five verbs over three files (see [`ainl_core::pkg`]):
//!
//! ```text
//! ainl pkg init                      write ainl.pkg here
//! ainl pkg get <name>[@<version>] <source>   add a dep, resolve and vendor it
//! ainl pkg install                  vendor everything the manifest declares
//! ainl pkg list                     print the resolved graph
//! ainl pkg verify                   check the vendor dir against the lockfile
//! ```
//!
//! Every command resolves from the **project root** (the nearest ancestor with
//! an `ainl.pkg`), not the working directory, so `ainl pkg verify` means the
//! same thing from anywhere in a tree — which is what makes it usable as a CI
//! step that does not have to know where it was invoked from.
//!
//! `get` and `install` write the lockfile; `verify` never does. That split is
//! deliberate: a check that repairs as it checks cannot fail a build, and a
//! build that repairs as it builds is not reproducible.

use ainl_core::pkg::{self, Dep, Lock, Manifest, Name, Source, Version};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Run `ainl pkg <args>`. Returns the process exit code.
pub fn run(args: &[String]) -> ExitCode {
    let result = match args.first().map(String::as_str) {
        Some("init") => cmd_init(&args[1..]),
        Some("get") => cmd_get(&args[1..]),
        Some("install") => cmd_install(&args[1..]),
        Some("list") => cmd_list(&args[1..]),
        Some("verify") => cmd_verify(&args[1..]),
        Some(other) => Err(format!(
            "unknown subcommand '{other}' (init, get, install, list, verify)"
        )),
        None => Err("usage: ainl pkg <init|get|install|list|verify>".to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::FAILURE
        }
    }
}

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// `ainl pkg init [--name <name>]`.
///
/// Refuses to overwrite an existing manifest: `init` is a scaffolding command,
/// and a command that overwrites work silently is a command that eventually
/// destroys it. The check is a plain existence test rather than a prompt
/// because this has to work in a script.
fn cmd_init(args: &[String]) -> Result<(), String> {
    let mut name: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--name" => {
                let Some(v) = args.get(i + 1) else {
                    return Err("--name needs a package name".to_string());
                };
                name = Some(v.clone());
                i += 2;
            }
            other => return Err(format!("unknown flag '{other}' (supported: --name <name>)")),
        }
    }
    let dir = cwd();
    let manifest_path = dir.join(pkg::MANIFEST);
    if manifest_path.exists() {
        return Err(format!(
            "{} already exists here — edit it, or delete it first to start over",
            manifest_path.display()
        ));
    }
    // The name defaults to the directory's own name, which is what a person
    // almost always wants and never has to type.
    let name = match name {
        Some(n) => n,
        None => dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "app".to_string()),
    };
    let name = Name::new(&name).map_err(|e| e.message().to_string())?;
    std::fs::write(&manifest_path, pkg::manifest_template(&name))
        .map_err(|e| format!("cannot write {}: {e}", manifest_path.display()))?;
    println!("created {}", manifest_path.display());
    println!("next: ainl pkg get <name>@<version> <source>");
    Ok(())
}

/// `ainl pkg get <name>[@<version>] <source>`.
///
/// The version may be omitted when the dependency's own manifest supplies it,
/// which keeps the common case (`greet ../greet`) short while still pinning.
/// If it is given and disagrees with that manifest, the command refuses —
/// the same rule resolution enforces, so a lockfile can never disagree with
/// the source it claims to pin.
fn cmd_get(args: &[String]) -> Result<(), String> {
    if args.len() != 2 {
        return Err("usage: ainl pkg get <name>[@<version>] <source>\n  \
             source: a local path (./dir, ../dir, /abs) or git:<url>@<rev>"
            .to_string());
    }
    let root = pkg::find_project_root(&cwd()).map_err(|e| e.message().to_string())?;
    let mut manifest = pkg::read_manifest(&root).map_err(|e| e.message().to_string())?;

    // `name@version` or bare `name`. The split is on the LAST '@', so a local
    // path with an '@' in it (rare, legal on macOS) still parses when no
    // version was written.
    let (name_text, version_text) = match args[0].rsplit_once('@') {
        Some((n, v)) => (n.to_string(), Some(v.to_string())),
        None => (args[0].clone(), None),
    };
    let name = Name::new(&name_text).map_err(|e| e.message().to_string())?;
    let source = Source::parse(&args[1]).map_err(|e| e.message().to_string())?;

    // A git source is fetched here and now: this is the only command that
    // touches the network, and doing it at `get` time means `install` and
    // `verify` can run offline from the clone cache.
    let dir = match &source {
        Source::Git { url, rev } => {
            pkg::git_clone(url, rev).map_err(|e| e.message().to_string())?
        }
        Source::Path(_) => root.join(&args[1]),
    };
    let dep_manifest = pkg::read_manifest(&dir).map_err(|e| e.message().to_string())?;
    if dep_manifest.name != name {
        return Err(format!(
            "ainl pkg: {} is named '{}' in its own {}",
            dir.display(),
            dep_manifest.name,
            pkg::MANIFEST
        ));
    }
    let version = match version_text {
        Some(v) => {
            let v = Version::new(&v).map_err(|e| e.message().to_string())?;
            if v != dep_manifest.version {
                return Err(format!(
                    "ainl pkg: you asked for {name}@{v} but {} says {}",
                    dir.display(),
                    dep_manifest.version
                ));
            }
            v
        }
        None => dep_manifest.version.clone(),
    };

    let dep = Dep {
        name: name.clone(),
        version: version.clone(),
        source: source.clone(),
    };
    // Replace an existing dep of the same name rather than appending a second
    // one: `get` twice on the same package is an update, and leaving both would
    // make the manifest unparseable.
    manifest.deps.retain(|d| d.name != name);
    manifest.deps.push(dep);
    manifest.deps.sort_by(|a, b| a.name.cmp(&b.name));
    write_manifest(&root, &manifest)?;
    install_into(&root, &manifest)
}

/// `ainl pkg install` — vendor everything the manifest declares.
fn cmd_install(_args: &[String]) -> Result<(), String> {
    if !_args.is_empty() {
        return Err(format!(
            "unknown argument '{}' (install takes none)",
            _args[0]
        ));
    }
    let root = pkg::find_project_root(&cwd()).map_err(|e| e.message().to_string())?;
    let manifest = pkg::read_manifest(&root).map_err(|e| e.message().to_string())?;
    install_into(&root, &manifest)
}

fn install_into(root: &Path, manifest: &Manifest) -> Result<(), String> {
    let vendored = pkg::vendor(root, manifest).map_err(|e| e.message().to_string())?;
    write_lock(root, &vendored.lock)?;
    for note in &vendored.notes {
        println!("{note}");
    }
    println!(
        "resolved {} package(s); wrote {}",
        vendored.lock.packages.len(),
        pkg::LOCKFILE
    );
    Ok(())
}

/// `ainl pkg list` — the resolved tree, with the bytes each package carries.
fn cmd_list(_args: &[String]) -> Result<(), String> {
    if !_args.is_empty() {
        return Err(format!("unknown argument '{}' (list takes none)", _args[0]));
    }
    let root = pkg::find_project_root(&cwd()).map_err(|e| e.message().to_string())?;
    // Prefer the lockfile: it is the *resolved* graph, so `list` describes what
    // a build would actually compile rather than what the manifest currently
    // asks for. Falling back to a live resolve keeps it useful before the
    // first `install`.
    let lock_path = root.join(pkg::LOCKFILE);
    let lock: Lock = if lock_path.is_file() {
        let text = std::fs::read_to_string(&lock_path)
            .map_err(|e| format!("cannot read {}: {e}", lock_path.display()))?;
        pkg::parse_lock(&text).map_err(|e| e.message().to_string())?
    } else {
        let manifest = pkg::read_manifest(&root).map_err(|e| e.message().to_string())?;
        let r = pkg::resolve(&root, &manifest).map_err(|e| e.message().to_string())?;
        Lock {
            root: Some((manifest.name.clone(), manifest.version.clone())),
            packages: r.packages,
        }
    };
    if let Some((n, v)) = &lock.root {
        println!("{n} {v}");
    } else {
        println!("(no root recorded — run `ainl pkg install`)");
    }
    if lock.packages.is_empty() {
        println!("(no dependencies)");
        return Ok(());
    }
    let mut packages = lock.packages.clone();
    packages.sort();
    for p in &packages {
        let files: usize = p.files.len();
        println!(
            "  {} {}  ({files} file(s), {})",
            p.name,
            p.version,
            p.source.to_text()
        );
    }
    Ok(())
}

/// `ainl pkg verify` — the CI gate. Exits non-zero on any difference.
fn cmd_verify(_args: &[String]) -> Result<(), String> {
    if !_args.is_empty() {
        return Err(format!(
            "unknown argument '{}' (verify takes none)",
            _args[0]
        ));
    }
    let root = pkg::find_project_root(&cwd()).map_err(|e| e.message().to_string())?;
    let lock_path = root.join(pkg::LOCKFILE);
    if !lock_path.is_file() {
        return Err(format!(
            "ainl pkg: no {} here — run `ainl pkg install` to create it",
            lock_path.display()
        ));
    }
    let text = std::fs::read_to_string(&lock_path)
        .map_err(|e| format!("cannot read {}: {e}", lock_path.display()))?;
    let lock = pkg::parse_lock(&text).map_err(|e| e.message().to_string())?;
    // The lockfile must belong to *this* project. A lock checked in next to the
    // wrong manifest is a real and confusing failure, and one comparison turns
    // it into a message instead.
    let (mn, mv) = read_name_version(&root)?;
    if let Some((ln, lv)) = &lock.root {
        if ln != &mn || lv != &mv {
            return Err(format!(
                "ainl pkg: {} is for {ln} {lv}, but {} says {mn} {mv} \
                 — regenerate it with `ainl pkg install`",
                pkg::LOCKFILE,
                pkg::MANIFEST
            ));
        }
    }
    let bad = pkg::verify(&root, &lock).map_err(|e| e.message().to_string())?;
    if bad.is_empty() {
        println!(
            "ok   {} package(s) match {}",
            lock.packages.len(),
            pkg::LOCKFILE
        );
        return Ok(());
    }
    for m in &bad {
        eprintln!("FAIL {} {}", m.package, m.what);
    }
    Err(format!(
        "{} difference(s) between {}/ and {} — run `ainl pkg install`",
        bad.len(),
        pkg::VENDOR_DIR,
        pkg::LOCKFILE
    ))
}

fn read_name_version(root: &Path) -> Result<(Name, Version), String> {
    let m = pkg::read_manifest(root).map_err(|e| e.message().to_string())?;
    Ok((m.name, m.version))
}

/// Write the manifest back, preserving the template's comment header.
///
/// The header is not decoration — it is where the format is explained to
/// whoever opens the file — so a manifest written by `ainl pkg init` keeps its
/// documentation after a `get`. Only `name`/`version`/`[deps]` are rewritten.
fn write_manifest(root: &Path, manifest: &Manifest) -> Result<(), String> {
    let path = root.join(pkg::MANIFEST);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let header: String = existing
        .lines()
        .take_while(|l| l.trim_start().starts_with('#') || l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let mut body = String::new();
    if !header.trim().is_empty() {
        body.push_str(&header);
        body.push_str("\n\n");
    }
    body.push_str(&format!(
        "name: {}\nversion: {}\n",
        manifest.name, manifest.version
    ));
    if !manifest.deps.is_empty() {
        body.push_str("\n[deps]\n");
        for d in &manifest.deps {
            body.push_str(&format!(
                "name: {}\nversion: {}\nsource: {}\n",
                d.name,
                d.version,
                d.source.to_text()
            ));
        }
    }
    std::fs::write(&path, body).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn write_lock(root: &Path, lock: &Lock) -> Result<(), String> {
    let path = root.join(pkg::LOCKFILE);
    std::fs::write(&path, lock.to_string())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}
