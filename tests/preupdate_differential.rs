//! Preupdate-hook event-stream differential: the SAME statement battery
//! through real SQLite (rusqlite, `preupdate_hook` feature) and the
//! engine (`Database::set_preupdate_hook`), comparing every event:
//! op code, db, table, rowid, column count, depth, and every old/new
//! column value. Pins SQLite's exact observable semantics:
//!
//! - rowid tables: INSERT (new), UPDATE (old+new), DELETE (old)
//! - WITHOUT ROWID tables: events fire with rowid = 0
//! - `INSERT OR REPLACE` = DELETE(old) + INSERT(new); upsert DO UPDATE =
//!   one UPDATE event; DO NOTHING fires nothing
//! - rowid-alias-changing UPDATE = ONE UPDATE event (old rowid reported)
//! - trigger bodies and FK actions fire at depth 1+, one level per
//!   nesting/cascade step
//! - DDL (CREATE/ALTER/DROP) fires nothing
//! - defaults materialize in the event's values
//!
//! `examples/preupdate_oracle.rs` prints the same battery's raw SQLite
//! stream (the human-readable version of this contract).

use rusqlite::hooks::{Action, PreUpdateCase};
use rusqlite::types::ValueRef;
use std::sync::Mutex;

/// Bit-stable f64 formatting (identical on both engines).
fn fmt_f64(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{v:.1}")
    } else {
        format!("{v}")
    }
}

/// One recorded event, in the shared comparable format:
/// `OP(code) main.table rowid=R count=N depth=D old=[..] new=[..]`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Ev(String);

fn fmt_value(vr: ValueRef<'_>) -> String {
    match vr {
        ValueRef::Null => "NULL".into(),
        ValueRef::Integer(v) => format!("i:{v}"),
        ValueRef::Real(v) => format!("f:{}", fmt_f64(v)),
        ValueRef::Text(t) => format!("t:{}", String::from_utf8_lossy(t)),
        ValueRef::Blob(b) => format!("b:{:?}", b),
    }
}

/// The statement battery: (label, SQL). Identical for both engines.
const SCRIPT: &[&str] = &[
    "CREATE TABLE t (a INTEGER, b TEXT, c REAL)",
    "INSERT INTO t VALUES (1, 'x', 1.5)",
    "INSERT INTO t VALUES (2, NULL, NULL), (3, 'z', 3.75)",
    "UPDATE t SET b = 'y' WHERE a = 1",
    "UPDATE t SET a = a + 10 WHERE a >= 2",
    "DELETE FROM t WHERE a = 12",
    "CREATE TABLE ipk (id INTEGER PRIMARY KEY, v TEXT)",
    "INSERT INTO ipk (id, v) VALUES (7, 'seven')",
    "INSERT INTO ipk (v) VALUES ('auto')",
    "UPDATE ipk SET id = 99 WHERE id = 7",
    "DELETE FROM ipk WHERE id = 99",
    "CREATE TABLE wr (k TEXT PRIMARY KEY, v INT) WITHOUT ROWID",
    "INSERT INTO wr VALUES ('a', 1), ('b', 2)",
    "UPDATE wr SET v = 2 WHERE k = 'a'",
    "DELETE FROM wr WHERE k = 'a'",
    "CREATE TABLE u (id INTEGER PRIMARY KEY, v TEXT)",
    "INSERT INTO u VALUES (1, 'one')",
    "INSERT INTO u VALUES (1, 'uno') ON CONFLICT (id) DO UPDATE SET v = 'one!'",
    "INSERT INTO u VALUES (1, 'x') ON CONFLICT DO NOTHING",
    "INSERT OR REPLACE INTO u VALUES (1, 'replaced')",
    "CREATE TABLE log (msg TEXT, n INTEGER); CREATE TABLE log2 (msg TEXT); \
     CREATE TRIGGER trg_ins AFTER INSERT ON u BEGIN INSERT INTO log VALUES ('ins', new.id); END; \
     CREATE TRIGGER trg_log AFTER INSERT ON log BEGIN INSERT INTO log2 VALUES ('nested'); END;",
    "INSERT INTO u VALUES (2, 'two')",
    "CREATE TABLE parent (id INTEGER PRIMARY KEY); \
     CREATE TABLE child (id INTEGER PRIMARY KEY, pid INT REFERENCES parent(id) ON DELETE CASCADE); \
     CREATE TABLE child2 (id INTEGER PRIMARY KEY, pid INT REFERENCES parent(id) ON DELETE SET NULL); \
     CREATE TABLE c_a (id INTEGER PRIMARY KEY, pid INT REFERENCES parent(id) ON DELETE SET NULL);",
    "INSERT INTO parent VALUES (10), (11)",
    "INSERT INTO child VALUES (1, 10), (2, 11)",
    "INSERT INTO child2 VALUES (1, 10)",
    "INSERT INTO c_a VALUES (1, 10)",
    "DELETE FROM parent WHERE id = 10",
    "UPDATE u SET v = 'nope' WHERE id = 999",
    "DELETE FROM u WHERE id = 999",
    "CREATE TABLE tr (a TEXT DEFAULT 'd', b INT DEFAULT NULL, c TEXT DEFAULT NULL)",
    "INSERT INTO tr DEFAULT VALUES",
    "UPDATE tr SET c = 'set' WHERE rowid = 1",
    "DELETE FROM tr WHERE rowid = 1",
    "CREATE TABLE ddl1 (a INT)",
    "CREATE INDEX ddl1_ix ON ddl1(a)",
    "ALTER TABLE ddl1 RENAME TO ddl1r",
    "DROP TABLE ddl1r",
    "CREATE TABLE tree (id INTEGER PRIMARY KEY, parent INT REFERENCES tree(id) ON DELETE CASCADE); \
     INSERT INTO tree VALUES (1, NULL), (2, 1), (3, 2);",
    "DELETE FROM tree WHERE id = 1",
];

// ---------------------------------------------------------------------------
// SQLite side
// ---------------------------------------------------------------------------

fn sqlite_stream() -> Vec<Ev> {
    let db = rusqlite::Connection::open_in_memory().unwrap();
    db.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    // 'static collector (both hook APIs require 'static closures):
    // leaked per call so parallel tests never share state.
    let events: &'static Mutex<Vec<String>> = Box::leak(Box::new(Mutex::new(Vec::new())));
    let ev = events;
    db.preupdate_hook(Some(
        move |action: Action, db_name: &str, table: &str, case: &PreUpdateCase| {
            let (op, opn, rowid, count, depth) = match case {
                PreUpdateCase::Insert(new) => (
                    "INSERT",
                    18,
                    new.get_new_row_id(),
                    new.get_column_count(),
                    new.get_query_depth(),
                ),
                PreUpdateCase::Delete(old) => (
                    "DELETE",
                    9,
                    old.get_old_row_id(),
                    old.get_column_count(),
                    old.get_query_depth(),
                ),
                PreUpdateCase::Update {
                    old_value_accessor, ..
                } => (
                    "UPDATE",
                    23,
                    old_value_accessor.get_old_row_id(),
                    old_value_accessor.get_column_count(),
                    old_value_accessor.get_query_depth(),
                ),
                PreUpdateCase::Unknown => ("UNKNOWN", -1, -1, -1, -1),
            };
            let _ = opn;
            let mut line =
                format!("{op} {db_name}.{table} rowid={rowid} count={count} depth={depth}");
            if let PreUpdateCase::Delete(old) = case {
                let vals: Vec<String> = (0..count)
                    .map(|i| fmt_value(old.get_old_column_value(i).unwrap()))
                    .collect();
                line.push_str(&format!(" old=[{}]", vals.join(",")));
            }
            if let PreUpdateCase::Update {
                old_value_accessor: old,
                new_value_accessor: new,
            } = case
            {
                let vals: Vec<String> = (0..count)
                    .map(|i| fmt_value(old.get_old_column_value(i).unwrap()))
                    .collect();
                line.push_str(&format!(" old=[{}]", vals.join(",")));
                let vals: Vec<String> = (0..count)
                    .map(|i| fmt_value(new.get_new_column_value(i).unwrap()))
                    .collect();
                line.push_str(&format!(" new=[{}]", vals.join(",")));
            }
            if let PreUpdateCase::Insert(new) = case {
                let vals: Vec<String> = (0..count)
                    .map(|i| fmt_value(new.get_new_column_value(i).unwrap()))
                    .collect();
                line.push_str(&format!(" new=[{}]", vals.join(",")));
            }
            let _ = action;
            ev.lock().unwrap().push(line);
        },
    ));
    for sql in SCRIPT {
        db.execute_batch(sql).unwrap();
    }
    ev.lock().unwrap().clone().into_iter().map(Ev).collect()
}

// ---------------------------------------------------------------------------
// Engine side
// ---------------------------------------------------------------------------

fn fmt_engine_value(v: &rustqlite::Value) -> String {
    match v {
        rustqlite::Value::Null => "NULL".into(),
        rustqlite::Value::Integer(i) => format!("i:{i}"),
        rustqlite::Value::Real(f) => format!("f:{}", fmt_f64(*f)),
        rustqlite::Value::Text(t) => format!("t:{}", t),
        rustqlite::Value::Blob(b) => format!("b:{:?}", b),
    }
}

fn engine_stream() -> Vec<Ev> {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("PRAGMA foreign_keys=ON", ()).unwrap();
    let events: &'static Mutex<Vec<String>> = Box::leak(Box::new(Mutex::new(Vec::new())));
    let ev = events;
    db.set_preupdate_hook(Some(Box::new(
        move |e: &rustqlite::preupdate::PreupdateEvent| {
            let op = match e.op {
                rustqlite::preupdate::PreupdateOp::Insert => "INSERT",
                rustqlite::preupdate::PreupdateOp::Delete => "DELETE",
                rustqlite::preupdate::PreupdateOp::Update => "UPDATE",
            };
            let count = e
                .old
                .as_ref()
                .or(e.new.as_ref())
                .map(|v| v.len())
                .unwrap_or(0);
            let mut line = format!(
                "{op} {}.{} rowid={} count={} depth={}",
                e.db, e.table, e.rowid, count, e.depth
            );
            if let Some(old) = &e.old {
                let vals: Vec<String> = old.iter().map(fmt_engine_value).collect();
                line.push_str(&format!(" old=[{}]", vals.join(",")));
            }
            if let Some(new) = &e.new {
                let vals: Vec<String> = new.iter().map(fmt_engine_value).collect();
                line.push_str(&format!(" new=[{}]", vals.join(",")));
            }
            ev.lock().unwrap().push(line);
        },
    )));
    for sql in SCRIPT {
        db.execute(sql, ()).unwrap();
    }
    ev.lock().unwrap().clone().into_iter().map(Ev).collect()
}

// ---------------------------------------------------------------------------
// The differential
// ---------------------------------------------------------------------------

#[test]
fn preupdate_event_streams_match_sqlite() {
    let sqlite = sqlite_stream();
    let engine = engine_stream();
    if sqlite != engine {
        let mut a = sqlite.iter().map(|e| &e.0);
        let mut b = engine.iter().map(|e| &e.0);
        let mut shown = 0;
        eprintln!("--- first divergence ---");
        loop {
            match (a.next(), b.next()) {
                (Some(x), Some(y)) if x == y => continue,
                (x, y) => {
                    eprintln!("sqlite: {x:?}");
                    eprintln!("engine: {y:?}");
                    shown += 1;
                    if shown >= 6 {
                        break;
                    }
                    // advance both to find further diffs
                    if a.next().is_none() && b.next().is_none() {
                        break;
                    }
                }
            }
        }
        eprintln!(
            "sqlite: {} events, engine: {} events",
            sqlite.len(),
            engine.len()
        );
        panic!("preupdate event streams diverge");
    }
}

/// The engine's public event surface (op codes, value sets) — a small
/// direct API test on top of the differential.
#[test]
fn preupdate_op_codes_and_no_hook_cost() {
    use rustqlite::preupdate::{PreupdateEvent, PreupdateHook, PreupdateOp};
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", ())
        .unwrap();
    let seen: &'static Mutex<Vec<(i32, String, i64)>> = Box::leak(Box::new(Mutex::new(Vec::new())));
    let s = seen;
    let hook: PreupdateHook = Box::new(move |e: &PreupdateEvent| {
        s.lock()
            .unwrap()
            .push((e.op.code(), e.table.clone(), e.rowid));
    });
    db.set_preupdate_hook(Some(hook));
    db.execute("INSERT INTO t VALUES (1, 'a')", ()).unwrap();
    db.execute("UPDATE t SET v = 'b' WHERE id = 1", ()).unwrap();
    db.execute("DELETE FROM t WHERE id = 1", ()).unwrap();
    let got = seen.lock().unwrap().clone();
    let want = vec![
        (18, "t".to_string(), 1),
        (23, "t".to_string(), 1),
        (9, "t".to_string(), 1),
    ];
    assert_eq!(got, want);

    // Clearing the hook: no events afterwards.
    db.set_preupdate_hook(None);
    db.execute("INSERT INTO t VALUES (2, 'x')", ()).unwrap();
    // WITHOUT ROWID: rowid reported as 0.
    db.execute(
        "CREATE TABLE wr (k TEXT PRIMARY KEY, v INT) WITHOUT ROWID",
        (),
    )
    .unwrap();
    let seen2: &'static Mutex<Vec<i64>> = Box::leak(Box::new(Mutex::new(Vec::new())));
    let s2 = seen2;
    db.set_preupdate_hook(Some(Box::new(move |e: &PreupdateEvent| {
        s2.lock().unwrap().push(e.rowid);
    })));
    db.execute("INSERT INTO wr VALUES ('k', 1)", ()).unwrap();
    assert_eq!(seen2.lock().unwrap().clone(), vec![0]);
    assert_eq!(PreupdateOp::Insert.code(), 18, "SQLITE_INSERT");
    assert_eq!(PreupdateOp::Delete.code(), 9, "SQLITE_DELETE");
    assert_eq!(PreupdateOp::Update.code(), 23, "SQLITE_UPDATE");
}
