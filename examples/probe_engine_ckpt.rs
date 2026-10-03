// Verify the engine's PRAGMA wal_checkpoint output vs SQLite's shapes.
use rustqlite::{Database, Value};

fn show(rows: &[Vec<Value>]) -> String {
    match rows.first() {
        None => "[]".to_string(),
        Some(r) => format!(
            "{:?}",
            r.iter()
                .map(|v| match v {
                    Value::Integer(i) => i.to_string(),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
        ),
    }
}

fn main() {
    // Non-WAL (delete-mode) file database.
    let dir = std::env::temp_dir().join(format!("rsql-ckpt-parity-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("d.db");
    {
        let mut db = Database::open(&p).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", [])
            .unwrap();
        let rows = db.query("PRAGMA wal_checkpoint", []).unwrap();
        println!("engine non-WAL: {}", show(&rows));
    }
    // WAL with frames, sole connection.
    let p2 = dir.join("w.db");
    let mut db = Database::open(&p2).unwrap();
    db.execute("PRAGMA journal_mode=WAL", []).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    for i in 0..50 {
        db.execute(
            "INSERT INTO t (v) VALUES (?)",
            [Value::Text(format!("v{i}").into())],
        )
        .unwrap();
    }
    for sql in [
        "PRAGMA wal_checkpoint",
        "PRAGMA wal_checkpoint(PASSIVE)",
        "PRAGMA wal_checkpoint(FULL)",
        "PRAGMA wal_checkpoint(TRUNCATE)",
    ] {
        let rows = db.query(sql, []).unwrap();
        println!("engine {sql}: {}", show(&rows));
    }
    // busy shape: a second live handle holds the file.
    {
        let _h2 = Database::open(&p2).unwrap();
        let rows = db.query("PRAGMA wal_checkpoint", []).unwrap();
        println!("engine busy case (PASSIVE, live reader): {}", show(&rows));
        // FULL must be a SQLITE_BUSY-class error through execute.
        match db.execute("PRAGMA wal_checkpoint(FULL)", []) {
            Ok(_) => println!("engine FULL busy: NO ERROR (wrong)"),
            Err(e) => println!("engine FULL busy error: {e}"),
        }
    }
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
