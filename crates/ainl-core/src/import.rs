//! `import` — file-based module composition, resolved at load time.
//!
//! AINL programs are S-expressions, so composition is a list whose head is the
//! symbol `import`:
//!
//! ```lisp
//! (import "lib/math.ainl")            ; flat: binds every top-level def
//! (import "lib/math.ainl" as m)       ; namespaced: binds one name `m`
//! ```
//!
//! # Why load-time, not a runtime special form
//!
//! The obvious implementation — an `import` branch inside the evaluators — was
//! rejected on purpose. `import` is not a *computation*: it has no value, and
//! what it produces (a set of bindings) is fixed before any user code runs.
//! Making it load-time buys three things a runtime form cannot:
//!
//! 1. **One implementation, both interpreters for free.** The loader resolves
//!    and evaluates a module's dependencies and hands the resulting *values*
//!    to whichever interpreter is already running. Neither interpreter grows a
//!    loader it would have to thread through `run`/`run_in` (public APIs many
//!    tests drive directly), and the two can never disagree about what
//!    `import` means — the thing that keeps them in lockstep today.
//! 2. **Isolation, and therefore an export rule that is a choice rather than
//!    an accident.** A module body evaluates in a *fresh* environment with the
//!    prelude in scope, so it cannot see the importer's names and its names
//!    cannot leak back. A module's exports are exactly the names it `def`s at
//!    its own top level — not "everything its env happens to hold". That last
//!    distinction matters: the looser rule would silently re-export everything
//!    the module itself imported, making a module's public surface depend on
//!    its own dependencies.
//! 3. **Static decidability.** Cycles, diamonds and duplicate imports are all
//!    resolvable before the program runs, so each gets a precise error rather
//!    than whatever order the dynamic lookup happened to produce.
//!
//! # Resolution order
//!
//! For a specifier `s`, from a file in directory `d` (see [`candidates`]):
//! 1. an absolute path is used as-is;
//! 2. a specifier containing a separator is *path-like*: `d/s` first, then
//!    `s` — a module importing a sibling is asking for "next to me", not "next
//!    to wherever I happen to be run from";
//! 3. a bare name is tried against the working directory **first**, then `d/s`.
//!
//! A candidate with no extension gets `.ainl` appended, so `(import "math")`
//! and `(import "math.ainl")` are the same module. A candidate that *does*
//! carry an extension is respected as written, so `(import "m.txt")` really
//! does read `m.txt` — an explicit extension is never second-guessed.
//!
//! When nothing resolves, the error lists every candidate with the base it was
//! tried against, so the fix is readable from the message alone.
//!
//! # Collision rule: error, never shadowing
//!
//! A flat `(import ...)` may not bind a name that is already bound — not to a
//! builtin, not to an earlier import, and not to a name the importing file
//! `def`s at its own top level. The error names the name, the module that
//! tried to bind it, where the name already came from, and the `as` escape
//! hatch.
//!
//! This is deliberately the strict choice. A silently shadowed binding is
//! invisible at the call site, which is the worst possible failure mode in a
//! language aimed at machine generation, where a program is written once and
//! read later by something that cannot see the import list. The namespaced
//! form is the principled answer to a genuine collision.
//!
//! Re-importing the *same* file is not a collision: a module is evaluated once
//! per canonical path and cached, so a diamond (`a` imports `c`, `b` imports
//! `c`, the main file imports both) evaluates `c` exactly once and binds its
//! names once. A file that imports itself, directly or transitively, is a
//! **cycle error** naming the cycle.

use crate::error::{Error, Result};
use crate::eval::Env;
use crate::parser::Node;
use crate::value::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// The symbol that introduces a module import.
pub const IMPORT_SYM: &str = "import";

/// The extension appended to an extension-less specifier.
pub const MODULE_EXT: &str = "ainl";

/// Hard cap on how many module files one program may pull in. Modules form a
/// graph the author did not necessarily lay out, and a cycle that somehow
/// escaped the cycle check must not spin forever. Generous enough for a real
/// multi-file tool, small enough to fail fast with a clear message.
const MAX_MODULES: usize = 512;

/// Which evaluator runs a module's body.
///
/// This is not bookkeeping. AINL carries two interpreters — the bytecode VM
/// (`vm`) and the tree-walk (`eval`) — and a closure made by one holds that
/// interpreter's compiled code (`Closure::code` is `Some` only for a
/// VM-made closure). A module body must therefore be evaluated by the *same*
/// interpreter that will run the code using it, or a run would mix two
/// representations of a closure, which the rest of the design assumes never
/// happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Executor {
    /// Run modules on the bytecode VM (what `ainl run` and the REPL use).
    Vm,
    /// Run modules on the tree-walking evaluator (the differential reference).
    TreeWalk,
}

/// A module that has been read, evaluated, and is ready to be bound.
#[derive(Clone)]
pub struct Module {
    /// Canonical path, used as the cache key so one file reached by two
    /// different relative paths is loaded — and evaluated — exactly once.
    pub path: PathBuf,
    /// The specifier as written, for error messages. This is what the user
    /// typed, which beats an absolute path they never saw.
    pub display: String,
    /// The module's exports, in first-`def` order: `(name, value)`.
    pub exports: Vec<(String, Value)>,
}

/// Per-load state: the module cache, the cycle stack, and the names bound so
/// far. The last of these is what makes "is this name already taken?" a
/// question with a decidable answer at load time, instead of a runtime
/// accident.
pub struct Loader {
    executor: Executor,
    modules: RefCell<HashMap<PathBuf, Module>>,
    /// The chain of modules currently being loaded, for cycle detection.
    stack: RefCell<Vec<PathBuf>>,
    /// A stack of scope tables, one per file whose scope is being resolved.
    ///
    /// A *stack*, not one table, and that is load-bearing: a name bound inside
    /// a module is not a name in its importer's scope. With a single shared
    /// table, `a.ainl` importing `math.ainl` would make `square` "already
    /// bound" for whoever imports `a.ainl`, even though `a` does not export it
    /// — so importing a module that uses a dependency, and then importing
    /// anything else, would fail for a reason invisible in the importer. The
    /// innermost table is the current file's scope; a module's table is pushed
    /// before its imports are bound and popped when it finishes.
    bound: RefCell<Vec<Scope>>,
}

/// One file's names, and the modules already bound into it.
#[derive(Default)]
struct Scope {
    /// Name in scope -> a human-readable description of where it came from,
    /// used verbatim in a collision error.
    names: HashMap<String, String>,
    /// Canonical paths of the modules already bound into this file.
    ///
    /// This is what makes re-importing the same file a no-op rather than a
    /// collision. Re-binding identical names would fail the freshness check even
    /// though nothing is actually being shadowed, so `(import "m")` twice — or
    /// two different routes to one file — must be recognized as the same import.
    /// Keyed by canonical path, so `lib/m.ainl` and `../lib/m.ainl` match.
    modules: HashSet<PathBuf>,
}

impl Loader {
    pub fn new(executor: Executor) -> Loader {
        let loader = Loader {
            executor,
            modules: RefCell::new(HashMap::new()),
            stack: RefCell::new(Vec::new()),
            bound: RefCell::new(Vec::new()),
        };
        // The prelude's names are in scope in every file, so they seed every
        // scope. Without this a module exporting `len` would bind over the
        // builtin silently — and every later `len` call in the program would
        // mean something different from what it looks like.
        let names: HashMap<String, String> = Env::with_prelude()
            .bindings()
            .into_iter()
            .map(|(name, _)| (name, "the prelude".to_string()))
            .collect();
        loader.bound.borrow_mut().push(Scope::new_with(names));
        loader
    }

    /// Record that `name` is already in scope in the *current* file, attributed
    /// to `origin` (a human-readable string, used as-is in collision errors).
    pub fn note_bound(&self, name: &str, origin: &str) {
        if let Some(scope) = self.bound.borrow_mut().last_mut() {
            scope.names.insert(name.to_string(), origin.to_string());
        }
    }

    /// True if `name` is already in scope in the current file.
    fn is_bound(&self, name: &str) -> bool {
        self.bound
            .borrow()
            .last()
            .is_some_and(|scope| scope.names.contains_key(name))
    }

    /// Where `name` came from, for a collision message.
    fn origin_of(&self, name: &str) -> Option<String> {
        self.bound
            .borrow()
            .last()
            .and_then(|scope| scope.names.get(name))
            .cloned()
    }

    /// True if the current file has already bound this module — i.e. the
    /// directive re-imports a file this file already has, which binds nothing
    /// new and therefore collides with nothing.
    fn already_bound(&self, path: &Path) -> bool {
        self.bound
            .borrow()
            .last()
            .is_some_and(|scope| scope.modules.contains(path))
    }

    /// Record that `path` has been bound into the current file.
    fn note_module(&self, path: &Path) {
        if let Some(scope) = self.bound.borrow_mut().last_mut() {
            scope.modules.insert(path.to_path_buf());
        }
    }

    /// Push a fresh scope for a module about to be resolved, and return a guard
    /// that pops it. Names a module binds stay inside that module, which is
    /// what keeps a module's imports from leaking into its importer's
    /// collision table.
    fn push_scope(&self) -> ScopeGuard<'_> {
        self.bound.borrow_mut().push(Scope::default());
        ScopeGuard { owner: self }
    }
}

impl Scope {
    fn new_with(names: HashMap<String, String>) -> Scope {
        Scope {
            names,
            modules: HashSet::new(),
        }
    }
}

/// Pops the name table pushed by [`Loader::push_scope`], on both the success and
/// the error path. It deliberately does *not* merge the names back into the
/// parent table: a module's scope is not part of its importer's.
struct ScopeGuard<'a> {
    owner: &'a Loader,
}

impl Drop for ScopeGuard<'_> {
    fn drop(&mut self) {
        self.owner.bound.borrow_mut().pop();
    }
}

// ---------------------------------------------------------------------------
// Directive parsing
// ---------------------------------------------------------------------------

/// One parsed `(import ...)` directive.
pub struct Spec {
    /// The specifier exactly as written in the source.
    pub raw: String,
    /// The `as` alias, when the directive used the namespaced form.
    pub alias: Option<String>,
    /// Byte offset of the directive's `import` symbol, for diagnostics.
    pub at: usize,
}

/// Parse one directive's arguments. AINL parses uniformly, so this is where
/// the *shape* `(import "f")` / `(import "f" as m)` is actually checked: a
/// malformed import would otherwise be a call to an unbound symbol, which is a
/// poor message for a mistake the user made in the right form.
fn parse_directive(args: &[Node]) -> Result<Spec> {
    let bad = |msg: &str| {
        Err(Error::runtime(format!(
            "import: {msg} (expected (import \"path.ainl\") or (import \"path.ainl\" as name))"
        )))
    };
    // The path is `args[0]`, not a whole-args slice pattern: the namespaced
    // form has three arguments, and matching the slice against a one-element
    // pattern would reject `(import "f" as m)` with a message about a missing
    // path — the exact opposite of the problem.
    let Some(Node::Str(path, _)) = args.first() else {
        return bad("expected a quoted path string");
    };
    if path.is_empty() {
        return bad("the path is empty");
    }
    match args.get(1) {
        None => Ok(Spec {
            raw: path.clone(),
            alias: None,
            at: 0,
        }),
        Some(Node::Sym(kw, _)) if kw == "as" => match args.get(2) {
            Some(Node::Sym(name, _)) if args.len() == 3 => Ok(Spec {
                raw: path.clone(),
                alias: Some(name.clone()),
                at: 0,
            }),
            _ => bad("`as` must be followed by exactly one name"),
        },
        Some(Node::Sym(other, _)) => bad(&format!(
            "expected `as` after the path, got the symbol '{other}'"
        )),
        Some(_) => bad("`as` must be followed by a name"),
    }
}

/// The arguments of `node` if it is an `import` directive, else `None`.
fn as_import(node: &Node) -> Option<&[Node]> {
    let Node::List(items, _) = node else {
        return None;
    };
    let Some(Node::Sym(head, _)) = items.first() else {
        return None;
    };
    (head == IMPORT_SYM).then_some(&items[1..])
}

/// Find every top-level `import` in `forms`, and reject any that is not at the
/// top level.
///
/// A nested import would be evaluated at some arbitrary point during a
/// function call, which is exactly the dynamic semantics this design rejects —
/// so it is an error rather than a silent extension of the feature. It is
/// reported *before* any file is read, which means a program with a nested
/// import fails identically whether or not the files it names exist: the shape
/// is wrong regardless, and a name that cannot be resolved would be a
/// misleading thing to complain about first.
pub fn scan(forms: &[Node], src: &str) -> Result<Vec<Spec>> {
    for form in forms {
        let Node::List(items, _) = form else {
            continue;
        };
        let Some(Node::Sym(head, _)) = items.first() else {
            continue;
        };
        // `quote` is data: nothing inside it is ever evaluated, so an `import`
        // in that position is a symbol in a list, not a directive.
        if head == "quote" {
            continue;
        }
        // Only a top-level form's *arguments* are searched. Its head is a
        // keyword or a callee — never a sub-form to execute — and the top-level
        // forms themselves are checked by the loop below, which is the only
        // thing that makes "top level" structural rather than a guess.
        if let Some(node) = nested_import(&items[1..]) {
            return Err(Error::runtime(format!(
                "import at line {}: `import` is only allowed at the top level of a file — a module's \
                 names must be bound before the code that uses them runs",
                line_of(src, node.span().start)
            )));
        }
    }
    let mut specs = Vec::new();
    for form in forms {
        if let Some(args) = as_import(form) {
            let mut spec = parse_directive(args)?;
            spec.at = form.span().start;
            specs.push(spec);
        }
    }
    Ok(specs)
}

/// The first `import` in a list of *sub-forms* — i.e. the first directive that
/// is not a top-level form of the file.
///
/// It is called only on the interiors of a form, so anything it finds is
/// genuinely nested. A nested `(import ...)` and a top-level one are the same
/// shape, so the only thing that can tell them apart is where the walk begins;
/// that is why this function and `scan`'s own loop are separate and neither
/// calls the other.
fn nested_import(forms: &[Node]) -> Option<&Node> {
    for form in forms {
        let Node::List(items, _) = form else {
            continue;
        };
        let Some(Node::Sym(head, _)) = items.first() else {
            continue;
        };
        if head == IMPORT_SYM {
            return Some(form);
        }
        if head == "quote" {
            continue;
        }
        if let Some(found) = nested_import(&items[1..]) {
            return Some(found);
        }
    }
    None
}

/// A file's forms with every top-level `import` directive removed.
///
/// The directives must be *stripped*, not merely resolved: both interpreters
/// dispatch a list by its head symbol, so a surviving `(import "m")` would be
/// compiled as an ordinary call to an unbound symbol named `import` — which
/// is exactly what a module that itself imports another module would hit.
pub fn strip_imports(forms: &[Node]) -> Vec<Node> {
    forms
        .iter()
        .filter(|f| as_import(f).is_none())
        .cloned()
        .collect()
}

/// A 1-based line number for a byte offset in `src`, for diagnostics.
fn line_of(src: &str, at: usize) -> usize {
    src[..at.min(src.len())].matches('\n').count() + 1
}

/// Every name `def`-bound at the top level of `forms`, in source order.
///
/// Read from the AST rather than from a module's finished environment because
/// the environment is not a reliable record of what was defined: a name the
/// module *imported* appears there too, and re-exporting it would make a
/// module's public surface depend on its own imports. `def` is the language's
/// one explicit binding form, so it is the one thing that makes a name the
/// module's own.
pub fn top_level_defs(forms: &[Node]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for form in forms {
        let Node::List(items, _) = form else {
            continue;
        };
        let Some(Node::Sym(head, _)) = items.first() else {
            continue;
        };
        if head != "def" {
            continue;
        }
        if let Some(Node::Sym(name, _)) = items.get(1) {
            if !out.contains(name) {
                out.push(name.clone());
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// The ordered candidate paths for `spec` imported from `from_dir`, each paired
/// with a short label naming the base it was tried against.
pub fn candidates(spec: &str, from_dir: &Path) -> Vec<(PathBuf, String)> {
    let p = Path::new(spec);
    let mut out: Vec<(PathBuf, String)> = Vec::new();
    let mut push = |path: PathBuf, label: String| {
        let path = with_module_ext(path);
        if !out.iter().any(|(existing, _)| *existing == path) {
            out.push((path, label));
        }
    };

    if p.is_absolute() {
        push(p.to_path_buf(), "absolute path".to_string());
        return out;
    }

    let importer = from_dir.display().to_string();
    if spec.contains('/') || spec.contains('\\') {
        push(from_dir.join(p), importer);
        push(p.to_path_buf(), "working directory".to_string());
    } else {
        push(p.to_path_buf(), "working directory".to_string());
        push(from_dir.join(p), importer);
    }
    out
}

/// Append [`MODULE_EXT`] unless the path already carries an extension.
fn with_module_ext(path: PathBuf) -> PathBuf {
    match path.extension() {
        Some(_) => path,
        None => path.with_extension(MODULE_EXT),
    }
}

/// The candidate list, minus anything that is not a readable file.
fn existing_candidates(spec: &str, from_dir: &Path) -> Vec<(PathBuf, String)> {
    candidates(spec, from_dir)
        .into_iter()
        .filter(|(p, _)| p.is_file())
        .collect()
}

/// Best-effort canonical path, used as the module cache key. `canonicalize`
/// resolves `..` and symlinks, so two routes to one file share a cache entry.
/// It can fail (a broken symlink, an unreadable parent), in which case the
/// lexically-normalized path is used: still a consistent key, just one that
/// cannot fold together two different routes to the same file.
pub fn cache_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| lexical_normalize(path))
}

/// Normalize `.` and `..` lexically. Unlike `canonicalize` this touches no
/// filesystem, so it cannot fail; it does not resolve symlinks, which is why it
/// is only the fallback.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

/// A readable, actionable "not found" error naming every candidate tried.
fn not_found(spec: &str, from_dir: &Path) -> Error {
    let tried = candidates(spec, from_dir)
        .into_iter()
        .map(|(p, label)| format!("  {} (from {label})", p.display()))
        .collect::<Vec<_>>()
        .join("\n");
    Error::runtime(format!(
        "import: cannot find module '{spec}'. Tried:\n{tried}"
    ))
}

/// A short display name for a path: at most the last two components, so a cycle
/// error reads `lib/math.ainl` rather than an absolute temp path.
fn display_of(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    match path.parent().and_then(|p| p.file_name()) {
        Some(dir) if !dir.is_empty() => format!("{}/{name}", dir.to_string_lossy()),
        _ => name,
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Load a module from disk, evaluate it in isolation, and return its exports.
/// A module already in `loader`'s cache is returned untouched — this is what
/// makes a diamond import evaluate the shared module exactly once.
pub fn load(spec: &str, from_dir: &Path, loader: &Loader) -> Result<Module> {
    let Some((path, _)) = existing_candidates(spec, from_dir).into_iter().next() else {
        return Err(not_found(spec, from_dir));
    };
    let key = cache_key(&path);

    if let Some(m) = loader.modules.borrow().get(&key) {
        return Ok(m.clone());
    }
    if loader.modules.borrow().len() >= MAX_MODULES {
        return Err(Error::runtime(format!(
            "import: too many modules (max {MAX_MODULES}) — one program may not pull in more than {MAX_MODULES} files"
        )));
    }
    {
        let stack = loader.stack.borrow();
        if let Some(pos) = stack.iter().position(|p| *p == key) {
            let cycle: Vec<String> = stack[pos..].iter().map(|p| display_of(p)).collect();
            return Err(Error::runtime(format!(
                "import: circular import — {} imports itself ({})",
                display_of(&key),
                cycle.join(" -> ")
            )));
        }
    }

    let src = std::fs::read_to_string(&path)
        .map_err(|e| Error::runtime(format!("import: cannot read {}: {e}", path.display())))?;

    loader.stack.borrow_mut().push(key.clone());
    let result = load_body(&path, &src, &key, spec, loader);
    // Pop whether or not the body succeeded, so a failed import does not leave
    // a phantom entry that would make an unrelated later import look cyclic.
    loader.stack.borrow_mut().pop();

    let module = result?;
    loader.modules.borrow_mut().insert(key, module.clone());
    Ok(module)
}

/// Read, resolve and evaluate one module file. Split out of [`load`] so the
/// cycle-stack push/pop around it is impossible to get wrong.
fn load_body(path: &Path, src: &str, key: &Path, display: &str, loader: &Loader) -> Result<Module> {
    let wrap = |e: Error| Error::runtime(format!("import: {}: {e}", display_of(path)));
    let forms = crate::parse(src).map_err(wrap)?;
    let specs = scan(&forms, src).map_err(wrap)?;
    let own_defs = top_level_defs(&forms);

    // A fresh environment with the prelude in scope: the module sees builtins
    // but never the importer's names, and its own names cannot escape except
    // through the `def`s collected in `own_defs`.
    let env = Env::with_prelude();

    // A scope of this module's own. Its imports are bound here, and this
    // module's `def` names are reserved here, so an import cannot take a name
    // the module defines itself — while a name bound *inside* this module
    // still cannot collide with the importer's scope.
    let _scope = loader.push_scope();
    for name in &own_defs {
        loader.note_bound(name, "this file");
    }
    // The module's own imports are bound into its env *before* its body runs,
    // which is what makes a module able to compose other modules. The names
    // they contribute are collected but then discarded: they are this module's
    // exports only if it also `def`s them, which is exactly what stops a module
    // from re-exporting its own dependencies.
    let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let mut scratch: Vec<(String, Value)> = Vec::new();
    for spec in &specs {
        let sub = load(&spec.raw, &dir, loader).map_err(wrap)?;
        bind_module(&env, &mut scratch, &sub, spec, loader).map_err(wrap)?;
    }

    // Evaluate the body. On failure the module's own name is added, so the
    // message points at the file with the bug rather than at the importer.
    // The directives are stripped first — a module may import, and a surviving
    // directive would be compiled as a call to an unbound `import`.
    let body = strip_imports(&forms);
    match loader.executor {
        Executor::Vm => crate::vm::run_forms(&body, &env),
        Executor::TreeWalk => crate::eval::run_forms(&body, &env),
    }
    .map_err(wrap)?;

    let exports: Vec<(String, Value)> = own_defs
        .into_iter()
        .filter_map(|name| env.get(&name).map(|v| (name, v)))
        .collect();
    Ok(Module {
        path: key.to_path_buf(),
        display: display.to_string(),
        exports,
    })
}

/// Define one module's bindings into `env`, appending them to `names`, and
/// enforcing the collision rule.
///
/// Every name an import would bind is checked *before* any of them is defined,
/// so two modules exporting the same name reports the collision instead of
/// half-applying the first and then failing.
///
/// Re-importing a file this file already has binds nothing and is a no-op. That
/// is not a special case bolted on afterwards — it is required for the
/// collision rule to be true rather than merely strict: re-binding the *same*
/// values shadows nothing, so reporting it as a collision would be a false
/// positive on a perfectly reasonable program (a diamond, or two spellings of
/// one path).
fn bind_module(
    env: &Env,
    names: &mut Vec<(String, Value)>,
    module: &Module,
    spec: &Spec,
    loader: &Loader,
) -> Result<()> {
    // A namespaced import binds its alias, not the module's exports, so a file
    // already bound flatly can still be bound under a fresh alias — and vice
    // versa. Only the exact same binding is a no-op.
    if spec.alias.is_none() && loader.already_bound(&module.path) {
        return Ok(());
    }
    match &spec.alias {
        Some(alias) => {
            check_fresh(alias, module, loader)?;
            loader.note_bound(
                alias,
                &format!("(import \"{}\" as {alias})", module.display),
            );
            let value = namespace_map(&module.exports);
            env.define(alias.clone(), value.clone());
            names.push((alias.clone(), value));
        }
        None => {
            for (name, _) in &module.exports {
                check_fresh(name, module, loader)?;
            }
            for (name, value) in &module.exports {
                loader.note_bound(name, &format!("(import \"{}\")", module.display));
                env.define(name.clone(), value.clone());
                names.push((name.clone(), value.clone()));
            }
        }
    }
    loader.note_module(&module.path);
    Ok(())
}

fn check_fresh(name: &str, module: &Module, loader: &Loader) -> Result<()> {
    if !loader.is_bound(name) {
        return Ok(());
    }
    let origin = loader
        .origin_of(name)
        .unwrap_or_else(|| "the prelude".to_string());
    Err(Error::runtime(format!(
        "import: '{name}' is already defined (by {origin}), so \"{}\" cannot bind it.\n\
         Rename one of them, or import the module under a name: \
         (import \"{}\" as <alias>) and reach it with (get <alias> \"{name}\")",
        module.display, module.display
    )))
}

/// A module's exports as a map value, for a namespaced import. Keys are
/// strings, so the ordinary map builtins are the whole access syntax:
/// `(get m "fib")`, `(has m "fib")`, `(keys m)`.
pub fn namespace_map(exports: &[(String, Value)]) -> Value {
    Value::Map(Rc::new(
        exports
            .iter()
            .map(|(k, v)| (Value::str(k.clone()), v.clone()))
            .collect(),
    ))
}

// ---------------------------------------------------------------------------
// Program assembly
// ---------------------------------------------------------------------------

/// A program's forms with its `import` directives removed, plus the bindings
/// those imports contribute.
///
/// The directives must be *stripped*, not merely resolved: the interpreters
/// dispatch a list by its head symbol, so a surviving `(import "m")` would be
/// compiled as an ordinary call to an unbound symbol named `import`.
pub struct Prepared {
    /// The program's own forms, in source order, with every top-level `import`
    /// removed. Everything else — order, spans, nested structure — is exactly
    /// what the source said.
    pub forms: Vec<Node>,
    /// `(name, value)` pairs to define in the program's environment before
    /// `forms` runs, in import order.
    pub names: Vec<(String, Value)>,
}

/// Resolve every top-level import in a program and strip the directives.
///
/// `from_dir` is the directory of the program doing the importing, which is the
/// base for *its* imports. The program's own top-level `def` names are
/// reserved before any import resolves, so a collision is caught even when the
/// `def` appears textually *after* the import — the same forward-reference
/// allowance the compiler already makes for `def`s, so `import` does not
/// introduce a surprising asymmetry.
///
/// The bindings are collected in a scratch environment and returned rather
/// than applied here, so a failure part-way through binds nothing: the
/// caller's environment is untouched when `prepare` returns `Err`.
pub fn prepare(forms: &[Node], src: &str, from_dir: &Path, loader: &Loader) -> Result<Prepared> {
    let specs = scan(forms, src)?;
    if specs.is_empty() {
        return Ok(Prepared {
            forms: forms.to_vec(),
            names: Vec::new(),
        });
    }
    for name in top_level_defs(forms) {
        loader.note_bound(&name, "this file");
    }
    let scratch = Env::new();
    // Import order, and within one import the module's `def` order: both are
    // observable through a collision message, and a HashMap would make that
    // order random. The list is as long as the program's imports, so a Vec
    // costs nothing and keeps the message deterministic.
    let mut names: Vec<(String, Value)> = Vec::new();
    for spec in &specs {
        let module = load(&spec.raw, from_dir, loader)?;
        bind_module(&scratch, &mut names, &module, spec, loader)?;
    }
    Ok(Prepared {
        forms: strip_imports(forms),
        names,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_name_tries_the_working_directory_first() {
        let c = candidates("math", Path::new("/proj/lib"));
        let names: Vec<String> = c.iter().map(|(p, _)| p.display().to_string()).collect();
        assert_eq!(names, vec!["math.ainl", "/proj/lib/math.ainl"]);
    }

    #[test]
    fn a_path_like_specifier_tries_the_importers_directory_first() {
        let c = candidates("sub/math", Path::new("/proj/lib"));
        let names: Vec<String> = c.iter().map(|(p, _)| p.display().to_string()).collect();
        assert_eq!(names, vec!["/proj/lib/sub/math.ainl", "sub/math.ainl"]);
    }

    #[test]
    fn an_explicit_extension_is_never_second_guessed() {
        let c = candidates("m.txt", Path::new("/proj"));
        assert_eq!(c[0].0, PathBuf::from("m.txt"));
    }

    #[test]
    fn an_absolute_path_has_exactly_one_candidate() {
        let c = candidates("/opt/m.ainl", Path::new("/proj"));
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].0, PathBuf::from("/opt/m.ainl"));
    }

    #[test]
    fn duplicate_candidates_are_collapsed() {
        // A path-like specifier in the working directory yields the same
        // candidate twice when the importer *is* the working directory; the
        // "tried:" list must not claim to have tried it twice.
        let c = candidates("sub/m", Path::new("."));
        let names: Vec<String> = c.iter().map(|(p, _)| p.display().to_string()).collect();
        assert_eq!(names, vec!["./sub/m.ainl", "sub/m.ainl"]);
    }

    #[test]
    fn lexical_normalize_folds_dot_segments() {
        assert_eq!(
            lexical_normalize(Path::new("a/./b/../c")),
            PathBuf::from("a/c")
        );
        assert_eq!(lexical_normalize(Path::new("../x")), PathBuf::from("../x"));
    }
}
