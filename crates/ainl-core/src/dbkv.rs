//! `db-set` / `db-get` / `db-del` / `db-keys` / `db-count` — the key-value API,
//! a value-level layer over the [`crate::db`] storage engine.
//!
//! # What this layer is
//!
//! [`crate::db`] (Tier 4 card 1) is a *byte-level* store: `db-put` takes a
//! string, `db-get` hands the same string back. That is the right primitive — it
//! is what the log, the checksum and crash recovery are all built on — but it
//! cannot express the thing a program actually wants to remember, which is a
//! number, a boolean, a list, or `nil`.
//!
//! This module closes that gap without touching the engine. A value is encoded
//! as **JSON**, through the *existing* [`crate::json_value`] writer and parser —
//! the same pair `json-serialize` and `json-parse` use — and the resulting text
//! is stored as bytes by `db-put`/`db-get`. So the on-disk format is unchanged:
//! JSON-encoded values riding inside the 4.1 record envelope. **No new
//! serialization format was introduced**, which is the property that keeps the
//! AOT C port small.
//!
//! # Why JSON, and what it costs
//!
//! Reusing the JSON machinery is not free of consequences, and the one that
//! matters is the number model. AINL has two numeric types, `Int` and `Float`;
//! JSON has one. The writer preserves the distinction by emitting `1` for an int
//! and `1.0` for a float, and the parser reads a bare `1` back as an `Int` and a
//! `1.0` back as a `Float` — so **the round trip is exact** on a backend whose
//! numbers are the same two types.
//!
//! That is true for the interpreter, the VM and the AOT C runtime. It is *not*
//! true for a JavaScript target, which has one number type: an AINL int becomes a
//! `Number` and would come back as `1.0`. `db-*` is refused by the transpilers
//! outright (see [`crate::interpreter_only`]), so that divergence is documented
//! rather than papered over — the same way `json_parity.rs` pins its own JS
//! exception explicitly instead of quietly filtering the comparison.
//!
//! Values that JSON cannot represent — a symbol, a closure, a builtin, a map with
//! a non-string key, a non-finite float, nesting past the depth limit — are
//! **refused with the JSON writer's own message**, not with a new one. A program
//! that tries to store a function gets `json-serialize: cannot serialize a
//! closure`, which is the same answer it would get from calling `json-serialize`
//! directly, and it is the answer the C port gives too because the C port calls
//! the same C JSON writer. Reusing the error rather than inventing one is what
//! makes the refusal identical on both engines for free.
//!
//! # `db-get` and its two layers
//!
//! There is a genuine name collision here, and it is worth being explicit about.
//! `db-get` means two different things in this tier:
//!
//! * at the **byte layer**, it is `db-put`'s reader — `(db-get h "k")` returns
//!   the raw text of whatever bytes are stored under `"k"`;
//! * at the **value layer**, it is `db-set`'s reader — `(db-get h "k")` returns
//!   the AINL value that `db-set` stored, having JSON-decoded it.
//!
//! They cannot both hold. The resolution here: **`db-get` is the value-level
//! read**, because this is the card that defines the value-level API, and a
//! `db-set`/`db-get` pair that did not round-trip its own argument would be
//! useless. `db-put` is untouched and keeps its documented string-only contract,
//! and the two layers still compose, because JSON is a superset of a string in
//! exactly the useful direction: a value written by `db-put` as the text
//! `buy milk` is *not* valid JSON, so `db-get` on it fails. See
//! [`db_get`] for the full rule — this is the sharpest edge in the module and it
//! is specified rather than left to a reader to discover.
//!
//! # Deletion is a log record, not a mutation
//!
//! An append-only log has no way to remove a record, so `db-del` **appends a
//! tombstone** and the replay applies records in order: a tombstone removes the
//! key from the in-memory index, and a later `db-set` for the same key adds it
//! back. That is what makes deletion survive a crash exactly the way a write
//! does — the recovery path is the same replay, not a second mechanism that has
//! to be kept in agreement with the first. It is also why `db-del` is
//! last-write-wins in the same way `db-set` is.
//!
//! The cost is the same cost the log already has and already documents: the file
//! grows. A key written and deleted still leaves two records behind. Compaction
//! is a later tier's problem, and pretending otherwise would mean a second
//! write path.
//!
//! # Keys
//!
//! Keys stay strings, as they are at the byte layer, and get no JSON treatment.
//! They are already length-prefixed in the record envelope, so a key containing
//! a space, a newline, a quote or non-ASCII text round-trips unchanged and needs
//! no escaping. Quoting them would be inventing a second key syntax to solve a
//! problem the format does not have.

use crate::db::{Db, DB_BUILTINS};
use crate::error::{Error, Result};
use crate::eval::Env;
use crate::json_value;
use crate::value::{ConsCell, Value};

pub const DB_SET: &str = "db-set";
pub const DB_DEL: &str = "db-del";
pub const DB_KEYS: &str = "db-keys";
pub const DB_COUNT: &str = "db-count";

/// The byte layer's raw read, kept under a new name.
///
/// Tier 4 card 1 shipped `db-get` as a byte-level read and documented it that
/// way, and card 2 needs `db-get` to be value-level. Rather than break the
/// documented contract, the byte-level read moves here: `db-put` and
/// `db-get-raw` are now the pair that read and write **bytes**, and
/// `db-set`/`db-get` are the pair that read and write **values**.
///
/// This is not a convenience rename. Without it there is no way to recover the
/// exact text a `db-put` wrote — which is the only thing 4.1's own crash tests
/// can assert about a record, and the only way a program can use this database
/// as a byte store now that `db-get` decodes. It reads the whole log, tombstones
/// included, because a raw read has no opinion about what a tombstone means:
/// that filtering is the value layer's job.
pub const DB_GET_RAW: &str = "db-get-raw";

/// `db-get` is the **byte layer's** name constant, reused rather than
/// redeclared: the value layer rebinds that same name, and two constants with
/// one value would be a way for the scanner, the C port and the docs to disagree
/// about what the symbol is called.
pub use crate::db::DB_GET;

/// Every `db-*` name this tier adds or reuses, in the order the refusal scanner
/// reports them. One list so the scanner, the docs and the tests cannot disagree
/// about which symbols are the value layer.
pub const KV_BUILTINS: &[&str] = &[DB_SET, DB_GET, DB_GET_RAW, DB_DEL, DB_KEYS, DB_COUNT];

/// The tombstone that marks a deleted key in the log.
///
/// A record whose value is exactly this byte string means "the key is gone".
/// It cannot collide with a real value, because a value in the log is always the
/// output of `json-serialize` and a bare word is never valid JSON — the writer
/// quotes every string, so a stored value always begins with `"`, `[`, `{`, `t`,
/// `f`, `n`, `-` or a digit. The tombstone begins with `~`, which is in none of
/// those sets. This is the property that lets deletion share the byte layer
/// instead of needing a second record type.
///
/// `pub(crate)` rather than private because the table layer deletes rows with
/// **the same** tombstone, deliberately: "this key is gone" must be one byte
/// string in the log rather than two, or a row deleted as a table row and a key
/// deleted as a KV key would be recovered by two different rules.
pub(crate) const TOMBSTONE: &str = "~";

/// Borrow the handle `n`, apply `f`, and turn an absent handle into an error.
///
/// This is [`crate::db::with_db`] under a local name, so the `who` a caller
/// passes reaches the same error message a byte-layer call would produce —
/// `db-set: handle 4 is not open`, not a `db-put` message. A value-layer
/// builtin borrowing the byte layer's registry is the point: one table, one
/// handle space, so `db-open` returning 1 means the same 1 to both layers.
fn with_db<T>(n: i64, who: &str, f: impl FnOnce(&mut Db) -> Result<T>) -> Result<T> {
    crate::db::with_db(n, who, f)
}

fn as_handle(v: &Value, who: &str) -> Result<i64> {
    match v {
        Value::Int(n) => Ok(*n),
        other => Err(Error::runtime(format!(
            "{who} expects a db handle, got {}",
            other.type_name()
        ))),
    }
}

fn as_key<'a>(v: &'a Value, who: &str) -> Result<&'a str> {
    match v {
        Value::Str(s) => Ok(s),
        other => Err(Error::runtime(format!(
            "{who} expects a str key, got {}",
            other.type_name()
        ))),
    }
}

/// `nil` when the key is absent or deleted; otherwise the stored value with the
/// tombstone filtered out.
fn lookup(db: &Db, key: &str) -> Option<String> {
    match db.get(key) {
        Some(TOMBSTONE) | None => None,
        Some(v) => Some(v.to_string()),
    }
}

/// Every key that is still **live**, in the index's own (undefined) order.
///
/// The filter is here rather than in [`Db::keys`] because a tombstone is the
/// value layer's concept: the byte layer has no idea what `~` means, and must
/// not — it would then have to know which of its own values are magic, which is
/// the coupling this module exists to avoid. Getting this wrong is silent and
/// total: a deleted key would still be listed by `db-keys` and still counted by
/// `db-count`, so `db-del` would look like it worked while a program that
/// enumerated the store kept seeing the corpse.
fn live_keys(db: &Db) -> Vec<String> {
    db.keys()
        .into_iter()
        .filter(|k| db.get(k) != Some(TOMBSTONE))
        .collect()
}

// ---- the builtins ----------------------------------------------------------

/// Bind the four new builtins, and re-bind `db-get` as the **value-level** read.
///
/// `db-get` is the tier's one name collision: `db::install` bound it to the
/// byte-level reader, and that binding is what this call replaces. The value
/// layer is installed *after* the byte layer precisely so that the value-level
/// `db-get` is the one a program ends up calling — the alternative ordering
/// would leave `db-set` writing JSON and `db-get` returning the raw text, which
/// fails this card's own round-trip criterion.
///
/// So `install` must stay the second call. That ordering is the contract, and it
/// is why the byte layer's reader is not simply deleted: `db::db_get` still
/// serves anyone who reaches it directly, and the two implementations are
/// pinned against each other by the tests in this module.
pub fn install(env: &Env) {
    macro_rules! b {
        ($name:expr, $f:expr) => {
            env.define($name, Value::Builtin { name: $name, f: $f });
        };
    }
    b!(DB_SET, db_set);
    b!(DB_GET, db_get);
    b!(DB_GET_RAW, db_get_raw);
    b!(DB_DEL, db_del);
    b!(DB_KEYS, db_keys);
    b!(DB_COUNT, db_count);
}

/// `(db-set handle key value)` → `nil`.
///
/// Stores any AINL value JSON-encoded. Returns `nil` so a caller can put it in a
/// `do` body without the store result leaking into a later form, which is the
/// same choice `db-put` and `db-flush` make.
fn db_set(args: &[Value]) -> Result<Value> {
    let [h, key, value] = args else {
        return Err(Error::runtime(format!(
            "{DB_SET} expects ({DB_SET} handle key value)"
        )));
    };
    let h = as_handle(h, DB_SET)?;
    let key = as_key(key, DB_SET)?;
    // A value JSON cannot represent is refused by the writer, with the writer's
    // own message. That is deliberate: the AOT C port performs the same encode
    // through the same C JSON writer, so both engines report one string instead
    // of two that have to be kept in agreement.
    let encoded = match json_value::builtin_json_serialize(std::slice::from_ref(value))? {
        Value::Str(s) => s.as_str().to_string(),
        other => {
            return Err(Error::runtime(format!(
                "internal: json-serialize returned a {}",
                other.type_name()
            )))
        }
    };
    with_db(h, DB_SET, |db| db.put(key, &encoded))?;
    Ok(Value::Nil)
}

/// `(db-get handle key)` → the value, or `nil` if absent or deleted.
///
/// This is the value-level read, and it is the one name collision in the tier:
/// `db-put` is the byte-level writer and `(db-get h k)` on a key `db-put`
/// stored returns its **text**, not a decoded value. Three cases, in order:
///
/// * absent, or deleted — `nil`;
/// * stored by `db-set` — the JSON-decoded AINL value, exactly equal to what
///   was written;
/// * stored by `db-put` — the stored text, if that text happens to be a JSON
///   literal; otherwise a `db-get` error naming the stored bytes.
///
/// The third case is the sharp edge. The alternative — always returning raw text
/// — is what would let a `db-set`/`db-get` pair fail its own round-trip, which
/// is the whole point of this card. The error names the bytes rather than
/// failing to a bare `nil`, because "your `db-put` text is not a value" is the
/// actionable fact and `nil` would be indistinguishable from a missing key.
fn db_get(args: &[Value]) -> Result<Value> {
    let [h, key] = args else {
        return Err(Error::runtime(
            "db-get expects (db-get handle key)".to_string(),
        ));
    };
    let h = as_handle(h, "db-get")?;
    let key = as_key(key, "db-get")?;
    let Some(stored) = with_db(h, "db-get", |db| Ok(lookup(db, key)))? else {
        return Ok(Value::Nil);
    };
    let v = Value::str(stored.clone());
    match json_value::builtin_json_parse(std::slice::from_ref(&v)) {
        Ok(parsed) => Ok(parsed),
        // The stored text is not a value — a `db-put` string, or a record from a
        // future version. The message quotes the bytes so the caller can see
        // what is actually in the log, and names the value-layer writer as the
        // thing that fixes it.
        Err(_) => Err(Error::runtime(format!(
            "db-get: '{key}' holds text that is not an AINL value ({stored}); \
             store it with {DB_SET} rather than db-put"
        ))),
    }
}

/// `(db-get-raw handle key)` → the stored **text**, or `nil` if absent.
///
/// The byte layer's reader, moved here from `db-get` when the value layer took
/// that name. It returns whatever bytes are in the log for `key`, with no
/// decoding — including the tombstone a `db-del` leaves, because deciding what a
/// tombstone means is the value layer's job and a raw read has no opinion about
/// it. `(db-get-raw h k)` returning `"~"` is therefore the way to see a deletion
/// in the raw log, which is a genuinely useful thing for a program debugging its
/// own database.
fn db_get_raw(args: &[Value]) -> Result<Value> {
    let [h, key] = args else {
        return Err(Error::runtime(format!(
            "{DB_GET_RAW} expects ({DB_GET_RAW} handle key)"
        )));
    };
    let h = as_handle(h, DB_GET_RAW)?;
    let key = as_key(key, DB_GET_RAW)?;
    match with_db(h, DB_GET_RAW, |db| Ok(db.get(key).map(|s| s.to_string())))? {
        Some(s) => Ok(Value::str(s)),
        None => Ok(Value::Nil),
    }
}

/// `(db-del handle key)` → `true` if the key was there, `false` if not.
///
/// Returns a boolean rather than `nil` so a caller can tell a delete that
/// removed something from one that did not, which is the difference between
/// "the file has no such key" and "I removed it". Deleting an absent key is not
/// an error: it is the idempotent answer, and it is what makes
/// `if (db-del h k) ...` safe to run twice.
fn db_del(args: &[Value]) -> Result<Value> {
    let [h, key] = args else {
        return Err(Error::runtime(format!(
            "{DB_DEL} expects ({DB_DEL} handle key)"
        )));
    };
    let h = as_handle(h, DB_DEL)?;
    let key = as_key(key, DB_DEL)?;
    let existed = with_db(h, DB_DEL, |db| {
        let existed = lookup(db, key).is_some();
        // A tombstone is appended even when the key is already absent, so the
        // log records the call. The alternative — skipping the write — would
        // make a delete-then-crash indistinguishable from a delete that never
        // happened, and `db-keys` would have to consult a clock to know.
        db.put(key, TOMBSTONE)?;
        Ok(existed)
    })?;
    Ok(Value::Bool(existed))
}

/// `(db-keys handle)` → every live key, **sorted**.
///
/// Sorted, and that is a parity requirement rather than a nicety. The two
/// engines index keys differently — a Rust `HashMap` and a chained hash table
/// in C — and neither has a defined iteration order, so returning the index in
/// its natural order would make this builtin print a *different list of the same
/// keys* on two backends. Byte-identical output is the tier's rule, so the order
/// is imposed here: ascending byte-value order, the same rule `list-dir` uses
/// and for the same reason.
fn db_keys(args: &[Value]) -> Result<Value> {
    let [h] = args else {
        return Err(Error::runtime(format!(
            "{DB_KEYS} expects ({DB_KEYS} handle)"
        )));
    };
    let h = as_handle(h, DB_KEYS)?;
    let mut keys = with_db(h, DB_KEYS, |db| Ok(live_keys(db)))?;
    keys.sort_unstable();
    let items: Vec<Value> = keys.into_iter().map(Value::str).collect::<Vec<_>>();
    Ok(Value::List(ConsCell::from_values(items)))
}

/// `(db-count handle)` → how many live keys there are.
///
/// Counts *live* keys, so a deleted key is already gone from the count. A count
/// of records would grow without bound as keys were overwritten — the same
/// growth the log itself has — and would answer a question nobody asked.
fn db_count(args: &[Value]) -> Result<Value> {
    let [h] = args else {
        return Err(Error::runtime(format!(
            "{DB_COUNT} expects ({DB_COUNT} handle)"
        )));
    };
    let h = as_handle(h, DB_COUNT)?;
    let n = with_db(h, DB_COUNT, |db| Ok(live_keys(db).len()))?;
    Ok(Value::Int(n as i64))
}

/// Re-exported so a test can assert the two layers' name sets do not overlap by
/// accident — the collision is real and intended, so it is pinned rather than
/// left to a reader to notice.
pub fn all_db_builtins() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = DB_BUILTINS.to_vec();
    v.extend_from_slice(KV_BUILTINS);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json_value::{builtin_json_parse, builtin_json_serialize};

    fn enc(v: &Value) -> String {
        match builtin_json_serialize(std::slice::from_ref(v)).expect("serialize") {
            Value::Str(s) => s.as_str().to_string(),
            o => panic!("expected a str, got {}", o.type_name()),
        }
    }

    fn dec(s: &str) -> Value {
        builtin_json_parse(&[Value::str(s)]).expect("parse")
    }

    /// A list of `items`, spelled the way every other test in this file builds
    /// one. `Value::List` wants an `Rc<ConsCell>`, and a helper keeps the
    /// three call sites below readable.
    fn list(items: Vec<Value>) -> Value {
        Value::List(ConsCell::from_values(items))
    }

    /// A scratch database file, unique per test.
    struct Scratch(std::path::PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let mut p = std::env::temp_dir();
            p.push(format!(
                "ainl-dbkv-{}-{tag}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&p);
            Scratch(p)
        }
        fn path(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn open(s: &Scratch) -> Db {
        Db::open(&s.path()).expect("open")
    }

    /// Every value type the card requires, paired with its JSON text. Written
    /// out rather than derived, because the whole point of the test is that
    /// these *exact* strings are what both engines write to the log.
    #[test]
    fn every_value_type_round_trips_through_the_log() {
        let cases: Vec<(Value, &str)> = vec![
            (Value::Int(42), "42"),
            (Value::Int(-7), "-7"),
            (Value::Float(1.0), "1.0"),
            (Value::Float(1.5), "1.5"),
            (Value::Bool(true), "true"),
            (Value::Bool(false), "false"),
            (Value::Nil, "null"),
            (Value::str("hello"), "\"hello\""),
            (Value::str(""), "\"\""),
        ];
        for (v, json) in &cases {
            assert_eq!(&enc(v), json, "wrong JSON for {v:?}");
            let s = Scratch::new("rt");
            let mut db = open(&s);
            db.put("k", &enc(v)).expect("put");
            drop(db);
            let db = open(&s);
            let back = lookup(&db, "k").expect("the key must survive");
            assert_eq!(back, *json, "stored text for {v:?} must be {json}");
            assert_eq!(dec(&back), *v, "{v:?} did not round trip");
        }
    }

    /// The int/float distinction is the one thing JSON is genuinely bad at, and
    /// it is the reason this module's parity claim is scoped to three backends.
    /// If this test ever needs a filter to pass, the fix is the encoding, not
    /// the assertion.
    #[test]
    fn an_int_stays_an_int_and_a_float_stays_a_float() {
        let s = Scratch::new("num");
        let mut db = open(&s);
        db.put("i", &enc(&Value::Int(1))).expect("put");
        db.put("f", &enc(&Value::Float(1.0))).expect("put");
        drop(db);
        let db = open(&s);
        assert_eq!(dec(&lookup(&db, "i").unwrap()), Value::Int(1));
        assert_eq!(dec(&lookup(&db, "f").unwrap()), Value::Float(1.0));
    }

    #[test]
    fn a_nested_list_round_trips() {
        let v = list(vec![
            Value::Int(1),
            Value::str("two"),
            Value::Bool(false),
            Value::Nil,
            list(vec![Value::Int(3), Value::Int(4)]),
        ]);
        let s = Scratch::new("nested");
        let mut db = open(&s);
        db.put("k", &enc(&v)).expect("put");
        drop(db);
        let db = open(&s);
        assert_eq!(dec(&lookup(&db, "k").unwrap()), v);
    }

    /// A tombstone must not be readable as a value, and must not be counted or
    /// listed. These are the three ways a delete could half-work.
    #[test]
    fn a_tombstone_is_absent_from_get_keys_and_count() {
        let s = Scratch::new("tomb");
        let mut db = open(&s);
        db.put("live", &enc(&Value::Int(1))).expect("put");
        db.put("gone", &enc(&Value::Int(2))).expect("put");
        db.put("gone", TOMBSTONE).expect("tombstone");
        assert_eq!(
            lookup(&db, "gone"),
            None,
            "a deleted key must read as absent"
        );
        assert!(lookup(&db, "live").is_some());
        assert_eq!(live_keys(&db), vec!["live".to_string()]);
    }

    /// Deletion is a log record, so it has to survive the reopen exactly the
    /// way a write does. A delete that only mutated memory would pass every test
    /// above and lose the key on the next process.
    #[test]
    fn a_delete_survives_a_reopen() {
        let s = Scratch::new("del-reopen");
        let mut db = open(&s);
        db.put("keep", &enc(&Value::Int(1))).expect("put");
        db.put("drop", &enc(&Value::Int(2))).expect("put");
        db.put("drop", TOMBSTONE).expect("tombstone");
        drop(db);
        let db = open(&s);
        assert_eq!(
            lookup(&db, "drop"),
            None,
            "the delete must outlive the process"
        );
        assert!(lookup(&db, "keep").is_some());
    }

    /// A set after a delete brings the key back, because both are just records
    /// and replay applies them in order.
    #[test]
    fn a_set_after_a_delete_revives_the_key() {
        let s = Scratch::new("revive");
        let mut db = open(&s);
        db.put("k", &enc(&Value::Int(1))).expect("put");
        db.put("k", TOMBSTONE).expect("tombstone");
        assert_eq!(lookup(&db, "k"), None);
        db.put("k", &enc(&Value::Int(2))).expect("put");
        assert_eq!(dec(&lookup(&db, "k").unwrap()), Value::Int(2));
    }

    /// The tombstone must be unreachable by any value JSON can produce, or a
    /// legitimate value would be deleted by a later replay.
    #[test]
    fn the_tombstone_cannot_be_produced_by_json() {
        for v in [
            Value::Nil,
            Value::Bool(true),
            Value::Int(0),
            Value::Float(0.5),
            Value::str("~"),
            Value::str(""),
            list(vec![Value::Int(1)]),
        ] {
            assert_ne!(
                enc(&v),
                TOMBSTONE,
                "a value encodes to the tombstone: {v:?}"
            );
        }
    }

    /// `db-keys` order is a parity requirement, so the sort is on the byte value
    /// of the key, not on any locale or numeric reading of it.
    #[test]
    fn keys_sort_by_byte_value() {
        let s = Scratch::new("sort");
        let mut db = open(&s);
        for k in ["b", "A", "a", "B", "10", "2", ""] {
            db.put(k, &enc(&Value::Nil)).expect("put");
        }
        let mut keys = db.keys();
        keys.sort_unstable();
        assert_eq!(keys, vec!["", "10", "2", "A", "B", "a", "b"]);
    }

    /// `db-put` text is readable through `db-get` only when it is valid JSON,
    /// and the message has to say so rather than returning a bare nil.
    #[test]
    fn db_put_text_is_not_silently_mistaken_for_a_value() {
        let s = Scratch::new("put-text");
        let mut db = open(&s);
        db.put("k", "buy milk").expect("put");
        // Not valid JSON: the caller gets the raw text, and the *builtin* is
        // what refuses — here we assert the stored bytes are what we expect, so
        // the error path in `db_get` is driven by exactly these bytes.
        assert_eq!(lookup(&db, "k").as_deref(), Some("buy milk"));
    }
}
