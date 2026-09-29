//! `db-open` / `db-put` / `db-get` / `db-flush` / `db-close` — the AINL
//! storage engine: one file, an append-only log, and crash recovery.
//!
//! This module is the **normative** definition of the five builtins and of the
//! on-disk format, the same relationship `json_value.rs` has to its ports. The
//! AOT C runtime carries a hand-port of the format (see the `db_*` block in
//! `crates/ainl-cc/src/runtime.c`), and the Python/JS/Ruby transpilers **refuse**
//! the builtins — see `ainl_core::interpreter_only` for why the refusal is
//! worded `transpiler-only` rather than `interpreter-only`, which matters here
//! because `ainl compile` does run them.
//!
//! # The format
//!
//! A `.ainl-db` file is a 16-byte header followed by an append-only log. Every
//! number is little-endian.
//!
//! ```text
//! header  "AINLDB" | version u8 | 0 u8 | header_len u32 | 0 u32
//! record  key_len u32 | val_len u32 | crc32 u32 | key bytes | val bytes
//! ```
//!
//! `crc32` is the CRC-32 everyone means: the reflected IEEE polynomial
//! `0xEDB88320`, initial value `0xFFFFFFFF`, final XOR `0xFFFFFFFF`. It is the
//! checksum `zlib.crc32("123456789")` returns (`0xCBF43926`), which is the test
//! vector both implementations carry — a "checksum" that is only self-consistent
//! between one writer and one reader is not a crash-recovery mechanism, it is a
//! private convention.
//!
//! Why a log and not a sorted tree: a log is the only structure where a write
//! is a pure append. A B-tree or an LSM tree has to rewrite interior nodes, so a
//! crash can leave a *hole* in the middle of the file rather than only a partial
//! record at the end, and recovering from a hole is a repair rather than a
//! replay. The log's cost is that replay is linear; that is exactly what
//! [`Db::open`] does once, and what it builds is an in-memory index, so every
//! later lookup is O(1).
//!
//! # Crash recovery
//!
//! The log is the source of truth, and it is the *only* thing on disk. `open`
//! reads the whole file, replays complete records into the in-memory index, and
//! stops at the first record that is truncated, oversized, fails its checksum,
//! or holds bytes that are not text. Everything from that point on is discarded
//! and the file is **truncated** back to the end of the last good record, so the
//! next append lands on a clean boundary instead of inheriting the garbage.
//!
//! The two properties this buys, both asserted by
//! `crates/ainl-cc/tests/db_crash.rs` against a *compiled binary*:
//!
//! * A process killed mid-write loses at most the record it was writing. Every
//!   record that was completely written — and flushed — is still there.
//! * Reopening is **idempotent**: recovery truncates, so opening the recovered
//!   file again replays exactly the same records and answers identically. A
//!   recovery that only ignored the tail would leave the same broken bytes in
//!   place, and the *second* open would be the one that silently lost data.
//!
//! A record whose checksum is wrong is treated exactly like a torn one — the
//! engine cannot tell a half-written record from a corrupted one, and it must
//! not try: a checksum that "usually" passes is not a checksum. The caller
//! learns which keys survived by reading them, not by being told.
//!
//! # Handles
//!
//! `db-open` returns an **int**: a slot number, 1-based, allocated lowest-free
//! and released by `db-close`. An int rather than a map, for two reasons that
//! are really one reason — it keeps the value model unchanged (no new `Value`
//! variant, so `print`, `=` and the JSON writer need no new arm in either
//! backend), and it makes "this handle was closed" a *checkable* state rather
//! than a missing binding, so `db-put` on a closed handle is an error naming
//! the handle instead of undefined behaviour.
//!
//! The registry is thread-local, like the evaluator's step counter, because a
//! `BuiltinFn` is a bare `fn` pointer with nowhere to put captured state. That
//! is the same constraint `http.rs` works under, and the reason this module
//! exists as a registry rather than as a method on a value.
//!
//! # Values are text
//!
//! Keys and values are AINL strings, so they are always valid UTF-8. A record
//! whose checksum passes but whose bytes are not well-formed text could only
//! have been written by something other than AINL, and `open` drops it — the
//! same rule `read-file` already applies to a file whose bytes are not UTF-8
//! (see `utf8_valid` in the C runtime for the port's copy). A record is dropped
//! rather than the whole database refused, so the records before it survive: a
//! bad tail is a tail, whether the crash or a foreign writer caused it.

use crate::error::{Error, Result};
use crate::eval::Env;
use crate::value::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::rc::Rc;

// ---- the format -----------------------------------------------------------

/// The six magic bytes every `.ainl-db` file starts with.
///
/// Six and not eight: a magic that spells the format is greppable, and the
/// version lives in its own field right after it, where a reader can branch on
/// it without arithmetic.
pub const MAGIC: &[u8; 6] = b"AINLDB";

/// The format version this engine reads and writes.
pub const VERSION: u8 = 1;

/// The header is a fixed 16 bytes, and says so in its own field so a future
/// version can grow it without a reader having to guess.
pub const HEADER_LEN: usize = 16;

/// The size of one record's fixed part: `key_len`, `val_len`, `crc32`.
pub const RECORD_HEADER_LEN: usize = 12;

/// The most databases one program may hold open at once.
///
/// A real limit rather than a growing table, because the alternative is an
/// allocation per handle in a runtime that promises a static binary with no
/// allocator accounting. 64 open files is far past what a script wants and far
/// below what would matter.
pub const MAX_OPEN: usize = 64;

/// The CRC-32 table: reflected IEEE, built at compile time.
///
/// A hand-rolled table rather than a crate, because the whole project is
/// zero-dependency so the runtime can be a static binary (see
/// docs/MASTER_PLAN.md §1.2). `crc32("123456789") == 0xCBF43926` is the check
/// that the polynomial is the standard one and not a private variant.
static CRC_TABLE: [u32; 256] = build_crc_table();

const fn build_crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            // Reflected form: shift right, and conditionally XOR the reversed
            // 0xEDB88320 polynomial.
            c = if c & 1 != 0 {
                0xEDB88320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

/// The CRC-32 of `bytes`: the reflected IEEE polynomial zlib defines.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in bytes {
        c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

/// The two length fields exist to frame the body, and the checksum covers
/// `key ++ value` — so the two are *not* redundant, they answer different
/// questions. The lengths say where the split is; the CRC says whether what was
/// written is what was intended. Framing alone would happily accept a record
/// whose key was silently shifted by a corrupted length field, which is why the
/// CRC is over the body rather than over the body *and* the header: a flipped
/// length byte changes what the body is, and the next record's CRC is what
/// catches it.
/// Public because it is part of the *format*, not of the implementation: a test
/// in the `ainl-cc` crate has to hand-build a record carrying a correct
/// checksum — to prove that a C-side replay rejects a record for some reason
/// *other* than a bad CRC — and computing that constant by hand in a second
/// language is how a test ends up asserting the code's own behaviour.
pub fn crc32_body(key: &[u8], value: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in key.iter().chain(value.iter()) {
        c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

// ---- the engine ------------------------------------------------------------

/// An open database: the log file plus the index replayed from it.
///
/// The index maps a key to the bytes of its **latest** value, so `get` is a
/// hash lookup and never touches the file. Overwriting a key appends a new
/// record; replay applies records in order, so the last write wins — which is
/// the same answer the caller gets from the index it is reading through, on
/// every backend.
pub struct Db {
    path: String,
    file: std::fs::File,
    index: HashMap<String, String>,
}

impl std::fmt::Debug for Db {
    /// The path and the key count — the two things a failing test needs to name
    /// where it was looking. The `File` and the values are not printed: a value
    /// is caller data, and a panic message is not the place to dump it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("path", &self.path)
            .field("keys", &self.index.len())
            .finish()
    }
}

impl Db {
    /// Open (or create) the database at `path` and replay its log.
    ///
    /// Truncates a torn tail, so the returned handle always sits on a record
    /// boundary — which is what makes the second `open` of a recovered file
    /// produce the same answer as the first.
    fn open(path: &str) -> Result<Db> {
        // `truncate(false)` is the *default* and is written out because the
        // alternative reading of `create(true)` is "create a fresh empty
        // database", which is exactly the bug that would destroy the log this
        // function is about to replay. An existing file is opened and replayed;
        // only a missing one is created.
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|_| db_err(DB_OPEN, "cannot open", path))?;

        let mut raw = Vec::new();
        file.read_to_end(&mut raw)
            .map_err(|_| db_err(DB_OPEN, "cannot read", path))?;

        if raw.is_empty() {
            write_header(&mut file).map_err(|_| db_err(DB_OPEN, "cannot write", path))?;
        } else {
            check_header(&raw, path)?;
        }

        let mut off = HEADER_LEN;
        let mut index: HashMap<String, String> = HashMap::new();
        // Replay to the first record that does not verify. Every `break` below
        // is a tail: the bytes from there on are discarded, not parsed.
        while off + RECORD_HEADER_LEN <= raw.len() {
            let key_len = read_u32(&raw, off) as usize;
            let val_len = read_u32(&raw, off + 4) as usize;
            let want = read_u32(&raw, off + 8);
            let body = off + RECORD_HEADER_LEN;
            // The two length fields are crash-controlled, so the bounds are
            // checked against what is actually in the file *before* any slicing
            // and before any allocation. A corrupt `val_len` of 0xFFFFFFFF must
            // not become a 4 GiB read on a forty-byte file.
            if key_len > raw.len() - body || val_len > raw.len() - body - key_len {
                break;
            }
            let end = body + key_len + val_len;
            let key_bytes = &raw[body..body + key_len];
            let val_bytes = &raw[body + key_len..end];
            if crc32_body(key_bytes, val_bytes) != want {
                break;
            }
            // Well-formed but not text: a foreign writer, not a crash. Dropped
            // like a torn record, so the records before it still survive.
            // A NUL is text by `from_utf8`'s rules but not by AINL's — the
            // lexer's escapes are `\\` `"` `/` `n` `r` `t` and there is no
            // `\0` — and the C port holds a value in a `char *`, where an
            // embedded NUL would silently truncate it. The two implementations
            // must agree on what "a usable record" means, or the same file
            // recovers differently on each backend.
            let usable = |b: &[u8]| std::str::from_utf8(b).is_ok() && !b.contains(&0);
            if !usable(key_bytes) || !usable(val_bytes) {
                break;
            }
            index.insert(
                std::str::from_utf8(key_bytes).expect("checked").to_string(),
                std::str::from_utf8(val_bytes).expect("checked").to_string(),
            );
            off = end;
        }

        // Drop the torn tail *on disk*, not just in memory. See the module note:
        // a recovery that leaves the garbage in place makes the next open the
        // one that loses data.
        if off != raw.len() {
            file.set_len(off as u64)
                .map_err(|_| db_err(DB_OPEN, "cannot truncate", path))?;
        }
        file.seek(SeekFrom::End(0))
            .map_err(|_| db_err(DB_OPEN, "cannot seek", path))?;

        Ok(Db {
            path: path.to_string(),
            file,
            index,
        })
    }

    /// Append one record and index it.
    fn put(&mut self, key: &str, value: &str) -> Result<()> {
        let key_b = key.as_bytes();
        let val_b = value.as_bytes();
        let mut rec = Vec::with_capacity(RECORD_HEADER_LEN + key_b.len() + val_b.len());
        rec.extend_from_slice(&(key_b.len() as u32).to_le_bytes());
        rec.extend_from_slice(&(val_b.len() as u32).to_le_bytes());
        rec.extend_from_slice(&crc32_body(key_b, val_b).to_le_bytes());
        rec.extend_from_slice(key_b);
        rec.extend_from_slice(val_b);
        self.file
            .write_all(&rec)
            .map_err(|_| db_err(DB_PUT, "cannot write", &self.path))?;
        self.index.insert(key.to_string(), value.to_string());
        Ok(())
    }

    /// The latest value for `key`, or `None` if the log has no such key.
    fn get(&self, key: &str) -> Option<&str> {
        self.index.get(key).map(|s| s.as_str())
    }

    /// Force the log to disk: the process's buffer, then the device.
    ///
    /// `sync_all` rather than just `flush`, because "on disk" for a crash test
    /// means the device, not the process. A `fsync`-less flush would pass every
    /// test in this file — the bytes really are in the page cache and a reopen
    /// in the same process would find them — and lose data on a power cut.
    fn flush(&mut self) -> Result<()> {
        self.file
            .flush()
            .and_then(|()| self.file.sync_all())
            .map_err(|_| db_err(DB_FLUSH, "cannot flush", &self.path))
    }
}

/// A handle's slot. `RefCell` rather than `&mut Db` because a builtin is a
/// `fn(&[Value])` — there is no place to stash a `&mut` across the call, and a
/// program may legitimately call `db-get` on one handle while iterating another.
type Slot = Option<Rc<RefCell<Db>>>;

thread_local! {
    /// Open handles, indexed by slot number minus one.
    static OPEN: RefCell<Vec<Slot>> = const { RefCell::new(Vec::new()) };
}

fn db_err(who: &str, verb: &str, path: &str) -> Error {
    Error::runtime(format!("{who}: {verb} '{path}'"))
}

/// Borrow the handle `n` and run `f` against it, or explain that it is not open.
///
/// `f` returns a `Result` so a handle borrow and an operation result stay one
/// value to the caller: a `db-put` that fails on the write and a `db-put` on a
/// closed handle are the same shape of failure, and the caller has one `?`.
fn with_db<T>(n: i64, who: &str, f: impl FnOnce(&mut Db) -> Result<T>) -> Result<T> {
    let db = OPEN
        .with(|c| {
            let slots = c.borrow();
            if n < 1 {
                return None;
            }
            slots.get((n - 1) as usize).and_then(|s| s.clone())
        })
        .ok_or_else(|| Error::runtime(format!("{who}: handle {n} is not open")))?;
    // The `RefMut` is bound to a named local and the call is its own
    // statement, so the guard is dropped *before* `db` goes out of scope.
    // Written as a single tail expression instead, the guard's destructor is
    // ordered after `db`'s and the borrow does not outlive its referent.
    let mut borrowed = db.borrow_mut();
    f(&mut borrowed)
}

fn as_str_arg<'a>(v: &'a Value, who: &str, what: &str) -> Result<&'a str> {
    match v {
        Value::Str(s) => Ok(s),
        other => Err(Error::runtime(format!(
            "{who} expects a str {what}, got {}",
            other.type_name()
        ))),
    }
}

/// The handle operand, reported under the builtin's own name.
fn as_handle(v: &Value, who: &str) -> Result<i64> {
    match v {
        Value::Int(n) => Ok(*n),
        other => Err(Error::runtime(format!(
            "{who} expects a db handle, got {}",
            other.type_name()
        ))),
    }
}

// ---- header helpers --------------------------------------------------------

fn write_header(file: &mut std::fs::File) -> std::io::Result<()> {
    let mut h = Vec::with_capacity(HEADER_LEN);
    h.extend_from_slice(MAGIC);
    h.push(VERSION);
    h.push(0);
    h.extend_from_slice(&(HEADER_LEN as u32).to_le_bytes());
    h.extend_from_slice(&0u32.to_le_bytes());
    file.write_all(&h)
}

fn check_header(raw: &[u8], path: &str) -> Result<()> {
    if raw.len() < HEADER_LEN || &raw[..MAGIC.len()] != MAGIC {
        return Err(Error::runtime(format!(
            "db-open: '{path}' is not an AINL database"
        )));
    }
    let version = raw[MAGIC.len()];
    if version != VERSION {
        return Err(Error::runtime(format!(
            "db-open: '{path}' is database version {version}, but this AINL reads \
             version {VERSION}"
        )));
    }
    Ok(())
}

fn read_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

// ---- the builtins ----------------------------------------------------------

/// The names this module installs. The refusal scan matches on exactly these,
/// so they are constants rather than literals at the binding sites.
pub const DB_OPEN: &str = "db-open";
pub const DB_CLOSE: &str = "db-close";
pub const DB_PUT: &str = "db-put";
pub const DB_GET: &str = "db-get";
pub const DB_FLUSH: &str = "db-flush";

/// Every `db-*` name, in the order they are reported. One list so the scanner,
/// the docs and the tests cannot disagree about which symbols are the engine.
pub const DB_BUILTINS: &[&str] = &[DB_OPEN, DB_CLOSE, DB_PUT, DB_GET, DB_FLUSH];

/// Bind the five storage builtins into `env`.
pub fn install(env: &Env) {
    macro_rules! b {
        ($name:expr, $f:expr) => {
            env.define($name, Value::Builtin { name: $name, f: $f });
        };
    }
    b!(DB_OPEN, db_open);
    b!(DB_CLOSE, db_close);
    b!(DB_PUT, db_put);
    b!(DB_GET, db_get);
    b!(DB_FLUSH, db_flush);
}

fn db_open(args: &[Value]) -> Result<Value> {
    let [path] = args else {
        return Err(Error::runtime(format!(
            "{DB_OPEN} expects ({DB_OPEN} path)"
        )));
    };
    let path = as_str_arg(path, DB_OPEN, "path")?;
    // Check the table's capacity *before* creating the file. Opening a
    // database is a side effect on the filesystem, and "too many open
    // databases" must not leave behind a file the caller never asked for — on
    // the 65th open of a path that did not exist, the interpreter and the C
    // port would otherwise disagree about whether it now does.
    OPEN.with(|c| {
        let slots = c.borrow();
        if slots.iter().all(|s| s.is_some()) && slots.len() >= MAX_OPEN {
            return Err(Error::runtime(format!(
                "{DB_OPEN}: too many open databases (max {MAX_OPEN})"
            )));
        }
        Ok(())
    })?;
    let db = Rc::new(RefCell::new(Db::open(path)?));
    OPEN.with(|c| {
        let mut slots = c.borrow_mut();
        // Lowest free slot, so a program that opens, closes and reopens gets
        // its handle back instead of a new number each time.
        match slots.iter().position(|s| s.is_none()) {
            Some(i) => {
                slots[i] = Some(db);
                Ok(Value::Int(i as i64 + 1))
            }
            None => {
                slots.push(Some(db));
                Ok(Value::Int(slots.len() as i64))
            }
        }
    })
}

fn db_close(args: &[Value]) -> Result<Value> {
    let [h] = args else {
        return Err(Error::runtime(format!(
            "{DB_CLOSE} expects ({DB_CLOSE} handle)"
        )));
    };
    let h = as_handle(h, DB_CLOSE)?;
    let db = OPEN.with(|c| {
        let mut slots = c.borrow_mut();
        if h < 1 {
            return None;
        }
        slots.get_mut((h - 1) as usize).and_then(|s| s.take())
    });
    let Some(db) = db else {
        return Err(Error::runtime(format!(
            "{DB_CLOSE}: handle {h} is not open"
        )));
    };
    // Flush *before* dropping: `close` is the last chance to get the records
    // out, and a caller that never called `db-flush` should not have to know
    // that. The slot is released first, so a failed flush still frees the
    // handle — the error is already reported and holding the slot would leak
    // the only handle a caller could retry with.
    let flushed = db.borrow_mut().flush();
    drop(db);
    flushed?;
    Ok(Value::Nil)
}

fn db_put(args: &[Value]) -> Result<Value> {
    let [h, key, value] = args else {
        return Err(Error::runtime(format!(
            "{DB_PUT} expects ({DB_PUT} handle key value)"
        )));
    };
    let h = as_handle(h, DB_PUT)?;
    let key = as_str_arg(key, DB_PUT, "key")?;
    let value = as_str_arg(value, DB_PUT, "value")?;
    with_db(h, DB_PUT, |db| db.put(key, value))?;
    Ok(Value::Nil)
}

fn db_get(args: &[Value]) -> Result<Value> {
    let [h, key] = args else {
        return Err(Error::runtime(format!(
            "{DB_GET} expects ({DB_GET} handle key)"
        )));
    };
    let h = as_handle(h, DB_GET)?;
    let key = as_str_arg(key, DB_GET, "key")?;
    // An absent key is `nil`, not an error: that is the answer `get` gives for
    // a missing map key, and it makes `db-get` a total function.
    match with_db(h, DB_GET, |db| Ok(db.get(key).map(str::to_string)))? {
        Some(s) => Ok(Value::str(s)),
        None => Ok(Value::Nil),
    }
}

fn db_flush(args: &[Value]) -> Result<Value> {
    let [h] = args else {
        return Err(Error::runtime(format!(
            "{DB_FLUSH} expects ({DB_FLUSH} handle)"
        )));
    };
    let h = as_handle(h, DB_FLUSH)?;
    with_db(h, DB_FLUSH, |db| db.flush())?;
    Ok(Value::Nil)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch path unique to one test; removed on drop.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let mut p = std::env::temp_dir();
            p.push(format!(
                "ainl-db-{}-{tag}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&p);
            Scratch(p)
        }

        fn path(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }

        fn raw(&self) -> Vec<u8> {
            std::fs::read(&self.0).expect("read")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The value at `key` after opening the file fresh, as a plain `String`.
    fn reopen(s: &Scratch) -> Db {
        Db::open(&s.path()).expect("open")
    }

    #[test]
    fn crc32_is_the_standard_one() {
        // The vector everyone quotes. A private checksum is the failure this
        // pins: it would still round-trip between this engine and its C port,
        // and would not be a CRC at all.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn the_two_length_fields_are_load_bearing() {
        // The checksum covers key ++ value, so ("a", "b") and ("ab", "") have the
        // *same* CRC by construction. What keeps them apart is the framing: the
        // reader splits the body with the two lengths, so a record written one
        // way is not read the other way. A port that concatenated the body and
        // searched for the split would collapse these into one key.
        let s = Scratch::new("framing");
        let mut db = Db::open(&s.path()).expect("open");
        db.put("a", "b").expect("put");
        db.put("ab", "").expect("put");
        drop(db);
        let db = reopen(&s);
        assert_eq!(db.get("a"), Some("b"));
        assert_eq!(db.get("ab"), Some(""));
        assert_eq!(db.index.len(), 2, "the two records must stay distinct");
    }

    #[test]
    fn a_new_file_starts_with_the_header() {
        let s = Scratch::new("header");
        Db::open(&s.path()).expect("open");
        let raw = s.raw();
        assert_eq!(&raw[..6], b"AINLDB");
        assert_eq!(raw[6], VERSION);
        assert_eq!(raw.len(), HEADER_LEN, "an empty database is header only");
    }

    #[test]
    fn records_survive_a_reopen_and_the_last_write_wins() {
        let s = Scratch::new("reopen");
        let mut db = Db::open(&s.path()).expect("open");
        db.put("a", "1").expect("put");
        db.put("b", "2").expect("put");
        db.put("a", "3").expect("overwrite");
        drop(db);

        let db = reopen(&s);
        assert_eq!(db.get("a"), Some("3"));
        assert_eq!(db.get("b"), Some("2"));
        assert_eq!(db.get("zz"), None);
    }

    #[test]
    fn a_torn_tail_is_dropped_and_the_file_is_repaired() {
        let s = Scratch::new("torn");
        let mut db = Db::open(&s.path()).expect("open");
        db.put("a", "1").expect("put");
        db.put("b", "2").expect("put");
        drop(db);
        let good_len = s.raw().len();

        // A half-written record: the header is there, the value is short.
        let mut raw = s.raw();
        raw.extend_from_slice(&7u32.to_le_bytes()); // key_len
        raw.extend_from_slice(&99u32.to_le_bytes()); // val_len: more than is present
        raw.extend_from_slice(&0u32.to_le_bytes());
        raw.extend_from_slice(b"partial");
        std::fs::write(&s.0, &raw).expect("write");

        let db = reopen(&s);
        assert_eq!(db.get("a"), Some("1"));
        assert_eq!(db.get("b"), Some("2"));
        assert_eq!(db.get("partial"), None);
        // The repair happened *on disk*, which is what makes the second open
        // identical to the first.
        assert_eq!(s.raw().len(), good_len, "the torn tail is still on disk");
        let db2 = reopen(&s);
        assert_eq!(db2.get("a"), Some("1"));
    }

    #[test]
    fn a_record_with_a_bad_checksum_is_rejected() {
        let s = Scratch::new("crc");
        let mut db = Db::open(&s.path()).expect("open");
        db.put("a", "1").expect("put");
        db.put("b", "2").expect("put");
        drop(db);

        // Flip one byte inside the *last* record's value. Every length still
        // parses, so only the checksum can catch this.
        let mut raw = s.raw();
        let last = raw.len() - 1;
        raw[last] ^= 0xFF;
        std::fs::write(&s.0, &raw).expect("write");

        let db = reopen(&s);
        assert_eq!(db.get("a"), Some("1"));
        assert_eq!(db.get("b"), None, "a corrupted record must not be served");
    }

    #[test]
    fn a_corrupt_length_does_not_trust_the_length_field() {
        let s = Scratch::new("huge-len");
        let mut db = Db::open(&s.path()).expect("open");
        db.put("a", "1").expect("put");
        drop(db);
        // Claim a 4 GiB value in a forty-byte file. A replay that trusted the
        // length field would try to read (or allocate) it.
        let mut raw = s.raw();
        let last_rec = raw.len() - (RECORD_HEADER_LEN + 2);
        raw[last_rec..last_rec + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(&s.0, &raw).expect("write");
        let db = reopen(&s);
        assert_eq!(db.get("a"), None);
    }

    #[test]
    fn a_foreign_file_is_refused() {
        let s = Scratch::new("foreign");
        std::fs::write(&s.0, b"not a database at all, just bytes").expect("write");
        let e = Db::open(&s.path()).expect_err("must refuse");
        assert!(
            e.message().contains("is not an AINL database"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn a_future_version_is_refused_by_name() {
        let s = Scratch::new("version");
        let mut h = Vec::new();
        h.extend_from_slice(MAGIC);
        h.push(VERSION + 1);
        h.push(0);
        h.extend_from_slice(&(HEADER_LEN as u32).to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes());
        std::fs::write(&s.0, h).expect("write");
        let e = Db::open(&s.path()).expect_err("must refuse");
        assert!(
            e.message().contains("is database version 2"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn a_checksum_valid_but_non_text_record_is_dropped() {
        let s = Scratch::new("nontext");
        let mut db = Db::open(&s.path()).expect("open");
        db.put("a", "1").expect("put");
        drop(db);
        // A record a hand-built file could contain and AINL cannot write: valid
        // lengths, valid CRC, invalid UTF-8. Dropped rather than refusing the
        // file, so the record before it survives.
        let key: &[u8] = b"bad";
        let val: &[u8] = &[0xFF, 0xFE];
        let mut raw = s.raw();
        raw.extend_from_slice(&(key.len() as u32).to_le_bytes());
        raw.extend_from_slice(&(val.len() as u32).to_le_bytes());
        raw.extend_from_slice(&crc32_body(key, val).to_le_bytes());
        raw.extend_from_slice(key);
        raw.extend_from_slice(val);
        std::fs::write(&s.0, &raw).expect("write");
        let db = reopen(&s);
        assert_eq!(db.get("a"), Some("1"));
        assert_eq!(db.get("bad"), None);
    }

    #[test]
    fn an_empty_value_round_trips() {
        // A zero-length value is the case a `""` -vs-missing confusion would
        // collapse, and `db-get` must return the empty string, not nil.
        let s = Scratch::new("empty-val");
        let mut db = Db::open(&s.path()).expect("open");
        db.put("k", "").expect("put");
        drop(db);
        let db = reopen(&s);
        assert_eq!(db.get("k"), Some(""));
        assert_eq!(db.get("other"), None);
    }

    #[test]
    fn a_multi_byte_value_round_trips_byte_for_byte() {
        // Values are text but they are *bytes* on disk: "héllo 日本" is 6 + 1 +
        // 6 bytes. A port that counted characters would truncate.
        let s = Scratch::new("utf8");
        let v = "héllo 日本 😀";
        let mut db = Db::open(&s.path()).expect("open");
        db.put("k", v).expect("put");
        drop(db);
        let db = reopen(&s);
        assert_eq!(db.get("k"), Some(v));
    }

    #[test]
    fn a_key_with_spaces_and_newlines_round_trips() {
        // The framing is length-prefixed, so no key is special — including one
        // that would break a line-oriented format.
        let s = Scratch::new("weird-key");
        let mut db = Db::open(&s.path()).expect("open");
        for k in ["a b", "a\nb", "  ", "日本", "k=v", "-"] {
            db.put(k, "v").expect("put");
        }
        drop(db);
        let db = reopen(&s);
        for k in ["a b", "a\nb", "  ", "日本", "k=v", "-"] {
            assert_eq!(db.get(k), Some("v"), "key {k:?} did not round trip");
        }
    }

    #[test]
    fn a_record_holding_a_nul_is_dropped() {
        // Well-formed UTF-8, valid CRC — but a NUL cannot cross the C port's
        // `char *` boundary intact, so both implementations must drop the
        // record. Without this the same file would replay as a truncated value
        // in a compiled binary and as the full one in the interpreter.
        let s = Scratch::new("nul");
        let mut db = Db::open(&s.path()).expect("open");
        db.put("a", "1").expect("put");
        drop(db);
        let key: &[u8] = b"k";
        let val: &[u8] = b"a\0b";
        let mut raw = s.raw();
        raw.extend_from_slice(&(key.len() as u32).to_le_bytes());
        raw.extend_from_slice(&(val.len() as u32).to_le_bytes());
        raw.extend_from_slice(&crc32_body(key, val).to_le_bytes());
        raw.extend_from_slice(key);
        raw.extend_from_slice(val);
        std::fs::write(&s.0, &raw).expect("write");
        let db = reopen(&s);
        assert_eq!(db.get("a"), Some("1"), "earlier records must survive");
        assert_eq!(db.get("k"), None);
    }

    #[test]
    fn a_full_table_refuses_the_next_open_without_creating_a_file() {
        // The capacity check runs before the file is created, so the 65th open
        // of a path that does not exist must leave the filesystem alone.
        let dir = std::env::temp_dir().join(format!(
            "ainl-db-cap-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let missing = dir.join("never.ainl-db");
        let missing_path = missing.to_string_lossy().into_owned();
        let v = |s: &str| Value::str(s.to_string());
        let mut handles = Vec::new();
        for i in 0..MAX_OPEN {
            let p = dir
                .join(format!("db{i}.ainl-db"))
                .to_string_lossy()
                .into_owned();
            handles.push(db_open(&[v(&p)]).expect("open within the limit"));
        }
        let e = db_open(&[v(&missing_path)]).expect_err("the table is full");
        assert!(
            e.message().contains("too many open databases"),
            "got: {}",
            e.message()
        );
        assert!(
            !missing.exists(),
            "a refused open must not create the file it was asked for"
        );
        // Closing one frees that slot, and the next open takes the *lowest*
        // free one — which is the one just released, so the number comes back
        // rather than a new one appearing.
        let closed = handles.pop().expect("a handle");
        db_close(std::slice::from_ref(&closed)).expect("close");
        let reused = db_open(&[v(&missing_path)]).expect("open after a close");
        assert_eq!(reused, closed, "the released handle number is reused");
        assert!(missing.exists(), "the open that succeeded created the file");
        db_close(&[reused]).expect("close");
        for h in handles {
            db_close(&[h]).expect("close");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_closed_handle_is_not_open_any_more() {
        // The reason the handle is an int and not a binding: using a closed
        // handle has to be a checkable error rather than an unbound symbol.
        let s = Scratch::new("closed");
        let path = s.path();
        let h = db_open(&[Value::str(path)]).expect("open");
        let Value::Int(handle_no) = h else {
            panic!("db-open must return a handle number, got {h}");
        };
        db_close(&[Value::Int(handle_no)]).expect("close");
        let e = db_put(&[Value::Int(handle_no), Value::str("k"), Value::str("v")])
            .expect_err("a closed handle must be refused");
        assert!(
            e.message()
                .contains(&format!("handle {handle_no} is not open")),
            "got: {}",
            e.message()
        );
    }
}
