//! The session capture side — a faithful transliteration of SQLite's
//! `sqlite3_session` object and its pre-update capture path
//! (`sessionPreupdateOneChange`, `sessionFindTable`, `sessionGrowHash`,
//! `sessionPreupdateHash`, `sessionPreupdateEqual`).
//!
//! EXACTNESS DISCIPLINE — the changeset byte layout depends on this
//! module's ordering rules, all mirrored from sqlite3session.c:
//! * tables appear in FIRST-ATTACHED order (the table list is appended,
//!   never reordered);
//! * per table, changes are stored in a hash of buckets — 256 initially
//!   (`2*128`), doubling once `nEntry >= nChange/2` — and a changeset
//!   iterates buckets `0..nChange`, chain HEAD first (newest entry);
//! * new entries are PREPENDED to their bucket; a re-hash walks old
//!   buckets in order, chains head→tail, PREPENDING each into its new
//!   bucket (which reverses within-bucket order — mirrored exactly);
//! * only the FIRST change per row key is kept: a later change to the
//!   same key just clears the `indirect` flag. The final old/new values
//!   are resolved against CURRENT database state at changeset-generation
//!   time (see `gen`), which makes the capture inherently self-healing
//!   for rolled-back statements and transactions.
//!
//! The capture consumes the engine's preupdate event stream: one TLS
//! load when no session is armed (see `preupdate::fire`).

use super::codec::*;
use crate::preupdate::PreupdateOp;
use crate::schema::Table;
use crate::types::Value;

/// The sticky per-session error (`SQLITE_SCHEMA` &c.). Once set, capture
/// stops and every API call fails with this error until cleared.
#[derive(Debug, Clone)]
pub struct SessionError(pub String);

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for SessionError {}
impl From<SessionError> for crate::error::Error {
    fn from(e: SessionError) -> Self {
        crate::error::Error::Runtime(e.0)
    }
}

/// The session table-filter callback type.
pub type TableFilterFn = Box<dyn Fn(&str) -> bool + Send>;

/// One recorded change: the OLD-record half (or, for an INSERT, the
/// PK-only record). See the module docs for the resolution model.
pub(crate) struct SessionChange {
    pub(crate) op: u8,
    pub(crate) indirect: bool,
    pub(crate) record: Vec<u8>,
}

/// One table tracked by a session.
pub(crate) struct SessionTable {
    pub(crate) name: String,
    /// Synthetic `_rowid_` first column (no declared PRIMARY KEY, with
    /// OBJCONFIG_ROWID enabled).
    pub(crate) b_rowid: bool,
    /// Column count INCLUDING the synthetic column.
    pub(crate) n_col: usize,
    /// PK array: the 1-based position within the PRIMARY KEY clause for
    /// PK columns (SQLite's `PRAGMA table_info` pk column), 0 otherwise
    /// — the EXACT bytes a changeset header carries.
    pub(crate) ab_pk: Vec<u8>,
    /// Column names in session order (`_rowid_` first when b_rowid).
    pub(crate) az_col: Vec<String>,
    /// True for `sqlite_stat1` (fake (tbl, idx) PK + idx NULL↔X'' swap).
    pub(crate) b_stat1: bool,
    /// True once the schema was resolved (lazily, at first change).
    pub(crate) initialized: bool,
    /// No explicit PRIMARY KEY and OBJCONFIG_ROWID off — the table is
    /// IGNORED by the session (SQLite's default).
    pub(crate) untrackable: bool,
    /// The change hash: bucket → entry ids, HEAD (index 0) = newest.
    pub(crate) buckets: Vec<Vec<usize>>,
    pub(crate) entries: Vec<Option<SessionChange>>,
    free_ids: Vec<usize>,
    pub(crate) n_entry: usize,
}

/// A blank, uninitialized session table model (the apply context
/// builder and the changegroup start from this shape).
pub(crate) fn blank_session_table(name: &str) -> SessionTable {
    SessionTable::new(name)
}

impl SessionTable {
    fn new(name: &str) -> Self {
        SessionTable {
            name: name.to_string(),
            b_rowid: false,
            n_col: 0,
            ab_pk: Vec::new(),
            az_col: Vec::new(),
            b_stat1: false,
            initialized: false,
            untrackable: false,
            buckets: Vec::new(),
            entries: Vec::new(),
            free_ids: Vec::new(),
            n_entry: 0,
        }
    }
}

/// The per-session state. Guarded by the caller's mutex (`Arc<Mutex<_>>`
/// in the engine registry); `record` runs inside `fire`.
pub struct SessionCore {
    pub enable: bool,
    pub indirect: bool,
    /// The attached schema name (`sqlite3session_create`'s zDb). Only
    /// "main" exists in this engine — a session bound to any other
    /// name records nothing, like SQLite against a missing schema.
    pub z_db: String,
    /// True after `attach(NULL)` or `table_filter` — unlisted tables are
    /// auto-attached (through the filter, if any). A fresh session
    /// records NOTHING until one of those (SQLite's default).
    pub auto_attach: bool,
    /// SQLITE_SESSION_OBJCONFIG_ROWID — track tables with no explicit
    /// PRIMARY KEY through a synthetic `_rowid_ INTEGER PRIMARY KEY`
    /// column. OFF by default (SQLite: such tables are simply ignored).
    /// A MISUSE to change after the first attach.
    pub implicit_pk: bool,
    /// SQLITE_SESSION_OBJCONFIG_SIZE — expose the changeset-size
    /// estimate API. OFF by default (computational overhead).
    pub enable_size: bool,
    pub(crate) tables: Vec<SessionTable>,
    pub rc: Option<SessionError>,
    /// Table-filter callback (SQLite `sqlite3session_table_filter`).
    filter: Option<TableFilterFn>,
}

// SAFETY: the filter is `Fn(&str) -> bool + Send`; the core is shared
// across threads exactly like the engine's other hook slots.
unsafe impl Send for SessionCore {}
unsafe impl Sync for SessionCore {}

impl Default for SessionCore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionCore {
    pub fn new() -> Self {
        SessionCore {
            enable: true,
            indirect: false,
            z_db: "main".to_string(),
            auto_attach: false,
            implicit_pk: false,
            enable_size: false,
            tables: Vec::new(),
            rc: None,
            filter: None,
        }
    }

    /// `sqlite3session_table_filter`.
    pub fn set_table_filter(&mut self, f: Option<TableFilterFn>) {
        self.auto_attach = true;
        self.filter = f;
    }

    /// `sqlite3session_object_config` — SQLITE_SESSION_OBJCONFIG_ROWID
    /// (2) / SIZE (1). `arg` follows SQLite's contract: >= 0 sets,
    /// < 0 queries; the variable receives the current value. Changing
    /// either after the first table attached is a misuse error.
    pub fn object_config(&mut self, op: i32, arg: &mut i32) -> bool {
        match op {
            1 => {
                if *arg >= 0 {
                    if !self.tables.is_empty() {
                        return false;
                    }
                    self.enable_size = *arg != 0;
                }
                *arg = self.enable_size as i32;
                true
            }
            2 => {
                if *arg >= 0 {
                    if !self.tables.is_empty() {
                        return false;
                    }
                    self.implicit_pk = *arg != 0;
                }
                *arg = self.implicit_pk as i32;
                true
            }
            _ => false,
        }
    }

    /// `sqlite3session_changeset_size` — a size estimate for the
    /// tracked changes (requires OBJCONFIG_SIZE).
    pub fn changeset_size(&self) -> usize {
        if !self.enable_size {
            return 0;
        }
        let mut n = 0usize;
        for t in &self.tables {
            if t.n_entry > 0 {
                n += 6 + t.n_col + t.name.len() + 1;
                for e in t.entries.iter().flatten() {
                    n += 2 + e.record.len();
                }
            }
        }
        n
    }

    /// `sqlite3session_attach(zName)`: NULL → auto-attach everything;
    /// a name → append to the table list (idempotent, case-insensitive).
    /// Order matters: the list order is the changeset's table order.
    pub fn attach(&mut self, name: Option<&str>) {
        match name {
            None => self.auto_attach = true,
            Some(n) => {
                if !self.tables.iter().any(|t| t.name.eq_ignore_ascii_case(n)) {
                    self.tables.push(SessionTable::new(n));
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tables.iter().all(|t| t.n_entry == 0)
    }

    /// Number of recorded changes (introspection / testing).
    pub fn change_count(&self) -> usize {
        self.tables.iter().map(|t| t.n_entry).sum()
    }

    fn find_table_idx(&self, name: &str) -> Option<usize> {
        self.tables
            .iter()
            .position(|t| t.name.eq_ignore_ascii_case(name))
    }

    /// `sessionFindTable` — locate the table object, auto-attaching new
    /// tables when `bAutoAttach` (through the filter). Returns the table
    /// index, or `None` when the table is not tracked.
    fn find_or_auto_attach(&mut self, name: &str) -> Option<usize> {
        if let Some(i) = self.find_table_idx(name) {
            return Some(i);
        }
        if !self.auto_attach {
            return None;
        }
        if let Some(f) = &self.filter {
            if !f(name) {
                return None;
            }
        }
        self.tables.push(SessionTable::new(name));
        Some(self.tables.len() - 1)
    }

    /// Resolve a table's session model from the ENGINE's live `Table`
    /// (`sessionInitTable` via `sessionTableInfo`). For `sqlite_stat1`
    /// the fake (tbl, idx) PK view applies. Tables with no explicit
    /// PRIMARY KEY are UNTRACKABLE unless `implicit_pk` is set (SQLite's
    /// OBJCONFIG_ROWID semantics).
    pub(crate) fn init_table_model(t: &mut SessionTable, tbl: &Table, implicit_pk: bool) {
        let b_stat1 = t.name.eq_ignore_ascii_case("sqlite_stat1");
        let has_decl_pk = tbl.columns.iter().any(|c| c.primary_key);
        // The synthetic _rowid_ column only exists with OBJCONFIG_ROWID.
        let b_rowid = implicit_pk && !tbl.without_rowid && !has_decl_pk && !b_stat1;
        let untrackable = !b_rowid && !has_decl_pk && !b_stat1;
        let mut ab_pk = Vec::with_capacity(tbl.columns.len() + 1);
        let mut az_col = Vec::with_capacity(tbl.columns.len() + 1);
        if b_rowid {
            ab_pk.push(1u8);
            az_col.push(SESSIONS_ROWID.to_string());
        }
        for c in &tbl.columns {
            // PRAGMA table_info's pk: the 1-based position within the
            // PRIMARY KEY clause, 0 for non-members — the exact bytes
            // changeset headers carry.
            ab_pk.push(c.pk_seq);
            az_col.push(c.name.clone());
        }
        if b_stat1 {
            // (tbl, idx) pretend-PK — three columns, no synthetic rowid.
            ab_pk.truncate(3);
            while ab_pk.len() < 3 {
                ab_pk.push(0);
            }
            ab_pk[0] = 1;
            ab_pk[1] = 2;
            ab_pk[2] = 0;
        }
        t.b_rowid = b_rowid;
        t.n_col = az_col.len();
        t.az_col = az_col;
        t.ab_pk = ab_pk;
        t.b_stat1 = b_stat1;
        t.untrackable = untrackable;
        t.initialized = true;
    }

    /// The stat1 view of an event value: column 1 NULL → zero-length
    /// blob (the X''/NULL swap SQLite's sessionStat1Old/New apply).
    fn stat1_view(b_stat1: bool, col: usize, v: &Value) -> Value {
        if b_stat1 && col == 1 && matches!(v, Value::Null) {
            Value::Blob(Vec::new())
        } else {
            v.clone()
        }
    }

    /// Record one preupdate event — `xPreUpdate`'s discipline: an
    /// UPDATE records (1) UPDATE at the OLD key carrying the old values,
    /// then (2) INSERT at the NEW key carrying the new PK values.
    pub(crate) fn record(
        &mut self,
        op: PreupdateOp,
        tbl: &Table,
        rowid: i64,
        old: Option<&[Value]>,
        new: Option<&[Value]>,
        depth: u32,
    ) {
        if !self.enable || self.rc.is_some() {
            return;
        }
        if !self.z_db.eq_ignore_ascii_case("main") {
            return; // a session on another schema never sees main's rows
        }
        let idx = match self.find_or_auto_attach(&tbl.name) {
            Some(i) => i,
            None => return,
        };

        // Lazy schema init, then column-count drift handling (ALTER
        // TABLE ... ADD COLUMN on a tracked table): re-resolve the model
        // and pad stored records with the new columns' DEFAULT values.
        if !self.tables[idx].initialized {
            let implicit = self.implicit_pk;
            let mut model = std::mem::replace(&mut self.tables[idx], SessionTable::new(""));
            Self::init_table_model(&mut model, tbl, implicit);
            self.tables[idx] = model;
        } else if self.tables[idx].untrackable {
            // No explicit PK and OBJCONFIG_ROWID off — ignored.
            return;
        } else {
            let expected = self.tables[idx].n_col - if self.tables[idx].b_rowid { 1 } else { 0 };
            let got = match op {
                PreupdateOp::Insert => new.map(|v| v.len()).unwrap_or(0),
                _ => old.map(|v| v.len()).unwrap_or(0),
            };
            if expected != got {
                let implicit = self.implicit_pk;
                let mut model = std::mem::replace(&mut self.tables[idx], SessionTable::new(""));
                let res = Self::reinit_and_pad(&mut model, tbl, implicit);
                self.tables[idx] = model;
                if let Err(e) = res {
                    self.rc = Some(SessionError(e));
                    return;
                }
            }
        }
        if self.tables[idx].untrackable {
            return;
        }

        match op {
            PreupdateOp::Update => {
                self.record_one(OP_UPDATE, tbl, rowid, old, new, depth, false);
                self.record_one(OP_INSERT, tbl, rowid, old, new, depth, true);
            }
            PreupdateOp::Insert => {
                self.record_one(OP_INSERT, tbl, rowid, old, new, depth, false);
            }
            PreupdateOp::Delete => {
                self.record_one(OP_DELETE, tbl, rowid, old, new, depth, false);
            }
        }
    }

    /// One capture step at ONE key (`sessionPreupdateOneChange`).
    /// `b_new_key` selects which side's PK keys the hash (old-key for
    /// the UPDATE entry of an UPDATE's double fire, new-key for its
    /// INSERT entry).
    fn record_one(
        &mut self,
        sop: u8,
        tbl: &Table,
        rowid: i64,
        old: Option<&[Value]>,
        new: Option<&[Value]>,
        depth: u32,
        b_new_key: bool,
    ) {
        let ti = match self.find_table_idx(&tbl.name) {
            Some(i) => i,
            None => return,
        };
        let t = &mut self.tables[ti];
        let b_rowid = t.b_rowid;
        let b_stat1 = t.b_stat1;
        let n_db_col = t.n_col - if b_rowid { 1 } else { 0 };

        let old_v: Option<Vec<Value>> = if sop != OP_INSERT {
            old.map(|v| {
                v.iter()
                    .enumerate()
                    .map(|(i, x)| Self::stat1_view(b_stat1, i, x))
                    .collect()
            })
        } else {
            None
        };
        let new_v: Option<Vec<Value>> = if sop != OP_DELETE {
            new.map(|v| {
                v.iter()
                    .enumerate()
                    .map(|(i, x)| Self::stat1_view(b_stat1, i, x))
                    .collect()
            })
        } else {
            None
        };

        // The change's key: the rowid for rowid tables, else the PK
        // columns of the old side (UPDATE/DELETE) or the new side
        // (INSERT / an UPDATE's new-key entry). A NULL PK value makes
        // the change invisible to the session module.
        let mut h: u32 = 0;
        if b_rowid {
            h = hash_i64(h, rowid);
        } else {
            let side = if b_new_key || sop == OP_INSERT {
                new_v.as_deref()
            } else {
                old_v.as_deref()
            };
            let side = match side {
                Some(s) if s.len() == n_db_col => s,
                _ => return,
            };
            for (i, v) in side.iter().enumerate() {
                if t.ab_pk
                    .get(i + if b_rowid { 1 } else { 0 })
                    .copied()
                    .unwrap_or(0)
                    == 0
                {
                    continue;
                }
                match v {
                    Value::Integer(x) => {
                        h = hash_type(h, TYPE_INT);
                        h = hash_i64(h, *x);
                    }
                    Value::Real(x) => {
                        h = hash_type(h, TYPE_REAL);
                        h = hash_i64(h, x.to_bits() as i64);
                    }
                    Value::Text(x) => {
                        h = hash_type(h, TYPE_TEXT);
                        h = hash_blob(h, x.as_str().as_bytes());
                    }
                    Value::Blob(x) => {
                        h = hash_type(h, TYPE_BLOB);
                        h = hash_blob(h, x);
                    }
                    Value::Null => return, // NULL PK: change ignored
                }
            }
        }

        // Grow the hash (256 first, double at load >= 1/2) — mirroring
        // sessionGrowHash's re-hash order exactly (old buckets in order,
        // chains head→tail, each PREPENDED to its new bucket).
        if t.buckets.is_empty() || t.n_entry >= t.buckets.len() / 2 {
            let n_new = if t.buckets.is_empty() {
                256
            } else {
                t.buckets.len() * 2
            };
            let mut new_buckets: Vec<Vec<usize>> = vec![Vec::new(); n_new];
            for chain in t.buckets.iter_mut() {
                for &id in chain.iter() {
                    let e = t.entries[id].as_ref().unwrap();
                    let ih = record_pk_hash(&t.ab_pk, &e.record, false, n_new as u32);
                    new_buckets[ih].insert(0, id);
                }
            }
            t.buckets = new_buckets;
        }

        let bucket = (h % t.buckets.len() as u32) as usize;

        // Existing entry for this key? (`sessionPreupdateEqual`.)
        let existing: Option<usize> = if b_rowid {
            t.buckets[bucket].iter().copied().find(|&id| {
                t.entries[id]
                    .as_ref()
                    .map(|e| e.record.first() == Some(&TYPE_INT) && get_i64(&e.record, 1) == rowid)
                    .unwrap_or(false)
            })
        } else {
            t.buckets[bucket].iter().copied().find(|&id| {
                t.entries[id]
                    .as_ref()
                    .map(|e| {
                        record_matches_pk(
                            t,
                            &e.record,
                            old_v.as_deref(),
                            new_v.as_deref(),
                            sop,
                            b_new_key,
                        )
                    })
                    .unwrap_or(false)
            })
        };

        match existing {
            Some(id) => {
                // Keep the FIRST change (generation resolves the rest).
                // A direct change clears an existing indirect marker.
                if depth == 0 && !self.indirect {
                    if let Some(e) = t.entries[id].as_mut() {
                        e.indirect = false;
                    }
                }
            }
            None => {
                // Serialize the record: for UPDATE/DELETE the FULL old
                // row; for INSERT the new PK values + undefined fillers.
                let mut record = Vec::with_capacity(16 + 9 * t.n_col);
                if b_rowid {
                    record.push(TYPE_INT);
                    put_i64(&mut record, rowid);
                }
                for i in 0..n_db_col {
                    let is_pk = t.ab_pk[i + if b_rowid { 1 } else { 0 }] != 0;
                    let v: Option<&Value> = match sop {
                        OP_INSERT => {
                            if is_pk {
                                new_v.as_deref().map(|s| &s[i])
                            } else {
                                None
                            }
                        }
                        _ => old_v.as_deref().map(|s| &s[i]),
                    };
                    append_value(&mut record, v);
                }
                let entry = SessionChange {
                    op: sop,
                    indirect: self.indirect || depth > 0,
                    record,
                };
                let id = if let Some(free) = t.free_ids.pop() {
                    t.entries[free] = Some(entry);
                    free
                } else {
                    t.entries.push(Some(entry));
                    t.entries.len() - 1
                };
                t.buckets[bucket].insert(0, id);
                t.n_entry += 1;
            }
        }
    }

    /// `sessionReinitTable` + `sessionUpdateChanges`: re-resolve the
    /// model after schema drift; pad stored records with the appended
    /// columns' DEFAULT values. Anything that changes the PK shape or
    /// drops columns is SQLITE_SCHEMA (sticky session error).
    pub(crate) fn reinit_and_pad(
        t: &mut SessionTable,
        tbl: &Table,
        implicit_pk: bool,
    ) -> Result<(), String> {
        let old_pk = t.ab_pk.clone();
        let old_n = t.n_col;
        let old_rowid = t.b_rowid;
        let old_cols = t.az_col.clone();
        Self::init_table_model(t, tbl, implicit_pk);
        if t.n_col < old_n || t.b_rowid != old_rowid {
            return Err("database schema has changed".to_string());
        }
        for (i, &pk) in t.ab_pk.iter().enumerate() {
            let was_pk = i < old_pk.len() && old_pk[i] != 0;
            let is_pk = pk != 0;
            if i < old_n && is_pk != was_pk {
                return Err("database schema has changed".to_string());
            }
            if i >= old_n && is_pk {
                return Err("database schema has changed".to_string());
            }
        }
        if t.n_col > old_n {
            // The appended columns (session order == declared order
            // after the pre-existing prefix).
            let db_old_n = old_n - if old_rowid { 1 } else { 0 };
            let new_defaults: Vec<Option<Value>> = tbl
                .columns
                .iter()
                .skip(db_old_n)
                .map(default_value_of)
                .collect();
            for slot in t.entries.iter_mut() {
                if let Some(e) = slot.as_mut() {
                    // Pad by appending serialized defaults — the number
                    // of appended fields equals t.n_col - old_n.
                    for dv in new_defaults.iter().take(t.n_col - old_n) {
                        append_value(&mut e.record, dv.as_ref());
                    }
                }
            }
        }
        // az_col names may legitimately change via ALTER RENAME COLUMN;
        // session keying is positional. Keep the fresh names.
        let _ = old_cols;
        Ok(())
    }
}

/// A column's DEFAULT value as the engine applies it on INSERT
/// (SQLite's `sessionPrepareDfltStmt` — `SELECT <defaults...>`).
fn default_value_of(c: &crate::schema::Column) -> Option<Value> {
    use crate::executor::expr::{evaluate, EvalContext};
    use std::collections::HashMap;
    match &c.default {
        Some(expr) => {
            static NO_PARAMS: &[Value] = &[];
            static NO_NAMED: std::sync::OnceLock<HashMap<String, Value>> =
                std::sync::OnceLock::new();
            let named = NO_NAMED.get_or_init(HashMap::new);
            let ctx = EvalContext::new(&[], &[], NO_PARAMS, named);
            match evaluate(expr, &ctx) {
                Ok(v) => Some(v),
                Err(_) => Some(Value::Null),
            }
        }
        None => Some(Value::Null),
    }
}

/// The record-based PK hash (`sessionChangeHash`) — used by the capture
/// re-hash, the changegroup and the rebaser. `b_pk_only`: the record
/// contains ONLY the PK fields (patchset DELETE records).
pub(crate) fn record_pk_hash(ab_pk: &[u8], rec: &[u8], b_pk_only: bool, n_bucket: u32) -> usize {
    let mut h: u32 = 0;
    let mut pos = 0usize;
    for &is_pk in ab_pk.iter() {
        let is_pk = is_pk != 0;
        if b_pk_only && !is_pk {
            continue; // field not present in a pk-only record
        }
        if is_pk {
            let e = rec[pos];
            h = hash_type(h, e);
            pos += 1;
            match e {
                TYPE_INT | TYPE_REAL => {
                    h = hash_i64(h, get_i64(rec, pos));
                    pos += 8;
                }
                _ => {
                    let (n, hdr) = get_varint(rec, pos);
                    h = hash_blob(h, &rec[pos + hdr..pos + hdr + n as usize]);
                    pos += hdr + n as usize;
                }
            }
        } else {
            pos += serial_len(rec, pos);
        }
    }
    (h % n_bucket) as usize
}

/// `sessionPreupdateEqual`, record form: do the PK fields of the stored
/// record equal this event's key-side PK values?
fn record_matches_pk(
    t: &SessionTable,
    rec: &[u8],
    old_v: Option<&[Value]>,
    new_v: Option<&[Value]>,
    sop: u8,
    b_new_key: bool,
) -> bool {
    let side = if b_new_key || sop == OP_INSERT {
        new_v
    } else {
        old_v
    };
    let side = match side {
        Some(s) if s.len() + if t.b_rowid { 1 } else { 0 } == t.n_col => s,
        _ => return false,
    };
    let mut pos = 0usize;
    for (i, &flag) in t.ab_pk.iter().enumerate() {
        if flag != 0 {
            let mut key_field = Vec::with_capacity(12);
            append_value(
                &mut key_field,
                Some(&side[i - if t.b_rowid { 1 } else { 0 }]),
            );
            let l = serial_len(rec, pos);
            if rec[pos..pos + l] != key_field[..] {
                return false;
            }
            pos += l;
        } else {
            pos += serial_len(rec, pos);
        }
    }
    true
}
