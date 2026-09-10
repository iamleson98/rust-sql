//! Pin REAL SQLite's preupdate-hook event semantics against a battery of
//! DML shapes. Output = one line per event, exactly what the engine must
//! reproduce (see tests/preupdate_differential.rs).
//!
//! ```sh
//! cargo run --example preupdate_oracle
//! ```

use rusqlite::hooks::{Action, PreUpdateCase, PreUpdateOldValueAccessor};
use rusqlite::types::ValueRef;
use rusqlite::Connection;
use std::sync::Mutex;

static EVENTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn fmt_old(acc: &PreUpdateOldValueAccessor, i: i32) -> String {
    match acc.get_old_column_value(i) {
        Ok(ValueRef::Null) => "NULL".into(),
        Ok(ValueRef::Integer(v)) => format!("i:{v}"),
        Ok(ValueRef::Real(v)) => format!("f:{v}"),
        Ok(ValueRef::Text(t)) => format!("t:{}", String::from_utf8_lossy(t)),
        Ok(ValueRef::Blob(b)) => format!("b:{:?}", b),
        Err(e) => format!("ERR({e})"),
    }
}

fn fmt_new(acc: &rusqlite::hooks::PreUpdateNewValueAccessor, i: i32) -> String {
    match acc.get_new_column_value(i) {
        Ok(ValueRef::Null) => "NULL".into(),
        Ok(ValueRef::Integer(v)) => format!("i:{v}"),
        Ok(ValueRef::Real(v)) => format!("f:{v}"),
        Ok(ValueRef::Text(t)) => format!("t:{}", String::from_utf8_lossy(t)),
        Ok(ValueRef::Blob(b)) => format!("b:{:?}", b),
        Err(e) => format!("ERR({e})"),
    }
}

fn main() -> rusqlite::Result<()> {
    let db = Connection::open_in_memory()?;
    db.preupdate_hook(Some(
        |action: Action, db_name: &str, table: &str, case: &PreUpdateCase| {
            let (op, rowid, count, depth) = match case {
                PreUpdateCase::Insert(new) => (
                    "INSERT",
                    new.get_new_row_id(),
                    new.get_column_count(),
                    new.get_query_depth(),
                ),
                PreUpdateCase::Delete(old) => (
                    "DELETE",
                    old.get_old_row_id(),
                    old.get_column_count(),
                    old.get_query_depth(),
                ),
                PreUpdateCase::Update {
                    old_value_accessor, ..
                } => (
                    "UPDATE",
                    old_value_accessor.get_old_row_id(),
                    old_value_accessor.get_column_count(),
                    old_value_accessor.get_query_depth(),
                ),
                PreUpdateCase::Unknown => ("UNKNOWN", -1, -1, -1),
            };
            let opn = match action {
                Action::SQLITE_INSERT => 18,
                Action::SQLITE_DELETE => 9,
                Action::SQLITE_UPDATE => 23,
                _ => -1,
            };
            let mut line =
                format!("{op}({opn}) {db_name}.{table} rowid={rowid} count={count} depth={depth}");
            if let PreUpdateCase::Delete(old) = case {
                let vals: Vec<String> = (0..count).map(|i| fmt_old(old, i)).collect();
                line.push_str(&format!(" old=[{}]", vals.join(",")));
            }
            if let PreUpdateCase::Update {
                old_value_accessor: old,
                new_value_accessor: new,
            } = case
            {
                let vals: Vec<String> = (0..count).map(|i| fmt_old(old, i)).collect();
                line.push_str(&format!(" old=[{}]", vals.join(",")));
                let vals: Vec<String> = (0..count).map(|i| fmt_new(new, i)).collect();
                line.push_str(&format!(" new=[{}]", vals.join(",")));
            }
            if let PreUpdateCase::Insert(new) = case {
                let vals: Vec<String> = (0..count).map(|i| fmt_new(new, i)).collect();
                line.push_str(&format!(" new=[{}]", vals.join(",")));
            }
            EVENTS.lock().unwrap().push(line);
        },
    ));

    let run = |db: &Connection, sql: &str| {
        EVENTS.lock().unwrap().push(format!("-- {sql}"));
        db.execute_batch(sql).expect(sql);
    };

    // Deterministic FK enforcement for the cascade scenarios.
    db.execute_batch("PRAGMA foreign_keys=ON")?;

    // ---- 1. plain rowid table ----
    run(&db, "CREATE TABLE t (a INTEGER, b TEXT, c REAL)");
    run(&db, "INSERT INTO t VALUES (1, 'x', 1.5)");
    run(&db, "INSERT INTO t VALUES (2, NULL, NULL), (3, 'z', 3.75)");
    run(&db, "UPDATE t SET b = 'y' WHERE a = 1");
    run(&db, "UPDATE t SET a = a + 10 WHERE a >= 2");
    run(&db, "DELETE FROM t WHERE a = 12");

    // ---- 2. INTEGER PRIMARY KEY alias ----
    run(&db, "CREATE TABLE ipk (id INTEGER PRIMARY KEY, v TEXT)");
    run(&db, "INSERT INTO ipk (id, v) VALUES (7, 'seven')");
    run(&db, "INSERT INTO ipk (v) VALUES ('auto')");
    run(&db, "UPDATE ipk SET id = 99 WHERE id = 7"); // rowid CHANGES
    run(&db, "DELETE FROM ipk WHERE id = 99");

    // ---- 3. WITHOUT ROWID ----
    run(
        &db,
        "CREATE TABLE wr (k TEXT PRIMARY KEY, v INT) WITHOUT ROWID",
    );
    run(&db, "INSERT INTO wr VALUES ('a', 1)");
    run(&db, "UPDATE wr SET v = 2 WHERE k = 'a'");
    run(&db, "DELETE FROM wr WHERE k = 'a'");

    // ---- 4. upsert shapes ----
    run(&db, "CREATE TABLE u (id INTEGER PRIMARY KEY, v TEXT)");
    run(&db, "INSERT INTO u VALUES (1, 'one')");
    run(
        &db,
        "INSERT INTO u VALUES (1, 'uno') ON CONFLICT (id) DO UPDATE SET v = 'one!'",
    );
    run(&db, "INSERT INTO u VALUES (1, 'x') ON CONFLICT DO NOTHING");
    run(&db, "INSERT OR REPLACE INTO u VALUES (1, 'replaced')");

    // ---- 5. triggers (depth) ----
    run(
        &db,
        "CREATE TABLE log (msg TEXT, n INTEGER); \
         CREATE TABLE log2 (msg TEXT); \
         CREATE TRIGGER trg_ins AFTER INSERT ON u BEGIN \
           INSERT INTO log VALUES ('ins', new.id); \
         END; \
         CREATE TRIGGER trg_log AFTER INSERT ON log BEGIN \
           INSERT INTO log2 VALUES ('nested'); \
         END;",
    );
    run(&db, "INSERT INTO u VALUES (2, 'two')");

    // ---- 6. FK cascade / set null ----
    run(
        &db,
        "CREATE TABLE parent (id INTEGER PRIMARY KEY); \
         CREATE TABLE child (id INTEGER PRIMARY KEY, pid INT REFERENCES parent(id) ON DELETE CASCADE); \
         CREATE TABLE child2 (id INTEGER PRIMARY KEY, pid INT REFERENCES parent(id) ON DELETE SET NULL);",
    );
    run(&db, "INSERT INTO parent VALUES (10), (11)");
    run(&db, "INSERT INTO child VALUES (1, 10), (2, 11)");
    run(&db, "INSERT INTO child2 VALUES (1, 10)");
    run(&db, "DELETE FROM parent WHERE id = 10");

    // ---- 7. no-op statements fire nothing ----
    run(&db, "UPDATE u SET v = 'nope' WHERE id = 999");
    run(&db, "DELETE FROM u WHERE id = 999");

    // ---- 8. trailing-NULL record trim (count vs stored) ----
    run(
        &db,
        "CREATE TABLE tr (a TEXT DEFAULT 'd', b INT DEFAULT NULL, c TEXT DEFAULT NULL)",
    );
    run(&db, "INSERT INTO tr DEFAULT VALUES");
    run(&db, "UPDATE tr SET c = 'set' WHERE rowid = 1");
    run(&db, "DELETE FROM tr WHERE rowid = 1");

    // ---- 9. DDL / internal writes (does sqlite_master fire?) ----
    run(&db, "CREATE TABLE ddl1 (a INT)");
    run(&db, "CREATE INDEX ddl1_ix ON ddl1(a)");
    run(&db, "ALTER TABLE ddl1 ADD COLUMN b TEXT");
    run(&db, "ALTER TABLE ddl1 RENAME TO ddl1r");
    run(&db, "DROP TABLE ddl1r");

    // ---- 10. self-referential trigger (same-table delete) ----
    run(
        &db,
        "CREATE TABLE tree (id INTEGER PRIMARY KEY, parent INT REFERENCES tree(id) ON DELETE CASCADE); \
         INSERT INTO tree VALUES (1, NULL), (2, 1), (3, 2);",
    );
    run(&db, "DELETE FROM tree WHERE id = 1");

    // ---- 11. FK action order across multiple referencing tables ----
    run(
        &db,
        "CREATE TABLE p2 (id INTEGER PRIMARY KEY); \
         CREATE TABLE c_a (id INTEGER PRIMARY KEY, pid INT REFERENCES p2(id) ON DELETE SET NULL); \
         CREATE TABLE c_b (id INTEGER PRIMARY KEY, pid INT REFERENCES p2(id) ON DELETE CASCADE); \
         CREATE TABLE c_c (id INTEGER PRIMARY KEY, pid INT REFERENCES p2(id) ON DELETE SET NULL); \
         INSERT INTO p2 VALUES (5); \
         INSERT INTO c_a VALUES (1, 5); \
         INSERT INTO c_b VALUES (1, 5); \
         INSERT INTO c_c VALUES (1, 5);",
    );
    run(&db, "DELETE FROM p2 WHERE id = 5");

    for line in EVENTS.lock().unwrap().iter() {
        println!("{line}");
    }
    Ok(())
}
