//! `db-create-table` / `db-insert` / `db-select` / `db-delete-row` /
//! `db-all-rows` — structured tables with a B-tree primary-key index.
//!
//! # What this layer is
//!
//! [`crate::db`] is a *byte* store, [`crate::dbkv`] a *value* store, and both
//! are flat: one namespace of string keys to one value. This layer adds
//! **tables** — named sets of rows, each row an AINL list, each table indexed on
//! the row's first column by [`crate::btree`] — so a lookup is O(log n) and
//! `db-all-rows` comes back in key order without sorting.
//!
//! # It sits on the existing log, and that is the design decision
//!
//! The alternatives were a second file or a second record type. Both are worse,
//! and for the same reason: the log already has a checksum, a torn-tail recovery
//! and a replay, and a second write path is a second thing that has to reproduce
//! all three. So a table is a **reserved region of the same key space** and
//! `db-insert` writes the same record `db-set` does — which is also why a
//! compiled binary and the interpreter can read each other's files.
//!
//! # The namespace, and why a NUL is the right separator
//!
//! A table row reaches the log as a string, under the key
//!
//! ```text
//! "@t:" + <len(table_name)> + ":" + table_name + primary_key_json
//! ```
//!
//! The `@t:` prefix marks the record as a table row rather than a KV pair, and
//! the length prefix makes the rest unambiguous. **Both halves were arrived at
//! by getting the first design wrong**, and the wrong version is worth reading
//! because each half looks obviously right:
//!
//! * The first design was `"\0T" + name + "\0" + key`, on the reasoning that a
//!   NUL is unreachable from a string literal, so no program could ever forge a
//!   row key or make two `(table, key)` pairs collide. The reasoning was sound
//!   and the conclusion was still false twice over. The log **drops every
//!   record containing a NUL** (see [`crate::db`], and `db_usable` in the C
//!   runtime) — a value travelling through the C port's `char *` would
//!   otherwise truncate there and not in Rust — so a NUL-prefixed key is not
//!   merely unguessable, it is *unstorable*, and every row silently vanished on
//!   the next open. And even in memory, `split_once` on the first NUL collapses
//!   table `a` with key `b\0c` into table `a\0b` with key `c`. The property the
//!   design needed was not "a program cannot write this", it was "this is
//!   storable and this is unambiguous", and only the second question was asked.
//! * The `@t:` marker is storable, but a program *can* write it — it is
//!   ordinary text, so `@t:3:abc1` is a legal `db-set` key. That is harmless
//!   because the length prefix disambiguates the encoding, and harmless again
//!   because `rebuild` only ever reads keys it recognises. The marker keeps the
//!   two layers structurally apart for a reader; it is not the thing that makes
//!   them safe.
//!
//! # What a row is, and what its key is
//!
//! A row is an AINL **list**, stored JSON-encoded through the same
//! [`crate::json_value`] writer the value layer uses: no new serialization
//! format, and a row holding a nested list or map round-trips with the value
//! layer's existing guarantees.
//!
//! The **primary key is the first element**, stored as its *JSON text*. That is
//! what lets a table have an integer key, and it is why the order is JSON byte
//! order: `1`, `10`, `2` come back in that order, because that is how their
//! bytes compare. It is a *stable, defined* order, which is the property the
//! parity claim needs — see `btree::Node::search` on why bytes and not numbers.
//!
//! Primary keys must be **scalars** (`int`, `float`, `str`, `bool`, `nil`). A
//! list or map has no single byte form that both engines would order
//! identically, and a primary key that cannot be ordered is an index that
//! cannot be walked. That is refused with its own message rather than by
//! stringifying whatever arrived.
//!
//! # A table that has no rows still has to survive a reopen
//!
//! `rebuild` learns which tables exist from the row records it finds, so a table
//! with no rows would leave nothing behind and vanish. `db-create-table`
//! therefore appends one **marker record**: a row whose primary key is a
//! character the JSON writer can never produce for a user value, and which
//! `db-all-rows` filters out. The alternative — a dedicated "create table"
//! record type — is the second record type this design exists to avoid, so the
//! marker is one, discriminated by its key, like every other record.
//!
//! # The tree is rebuilt, not serialized
//!
//! `db-delete-row` appends the same tombstone [`crate::dbkv`] uses, so a delete
//! survives a crash through the same replay a write does: one recovery path, not
//! two. What is persisted in the db file, as the card asks, is the log records
//! the index is built from — the rows, the primary keys, and therefore the
//! order; a reopen rebuilds an identical tree. Writing the node structure into
//! the file instead would be a second write path *and* a second recovery path,
//! because a half-written interior node is a hole in the middle of the file —
//! precisely the failure the append-only log exists to make impossible.
//!
//! # Tables and KV keys share one file, on purpose
//!
//! A program may keep `db-set` keys and table rows in one database and both
//! survive a reopen, because they are the same records under the same replay.
//! That is what makes sharing the log safe rather than merely convenient:
//! nothing here can read a KV key as a table name, or a table row as a KV value.

use crate::btree::BTree;
use crate::dbkv::TOMBSTONE;
use crate::error::{Error, Result};
use crate::eval::Env;
use crate::json_value;
use crate::value::{ConsCell, Value};

pub const DB_CREATE_TABLE: &str = "db-create-table";
pub const DB_INSERT: &str = "db-insert";
pub const DB_SELECT: &str = "db-select";
pub const DB_DELETE_ROW: &str = "db-delete-row";
pub const DB_ALL_ROWS: &str = "db-all-rows";

/// Every name this layer adds, in the order the refusal scanner reports them.
/// One list, so the scanner, the docs and the tests cannot disagree about which
/// symbols are the table layer.
pub const TABLE_BUILTINS: &[&str] = &[
    DB_CREATE_TABLE,
    DB_INSERT,
    DB_SELECT,
    DB_DELETE_ROW,
    DB_ALL_ROWS,
];

/// Marks a log key as a table row.
///
/// A NUL is the obvious choice for a marker byte and this is the record of why
/// it is the **wrong** one — the first design of this module used `\0T` and both
/// of its reasons turned out to be backwards:
///
/// 1. The log **drops any record containing a NUL** (see [`crate::db`], and
///    `db_usable` in the C runtime), because a value travelling through the C
///    port's `char *` would otherwise truncate there and not in Rust. So a
///    NUL-prefixed key is not merely unusual, it is *unstorable* — every row
///    vanished on the next open. A namespacing scheme has to survive the log,
///    and the log's own restriction is the constraint that decides it.
/// 2. Splitting a NUL-separated key on the *first* NUL is ambiguous anyway:
///    table `a` + key `b\0c` and table `a\0b` + key `c` encode identically, so
///    the separator was never unambiguous in the first place.
///
/// What replaces it is a marker that is storable, plus a **length-prefixed**
/// encoding that is self-delimiting — see [`row_key`]. A program can write a
/// table name containing the marker (it is ordinary text), so the marker alone
/// is not a guarantee; the length prefix is what makes the encoding
/// unambiguous, and the marker is what keeps the two layers' keys visually and
/// structurally apart.
const ROW_PREFIX: &str = "@t:";

/// The primary key of the marker record that records "this table exists".
///
/// An empty key. A row's key is always at least two bytes of JSON (`"a"`, `1`,
/// `null`), so the empty string is the one key no user row can have — which is
/// what makes the marker unreachable as a row without a second reserved value.
pub const TABLE_MARKER_KEY: &str = "";

/// The log key for `(table, primary_key_json)`.
///
/// **Length-prefixed**, which is what makes the encoding unambiguous for *any*
/// table name and *any* key, including both containing the separator. The name
/// is preceded by its byte length in decimal and a `:`, so a reader knows
/// exactly how many bytes the name is and cannot be misled by a separator
/// character inside it:
///
/// ```text
/// "@t:" + <len(table)> + ":" + table + key_json
/// ```
///
/// The first attempt at this encoding joined the two parts with a NUL, on the
/// reasoning that a NUL is unreachable from a string literal. That reasoning was
/// wrong twice over: the log cannot *store* a NUL, so nothing written that way
/// ever came back, and `split_once` on the first NUL collapses
/// `(a, "b\0c")` into `(a\0b, "c")` regardless. A length prefix has neither
/// problem, and it is six characters of code rather than a proof about the
/// language's escape table.
///
/// The cost is that a key is not human-readable in a hex dump — the table name
/// is there, but preceded by its length. That is the right trade: a reader who
/// cannot parse the key can still use `db-all-rows`, and a reader who can parse
/// it never gets a wrong answer.
/// Public because the query layer's tests build a table directly, and a fixture
/// that spells the prefix format out in a second place is one that keeps
/// passing after the encoding changes.
pub fn row_key(table: &str, key_json: &str) -> String {
    format!("{ROW_PREFIX}{}:{table}{key_json}", table.len())
}

/// The `(table, key_json)` a log key denotes, or `None` if it is not a row key.
fn parse_row_key(k: &str) -> Option<(&str, &str)> {
    let rest = k.strip_prefix(ROW_PREFIX)?;
    let colon = rest.find(':')?;
    let len: usize = rest[..colon].parse().ok()?;
    let name = rest.get(colon + 1..)?;
    let table = name.get(..len)?;
    // `get(..len)` returning `Some` already proves the boundary is on a
    // character edge, so slicing the rest cannot panic on a non-ASCII name.
    let key_json = name.get(len..)?;
    Some((table, key_json))
}

// ---- argument helpers ------------------------------------------------------

/// The handle operand, with the same message every other layer uses, so a
/// program that mixes them does not see three spellings of "handle 4 is not
/// open".
fn as_handle(v: &Value, who: &str) -> Result<i64> {
    match v {
        Value::Int(n) => n.as_i64().ok_or_else(|| {
            Error::runtime(format!("{who} expects a db handle, got {}", v.type_name()))
        }),
        other => Err(Error::runtime(format!(
            "{who} expects a db handle, got {}",
            other.type_name()
        ))),
    }
}

fn as_str<'a>(v: &'a Value, who: &str, what: &str) -> Result<&'a str> {
    match v {
        Value::Str(s) => Ok(s),
        other => Err(Error::runtime(format!(
            "{who} expects a str {what}, got {}",
            other.type_name()
        ))),
    }
}

/// A table name.
///
/// No character is refused: the key encoding is length-prefixed, so a name may
/// contain anything the language can put in a string — including the `@t:`
/// marker and the `:` that follows the length. The one thing that would break
/// it is a name whose **byte** length disagrees with where it ends, and that is
/// not a property of the name at all but of the encoding, which writes the
/// length itself. So this is a plain string check, and the interesting
/// validation lives in [`row_key`] / [`parse_row_key`], which are tested
/// directly.
fn table_name(v: &Value, who: &str) -> Result<String> {
    Ok(as_str(v, who, "table name")?.to_string())
}

// ---- the JSON text of a primary key ---------------------------------------

/// The primary key's JSON text, or an error naming what is wrong with it.
///
/// The only place a key is validated, and it validates two things: that the
/// value is a **scalar**, so it has one byte form both engines order
/// identically.
///
/// Nothing is refused for its *bytes*. The JSON writer escapes a NUL as `\u0000`
/// — six printable characters, no NUL — so the text is storable whatever the
/// key contains, and the length prefix in [`row_key`] makes the encoding
/// unambiguous regardless. That is a deliberate improvement on the NUL-separated
/// first design, which had to reject a key the log could not have stored anyway.
pub fn key_json(v: &Value, who: &str) -> Result<String> {
    match v {
        Value::List(_) | Value::Map(_) => {
            return Err(Error::runtime(format!(
                "{who}: primary key cannot be a {} — a table is indexed on one \
                 column, and a composite or unordered key has no order to index by",
                v.type_name()
            )))
        }
        Value::Closure(_) | Value::Builtin { .. } | Value::Sym(_) => {
            return Err(Error::runtime(format!(
                "{who}: primary key cannot be a {}",
                v.type_name()
            )))
        }
        _ => {}
    }
    encode_row(v, who)
}

/// `json-serialize` as a `String`, with the internal-error guard both layers
/// share.
///
/// Public because the query layer stores nothing of its own but still has to
/// read a row, and a *second* JSON reader here would be a second thing that can
/// disagree with the first about what a stored row is.
pub fn encode_row(v: &Value, who: &str) -> Result<String> {
    match json_value::builtin_json_serialize(std::slice::from_ref(v))? {
        Value::Str(s) => Ok(s.as_str().to_string()),
        other => Err(Error::runtime(format!(
            "{who}: internal: json-serialize returned a {}",
            other.type_name()
        ))),
    }
}

/// `json-parse` of stored text, with a message that says where the text came
/// from rather than repeating the parser's complaint about a byte offset inside
/// a record the caller never wrote.
/// The same reader, for the query layer: `(text, builtin, "<what this text is>")`.
///
/// `what` is the caller's phrase, so `db-select` says "the row for key …" and
/// `db-query` says "a row of 'people'" without either message knowing about the
/// other. One reader, two callers, and no second place where a stored row can be
/// turned into a value.
pub fn decode_row(text: &str, who: &str, what: &str) -> Result<Value> {
    let v = Value::str(text.to_string());
    json_value::builtin_json_parse(std::slice::from_ref(&v)).map_err(|_| {
        Error::runtime(format!(
            "{who}: {what} is not readable as an AINL value ({text}); it was not \
             written by {DB_INSERT}"
        ))
    })
}

/// A `db-insert` operand: a non-empty list of columns.
fn as_row(v: &Value, who: &str) -> Result<Vec<Value>> {
    let Value::List(cell) = v else {
        return Err(Error::runtime(format!(
            "{who} expects a list row, got {}",
            v.type_name()
        )));
    };
    let mut out = Vec::with_capacity(cell.len);
    let mut i = 0;
    while let Some(x) = cell.nth(i) {
        out.push(x.clone());
        i += 1;
    }
    if out.is_empty() {
        return Err(Error::runtime(format!(
            "{who}: a row needs a primary key, so it cannot be empty"
        )));
    }
    Ok(out)
}

fn list_value(items: Vec<Value>) -> Value {
    Value::List(ConsCell::from_values(items))
}

// ---- the per-database table set -------------------------------------------

/// Every table in one open database, each a B-tree from a row's JSON primary
/// key to the row's JSON text.
///
/// One set per **handle**, not per table name, so the same table name in two
/// files cannot interact at all.
#[derive(Default)]
pub struct TableSet {
    tables: std::collections::BTreeMap<String, BTree>,
}

impl TableSet {
    /// Build the set from a replayed log, in file order.
    ///
    /// Records are applied in order and a later record for the same key wins, so
    /// an insert followed by a delete resolves the same way the live table
    /// would. `index` is the log's own map of key to latest value, so this is a
    /// walk of what survived replay — a torn or corrupt record is already gone
    /// before this sees it, which is what keeps the two engines agreeing.
    pub fn rebuild(index: &std::collections::HashMap<String, String>) -> TableSet {
        let mut set = TableSet::default();
        // Sorted, so a table's own records are applied in key order. This does
        // not change the *result* — the tree replaces by key, and a later
        // record for a different key is independent — but it makes the build
        // deterministic rather than dependent on the hash map's iteration
        // order, which is the whole class of bug §3l documents for `db-keys`.
        let mut keys: Vec<&String> = index.keys().collect();
        keys.sort_unstable();
        for k in keys {
            let Some((table, key_json)) = parse_row_key(k) else {
                continue;
            };
            let value = index[k].as_str();
            let t = set.tables.entry(table.to_string()).or_default();
            if key_json == TABLE_MARKER_KEY {
                // The "this table exists" record. It is not a row, so it is
                // never put in the tree — it only keeps the table from
                // disappearing when it has no rows.
                continue;
            }
            t.insert(key_json, value);
        }
        set
    }

    /// Every table name, in byte order. Used by the tests and by nothing the
    /// language can reach — there is deliberately no `db-tables` builtin on this
    /// card, for the same reason there is no second index.
    pub fn names(&self) -> Vec<String> {
        self.tables.keys().cloned().collect()
    }

    /// Create `name`, or return it if it already exists.
    ///
    /// Idempotent rather than an error, because the shape a program actually
    /// wants is "make sure this table exists" at the top of a program that may
    /// be re-run against an existing file. A refusal would make the idempotent
    /// program the hard case and the accidental duplicate the easy one.
    pub fn create(&mut self, name: &str) {
        self.tables.entry(name.to_string()).or_default();
    }

    /// Check that `name` exists, without creating it.
    ///
    /// The non-mutating half of [`TableSet::put`], for the caller that has to
    /// know "no such table" *before* it writes anything. Split out so the
    /// "refused means no record was written" rule in [`db_insert`] is one call
    /// rather than an ordering argument someone can reorder.
    pub fn require(&self, name: &str) -> bool {
        self.tables.contains_key(name)
    }

    fn tree_mut(&mut self, name: &str, who: &str) -> Result<&mut BTree> {
        if !self.tables.contains_key(name) {
            return Err(Error::runtime(format!(
                "{who}: no table named '{name}' in this database"
            )));
        }
        Ok(self.tables.get_mut(name).expect("just checked"))
    }

    /// The stored JSON text of the row with primary key `key_json`, or `None` if
    /// the row is absent or deleted.
    pub fn row(&self, table: &str, key_json: &str, who: &str) -> Result<Option<String>> {
        let t = self.tables.get(table).ok_or_else(|| no_table(table, who))?;
        match t.get(key_json) {
            Some(TOMBSTONE) | None => Ok(None),
            Some(v) => Ok(Some(v.to_string())),
        }
    }

    /// Store `row_json` under `key_json`, replacing any row with that key.
    ///
    /// The B-tree replaces in place, so a re-inserted primary key does not grow
    /// the table and `db-all-rows` cannot list a key twice.
    pub fn put(&mut self, table: &str, key_json: &str, row_json: &str) -> Result<()> {
        self.tree_mut(table, DB_INSERT)?.insert(key_json, row_json);
        Ok(())
    }

    /// Remove the row with primary key `key_json`. Returns whether it existed.
    pub fn remove(&mut self, table: &str, key_json: &str, who: &str) -> Result<bool> {
        Ok(self.tree_mut(table, who)?.remove(key_json))
    }

    /// Every live row's JSON text, **in primary-key order**.
    ///
    /// The order is the in-order walk, so it is a property of the tree rather
    /// than a sort applied here — which is what lets the two engines agree on
    /// it without either one sorting. Tombstones are filtered here rather than
    /// inside the tree, for the same reason [`crate::dbkv`] filters them in
    /// `db-keys`: a tombstone is the value layer's concept, and the B-tree has no
    /// business knowing it.
    pub fn rows(&self, table: &str, who: &str) -> Result<Vec<String>> {
        let t = self.tables.get(table).ok_or_else(|| no_table(table, who))?;
        Ok(t.iter()
            .into_iter()
            .map(|(_, v)| v.to_string())
            .filter(|v| v != TOMBSTONE)
            .collect())
    }
}

fn no_table(table: &str, who: &str) -> Error {
    Error::runtime(format!("{who}: no table named '{table}' in this database"))
}

// ---- the builtins ----------------------------------------------------------

/// Bind the five table builtins.
///
/// Installed after [`crate::dbkv`], and it does not rebind any name the earlier
/// layers own — the table layer adds five new ones and takes nothing. That is
/// deliberate: the tier already has one name collision to reason about
/// (`db-get`), and the cheapest way to avoid a second is not to share names.
pub fn install(env: &Env) {
    macro_rules! b {
        ($name:expr, $f:expr) => {
            env.define($name, Value::Builtin { name: $name, f: $f });
        };
    }
    b!(DB_CREATE_TABLE, db_create_table);
    b!(DB_INSERT, db_insert);
    b!(DB_SELECT, db_select);
    b!(DB_DELETE_ROW, db_delete_row);
    b!(DB_ALL_ROWS, db_all_rows);
}

/// `(db-create-table handle name)` → the table name.
///
/// Returns the name so one form binds it:
///
/// ```ainl
/// (def people (db-create-table h "people"))
/// ```
///
/// which is what makes the later calls read as `db-insert people` rather than
/// repeating the string at every site. Idempotent — see [`TableSet::create`].
fn db_create_table(args: &[Value]) -> Result<Value> {
    let [h, name] = args else {
        return Err(Error::runtime(format!(
            "{DB_CREATE_TABLE} expects ({DB_CREATE_TABLE} handle name)"
        )));
    };
    let h = as_handle(h, DB_CREATE_TABLE)?;
    let name = table_name(name, DB_CREATE_TABLE)?;
    with_db(h, DB_CREATE_TABLE, |db| {
        db.tables().create(&name);
        // The marker record, so an empty table survives a reopen. Written
        // through the byte layer's own `put`, so it is a normal log record with
        // a normal checksum and normal crash semantics.
        db.put(&row_key(&name, TABLE_MARKER_KEY), "")?;
        Ok(())
    })?;
    Ok(Value::str(name))
}

/// `(db-insert handle table row)` → `nil`.
///
/// The row must be a non-empty list whose first element is the primary key. A
/// re-insert of an existing key replaces the row — the same last-write-wins
/// rule the KV layer documents, and the reason the B-tree replaces rather than
/// duplicating.
fn db_insert(args: &[Value]) -> Result<Value> {
    let [h, table, row] = args else {
        return Err(Error::runtime(format!(
            "{DB_INSERT} expects ({DB_INSERT} handle table row)"
        )));
    };
    let h = as_handle(h, DB_INSERT)?;
    let table = as_str(table, DB_INSERT, "table name")?;
    let row = as_row(row, DB_INSERT)?;
    // The key first: a row whose primary key is unorderable is refused before
    // anything is written, so a bad `db-insert` leaves no record behind.
    let key = key_json(&row[0], DB_INSERT)?;
    let encoded = encode_row(&list_value(row), DB_INSERT)?;
    with_db(h, DB_INSERT, |db| {
        // The table is checked **before** the record is written, not after. The
        // first version wrote the log record and only then asked the table
        // whether it existed, so an insert into a missing table refused the
        // program *and* left a row in the log — a refusal that changes the file
        // is a bug in its own right, and on the next open the row would be
        // there with the table the record itself created. The C port checks
        // first, and this is where the two engines agreed to differ until they
        // did not.
        //
        // Validating everything before writing is the general rule here: a
        // refused call must leave no trace, or the refusal is not idempotent.
        if !db.tables().require(table) {
            return Err(no_table(table, DB_INSERT));
        }
        db.put(&row_key(table, &key), &encoded)?;
        db.tables().put(table, &key, &encoded)?;
        Ok(())
    })?;
    Ok(Value::Nil)
}

/// `(db-select handle table key)` → the row, or `nil` if there is none.
///
/// `key` is the **primary key value**, not a row: `(db-select h "people" 1)`, not
/// `(db-select h "people" (list 1 "ada"))`. That is what makes this a lookup
/// rather than a re-encode-and-compare, and it is the same value the caller
/// passed as the row's first element to `db-insert`.
fn db_select(args: &[Value]) -> Result<Value> {
    let [h, table, key] = args else {
        return Err(Error::runtime(format!(
            "{DB_SELECT} expects ({DB_SELECT} handle table key)"
        )));
    };
    let h = as_handle(h, DB_SELECT)?;
    let table = as_str(table, DB_SELECT, "table name")?;
    let key = key_json(key, DB_SELECT)?;
    let stored = with_db(h, DB_SELECT, |db| db.tables().row(table, &key, DB_SELECT))?;
    match stored {
        None => Ok(Value::Nil),
        Some(text) => decode_row(
            &text,
            DB_SELECT,
            &format!("the row for key {key} in '{table}'"),
        ),
    }
}

/// `(db-delete-row handle table key)` → `true` if the row was there.
///
/// A boolean for the same reason `db-del` returns one: the difference between
/// "I removed it" and "there was nothing there". Deleting an absent row is not
/// an error — it is the idempotent answer, and it is what makes a cleanup pass
/// safe to run twice.
fn db_delete_row(args: &[Value]) -> Result<Value> {
    let [h, table, key] = args else {
        return Err(Error::runtime(format!(
            "{DB_DELETE_ROW} expects ({DB_DELETE_ROW} handle table key)"
        )));
    };
    let h = as_handle(h, DB_DELETE_ROW)?;
    let table = as_str(table, DB_DELETE_ROW, "table name")?;
    let key = key_json(key, DB_DELETE_ROW)?;
    let existed = with_db(h, DB_DELETE_ROW, |db| {
        let had = db.tables().row(table, &key, DB_DELETE_ROW)?.is_some();
        if had {
            // The same tombstone the value layer writes, so "gone" means one
            // thing in the log rather than two — and so the C port, which
            // compares against its own copy of that one string, agrees.
            db.put(&row_key(table, &key), TOMBSTONE)?;
            db.tables().remove(table, &key, DB_DELETE_ROW)?;
        }
        Ok(had)
    })?;
    Ok(Value::Bool(existed))
}

/// `(db-all-rows handle table)` → every row, **sorted by primary key**.
///
/// The order is the tree's own in-order walk, not a sort applied here.
fn db_all_rows(args: &[Value]) -> Result<Value> {
    let [h, table] = args else {
        return Err(Error::runtime(format!(
            "{DB_ALL_ROWS} expects ({DB_ALL_ROWS} handle table)"
        )));
    };
    let h = as_handle(h, DB_ALL_ROWS)?;
    let table = as_str(table, DB_ALL_ROWS, "table name")?;
    let texts = with_db(h, DB_ALL_ROWS, |db| db.tables().rows(table, DB_ALL_ROWS))?;
    let mut rows = Vec::with_capacity(texts.len());
    for t in texts {
        rows.push(decode_row(&t, DB_ALL_ROWS, &format!("a row of '{table}'"))?);
    }
    Ok(list_value(rows))
}

/// Borrow the handle for a table operation, or explain that it is not open.
fn with_db<T>(h: i64, who: &str, f: impl FnOnce(&mut crate::db::Db) -> Result<T>) -> Result<T> {
    crate::db::with_db(h, who, f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bignum::BigNum;
    use crate::db::Db;

    /// A scratch database file, unique per test, removed on drop.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let mut p = std::env::temp_dir();
            p.push(format!(
                "ainl-dbtab-{}-{tag}-{:?}",
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

    /// A row of strings, the shape most tests use.
    fn row(items: &[&str]) -> Value {
        list_value(items.iter().map(|s| Value::str(*s)).collect())
    }

    /// A row whose columns are mixed, so a round trip has to preserve types.
    fn mixed_row() -> Value {
        list_value(vec![
            Value::Int(BigNum::small(7)),
            Value::str("ada"),
            Value::Float(1.5),
            Value::Bool(true),
            Value::Nil,
        ])
    }

    /// Create a table and return it, so a test reads as one line per fixture.
    fn with_table(db: &mut Db, name: &str) {
        db.tables().create(name);
        db.put(&row_key(name, TABLE_MARKER_KEY), "")
            .expect("marker");
    }

    fn insert(db: &mut Db, table: &str, r: Value) {
        let cols = as_row(&r, DB_INSERT).expect("row");
        let key = key_json(&cols[0], DB_INSERT).expect("key");
        let encoded = encode_row(&r, DB_INSERT).expect("encode");
        db.put(&row_key(table, &key), &encoded).expect("put");
        db.tables().put(table, &key, &encoded).expect("tree");
    }

    /// A read helper that takes `&mut Db` because `Db::tables` hands out `&mut`
    /// (it builds the index lazily), so even a read has to go through a mutable
    /// borrow. Every caller below therefore takes `&mut db`.
    fn select(db: &mut Db, table: &str, key: &Value) -> Value {
        let k = key_json(key, DB_SELECT).expect("key");
        match db.tables().row(table, &k, DB_SELECT).expect("row") {
            None => Value::Nil,
            Some(t) => decode_row(&t, DB_SELECT, "row").expect("decode"),
        }
    }

    fn all_rows(db: &mut Db, table: &str) -> Vec<Value> {
        db.tables()
            .rows(table, DB_ALL_ROWS)
            .expect("rows")
            .iter()
            .map(|t| decode_row(t, DB_ALL_ROWS, "row").expect("decode"))
            .collect()
    }

    fn del(db: &mut Db, table: &str, key: &Value) -> bool {
        let k = key_json(key, DB_DELETE_ROW).expect("key");
        let had = db
            .tables()
            .row(table, &k, DB_DELETE_ROW)
            .expect("row")
            .is_some();
        if had {
            db.put(&row_key(table, &k), TOMBSTONE).expect("tombstone");
            db.tables()
                .remove(table, &k, DB_DELETE_ROW)
                .expect("remove");
        }
        had
    }

    // ---- the key encoding -------------------------------------------------

    /// The whole namespacing argument in one test: a row key starts with the
    /// NUL marker, no ordinary key can, and the table and the key are
    /// unambiguously separated.
    /// The encoding's whole contract: a row key round-trips to its `(table,
    /// key)` pair, and no two pairs encode to the same string.
    ///
    /// The collision cases are the ones a separator-based encoding gets wrong,
    /// and they are exactly why the length prefix is there: with a NUL
    /// separator, table `a` + key `b\0c` and table `a\0b` + key `c` produced the
    /// same key, and the *key* half of that pair is not even reachable from a
    /// program — only the *name* half is, which is enough to make the encoding
    /// ambiguous in principle and a latent bug the moment a name carried a
    /// separator.
    #[test]
    fn a_row_key_round_trips_and_never_collides() {
        assert_eq!(row_key("people", "\"1\""), "@t:6:people\"1\"");
        for (table, key) in [
            ("people", "\"1\""),
            ("", ""),
            ("a", "b:c"),
            ("a:b", "c"),
            ("a", "b\0c"),
            ("a\0b", "c"),
            // A name that *contains* the marker and the length delimiter, which
            // is legal now and was the reason the first design needed a
            // reserved separator.
            ("@t:3:x", "1"),
            ("日", "\"日本\""),
            ("with\nnewline", "null"),
        ] {
            let encoded = row_key(table, key);
            assert_eq!(
                parse_row_key(&encoded),
                Some((table, key)),
                "round trip failed for {table:?} / {key:?} (encoded {encoded:?})"
            );
        }
        // And the two pairs that a NUL separator would have merged.
        assert_ne!(row_key("a", "b:c"), row_key("a:b", "c"));
        assert_ne!(row_key("a", "b\0c"), row_key("a\0b", "c"));
    }

    /// The prefix keeps the table layer's keys structurally apart from the KV
    /// layer's, and a KV key that merely *looks* like a row key is never read as
    /// one by `rebuild` — because rebuild only accepts keys the encoding itself
    /// produced, and a `db-set` key is not one of them.
    #[test]
    fn an_ordinary_key_is_never_mistaken_for_a_row() {
        assert!(parse_row_key("theme").is_none());
        assert!(parse_row_key("Tpeople").is_none());
        assert!(parse_row_key("people\"1\"").is_none(), "no marker prefix");
        // A KV key the program wrote that happens to *start* with the marker.
        // It parses as a row key, which is harmless and worth being precise
        // about: the safety comes from the length prefix being self-consistent,
        // not from the marker being unguessable. A program can only produce this
        // by deliberately writing the encoding by hand, and `rebuild` will then
        // show a table for it — visible, not silent.
        assert_eq!(
            parse_row_key("@t:3:abc1"),
            Some(("abc", "1")),
            "the encoding is parseable, so a hand-written key parses too"
        );
        // A malformed one does not parse at all, and so cannot resurrect
        // anything: no length, a non-numeric length, a length past the end, or
        // a length that would cut a multi-byte character in half.
        assert!(parse_row_key("@t:abc").is_none());
        assert!(parse_row_key("@t:x:abc").is_none());
        assert!(parse_row_key("@t:99:abc").is_none());
        assert!(parse_row_key("@t:1:日").is_none(), "a length mid-character");
    }

    /// The marker record must not be reachable as a user row, and it must not
    /// show up in `db-all-rows`.
    #[test]
    fn the_empty_table_marker_is_not_a_row() {
        let s = Scratch::new("marker");
        let mut db = open(&s);
        with_table(&mut db, "empty");
        assert_eq!(
            db.tables().names(),
            vec!["empty".to_string()],
            "an empty table must still exist"
        );
        assert!(
            all_rows(&mut db, "empty").is_empty(),
            "the marker is not a row"
        );
        assert!(matches!(
            select(&mut db, "empty", &Value::Int(BigNum::small(1))),
            Value::Nil
        ));
    }

    // ---- the round trip ---------------------------------------------------

    #[test]
    fn insert_then_select_returns_that_row() {
        let s = Scratch::new("roundtrip");
        let mut db = open(&s);
        with_table(&mut db, "t");
        insert(&mut db, "t", row(&["1", "ada", "math"]));
        insert(&mut db, "t", row(&["2", "grace", "navy"]));
        assert_eq!(
            select(&mut db, "t", &Value::str("1")),
            row(&["1", "ada", "math"])
        );
        assert_eq!(
            select(&mut db, "t", &Value::str("2")),
            row(&["2", "grace", "navy"])
        );
        assert!(
            matches!(select(&mut db, "t", &Value::str("3")), Value::Nil),
            "absent is nil"
        );
    }

    /// Every column type the card requires, so a row is not accidentally
    /// stringified on the way through.
    #[test]
    fn a_row_of_mixed_columns_round_trips_exactly() {
        let s = Scratch::new("types");
        let mut db = open(&s);
        with_table(&mut db, "t");
        let r = mixed_row();
        insert(&mut db, "t", r.clone());
        assert_eq!(select(&mut db, "t", &Value::Int(BigNum::small(7))), r);
    }

    /// A re-insert replaces rather than duplicating — the property that lets a
    /// primary key *identify* a row.
    #[test]
    fn inserting_the_same_key_twice_replaces_the_row() {
        let s = Scratch::new("replace");
        let mut db = open(&s);
        with_table(&mut db, "t");
        insert(&mut db, "t", row(&["1", "first"]));
        insert(&mut db, "t", row(&["1", "second"]));
        assert_eq!(
            select(&mut db, "t", &Value::str("1")),
            row(&["1", "second"])
        );
        assert_eq!(all_rows(&mut db, "t").len(), 1, "not two rows");
    }

    // ---- ordering ---------------------------------------------------------

    /// `db-all-rows` is sorted, and the order is the **JSON byte order** of the
    /// primary keys.
    #[test]
    fn all_rows_comes_back_in_key_order() {
        let s = Scratch::new("order");
        let mut db = open(&s);
        with_table(&mut db, "t");
        for k in ["10", "2", "1", "b", "a", "B", ""] {
            insert(
                &mut db,
                "t",
                list_value(vec![Value::str(k), Value::Int(BigNum::small(1))]),
            );
        }
        let got: Vec<Value> = all_rows(&mut db, "t")
            .into_iter()
            .map(|r| as_row(&r, DB_ALL_ROWS).unwrap().remove(0))
            .collect();
        let want = ["", "1", "10", "2", "B", "a", "b"].map(Value::str);
        assert_eq!(got, want.to_vec());
    }

    /// Numeric keys sort as their **JSON text**, so the order is the byte order
    /// of `1`, `10`, `100`, `2` — which puts `10` before `2`.
    ///
    /// This is a deliberate property, not an accident of the test: the key is
    /// stored as text so that one comparator works for every scalar type on both
    /// engines, and the cost is that an integer key is *not* in numeric order.
    /// A table with integer keys that a program wants walked numerically is a
    /// table that wants a different key type (a zero-padded string) or a
    /// secondary index — and both are Tier 5 conversations, not something to
    /// bolt on here behind an inconsistent comparison.
    #[test]
    fn numeric_keys_sort_as_json_text_not_numerically() {
        let s = Scratch::new("numkeys");
        let mut db = open(&s);
        with_table(&mut db, "t");
        for n in [2, 10, 1, 100] {
            insert(&mut db, "t", list_value(vec![Value::Int(BigNum::small(n))]));
        }
        // Each row is the one-column list `[key]`, so the walk comes back as a
        // list of single-element lists.
        let got: Vec<Value> = all_rows(&mut db, "t");
        let keys: Vec<Value> = got
            .iter()
            .map(|r| as_row(r, DB_ALL_ROWS).unwrap()[0].clone())
            .collect();
        // Byte order of "1", "10", "100", "2" -> 1, 10, 100, 2. Note `10`
        // before `2`: the keys are compared as text, not as numbers.
        assert_eq!(
            keys,
            vec![
                Value::Int(BigNum::small(1)),
                Value::Int(BigNum::small(10)),
                Value::Int(BigNum::small(100)),
                Value::Int(BigNum::small(2))
            ]
        );
    }

    // ---- deletion ---------------------------------------------------------

    #[test]
    fn a_deleted_row_is_gone_from_select_and_all_rows() {
        let s = Scratch::new("del");
        let mut db = open(&s);
        with_table(&mut db, "t");
        insert(&mut db, "t", row(&["1", "a"]));
        insert(&mut db, "t", row(&["2", "b"]));
        assert!(del(&mut db, "t", &Value::str("1")));
        assert!(matches!(select(&mut db, "t", &Value::str("1")), Value::Nil));
        assert_eq!(all_rows(&mut db, "t"), vec![row(&["2", "b"])]);
        // Deleting again is the idempotent answer, and says so.
        assert!(!del(&mut db, "t", &Value::str("1")));
    }

    /// Deleting a row and re-inserting it must bring it back, because both are
    /// just log records applied in order.
    #[test]
    fn a_row_can_be_reinserted_after_deletion() {
        let s = Scratch::new("revive");
        let mut db = open(&s);
        with_table(&mut db, "t");
        insert(&mut db, "t", row(&["1", "old"]));
        del(&mut db, "t", &Value::str("1"));
        insert(&mut db, "t", row(&["1", "new"]));
        assert_eq!(select(&mut db, "t", &Value::str("1")), row(&["1", "new"]));
        assert_eq!(all_rows(&mut db, "t").len(), 1);
    }

    // ---- persistence ------------------------------------------------------

    /// The card's persistence claim, at the engine level: rows, keys, the
    /// *order* and the table itself all come back, and the index is rebuilt
    /// rather than stored.
    #[test]
    fn rows_and_order_survive_a_reopen() {
        let s = Scratch::new("reopen");
        let mut db = open(&s);
        with_table(&mut db, "t");
        with_table(&mut db, "other");
        for k in ["c", "a", "b"] {
            insert(&mut db, "t", row(&[k, "v"]));
        }
        let before = all_rows(&mut db, "t");
        drop(db);

        let mut db = open(&s);
        assert_eq!(
            all_rows(&mut db, "t"),
            before,
            "the rebuilt index must walk in the same order"
        );
        assert_eq!(select(&mut db, "t", &Value::str("a")), row(&["a", "v"]));
        assert_eq!(
            db.tables().names(),
            vec!["other".to_string(), "t".to_string()],
            "both tables survive, including the empty one"
        );
    }

    /// A delete has to survive the reopen too, or it only ever worked in memory.
    #[test]
    fn a_delete_survives_a_reopen() {
        let s = Scratch::new("del-reopen");
        let mut db = open(&s);
        with_table(&mut db, "t");
        insert(&mut db, "t", row(&["keep", "1"]));
        insert(&mut db, "t", row(&["drop", "2"]));
        del(&mut db, "t", &Value::str("drop"));
        drop(db);

        let mut db = open(&s);
        assert!(matches!(
            select(&mut db, "t", &Value::str("drop")),
            Value::Nil
        ));
        assert_eq!(all_rows(&mut db, "t"), vec![row(&["keep", "1"])]);
    }

    /// The last write wins after a reopen, which is the same rule replay applies
    /// everywhere else in the log.
    #[test]
    fn the_last_write_of_a_key_wins_after_a_reopen() {
        let s = Scratch::new("lww");
        let mut db = open(&s);
        with_table(&mut db, "t");
        insert(&mut db, "t", row(&["1", "first"]));
        insert(&mut db, "t", row(&["1", "second"]));
        insert(&mut db, "t", row(&["1", "third"]));
        drop(db);
        let mut db = open(&s);
        assert_eq!(select(&mut db, "t", &Value::str("1")), row(&["1", "third"]));
        assert_eq!(all_rows(&mut db, "t").len(), 1);
    }

    /// A KV key and a table row in one file, both intact — the property that
    /// makes sharing the log safe.
    #[test]
    fn a_kv_key_and_a_table_row_coexist() {
        let s = Scratch::new("mixed");
        let mut db = open(&s);
        db.put("theme", "\"dark\"").expect("kv put");
        with_table(&mut db, "t");
        insert(&mut db, "t", row(&["1", "a"]));
        drop(db);

        let mut db = open(&s);
        assert_eq!(db.get("theme"), Some("\"dark\""));
        assert_eq!(select(&mut db, "t", &Value::str("1")), row(&["1", "a"]));
    }

    // ---- refusals ---------------------------------------------------------

    #[test]
    fn a_composite_primary_key_is_refused() {
        // A list key has no single byte order, so it cannot be indexed. The
        // message says so rather than stringifying it.
        let e = key_json(&Value::str("x"), DB_INSERT);
        assert!(e.is_ok());
        let l = list_value(vec![Value::Int(BigNum::small(1))]);
        let err = key_json(&l, DB_INSERT).expect_err("a list is not a key");
        assert!(
            err.message().contains("primary key cannot be a list"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn a_function_primary_key_is_refused() {
        let f = Value::Builtin {
            name: "print",
            f: |_| Ok(Value::Nil),
        };
        let err = key_json(&f, DB_INSERT).expect_err("a builtin is not a key");
        assert!(err.message().contains(DB_INSERT), "got: {}", err.message());
    }

    #[test]
    fn an_empty_row_is_refused() {
        let err = as_row(&list_value(vec![]), DB_INSERT).expect_err("empty");
        assert!(
            err.message().contains("needs a primary key"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn a_table_that_does_not_exist_is_refused_by_name() {
        let s = Scratch::new("notable");
        let mut db = open(&s);
        let err = db
            .tables()
            .row("nope", "\"1\"", DB_SELECT)
            .expect_err("no table");
        assert!(
            err.message().contains("db-select: no table named 'nope'"),
            "got: {}",
            err.message()
        );
        let err = db.tables().rows("nope", DB_ALL_ROWS).expect_err("no table");
        assert!(
            err.message().contains("db-all-rows: no table named 'nope'"),
            "got: {}",
            err.message()
        );
        let err = db
            .tables()
            .remove("nope", "\"1\"", DB_DELETE_ROW)
            .expect_err("no table");
        assert!(
            err.message()
                .contains("db-delete-row: no table named 'nope'"),
            "got: {}",
            err.message()
        );
    }

    /// A refusal must leave the file untouched.
    ///
    /// The first version of `db_insert` wrote the log record and only then asked
    /// the table set whether the table existed, so this case refused the program
    /// *and* left a row behind — one that the record itself would resurrect as a
    /// table on the next open. The compiled port checked first, so the two
    /// engines disagreed about whether the program was refused at all, which is
    /// how this was found.
    #[test]
    fn a_refused_insert_writes_nothing() {
        let s = Scratch::new("no-write");
        // Through a real handle, because the ordering bug this guards sits
        // between the handle lookup, the table check and the append; a test that
        // built its own `Db` would not exercise that sequence.
        let handle = crate::db::open_for_test(&s.path());
        // No `db-create-table`, so there is no table named "nope".
        let err = db_insert(&[
            Value::Int(BigNum::small(handle)),
            Value::str("nope"),
            list_value(vec![Value::str("a"), Value::Int(BigNum::small(1))]),
        ])
        .expect_err("no such table");
        assert!(
            err.message().contains("no table named 'nope'"),
            "got: {}",
            err.message()
        );
        crate::db::with_db(handle, DB_INSERT, |db| {
            assert!(
                db.keys().is_empty(),
                "a refused db-insert must not append a record: {:?}",
                db.keys()
            );
            assert!(
                db.tables().names().is_empty(),
                "a refused db-insert must not create the table it named"
            );
            Ok(())
        })
        .expect("inspect the handle");
    }

    /// `nil` is a legal primary key, because `nil` is a legal AINL value and a
    /// key is just "the first column". It must not be confused with absence.
    #[test]
    fn nil_is_a_legal_primary_key() {
        let s = Scratch::new("nilkey");
        let mut db = open(&s);
        with_table(&mut db, "t");
        insert(&mut db, "t", list_value(vec![Value::Nil, Value::str("v")]));
        assert_eq!(
            select(&mut db, "t", &Value::Nil),
            list_value(vec![Value::Nil, Value::str("v")])
        );
        assert_eq!(all_rows(&mut db, "t").len(), 1);
    }

    /// A non-ASCII key must survive, and must sort by bytes — the rule the
    /// C port's `memcmp` also implements.
    #[test]
    fn a_non_ascii_key_survives_and_sorts_by_bytes() {
        let s = Scratch::new("utf8");
        let mut db = open(&s);
        with_table(&mut db, "t");
        for k in ["日本", "a", "é", "\u{1F600}"] {
            insert(
                &mut db,
                "t",
                list_value(vec![Value::str(k), Value::Int(BigNum::small(1))]),
            );
        }
        let got: Vec<String> = all_rows(&mut db, "t")
            .into_iter()
            .map(|r| as_row(&r, DB_ALL_ROWS).unwrap()[0].to_string())
            .collect();
        let mut want: Vec<String> = ["日本", "a", "é", "\u{1F600}"]
            .iter()
            .map(|s| String::from(*s))
            .collect();
        want.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        assert_eq!(got, want);
    }
}
