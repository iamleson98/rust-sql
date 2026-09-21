// Scratch probe (untracked): count READ SYSCALLS (/proc/self/io syscr)
// around a fresh-open + full-table SUM on a WAL-mode file — the S17
// shape. Read-ahead engagement shows up as syscr per page << 1.
use rustqlite::{Database, Value};

fn syscr() -> u64 {
    let txt = std::fs::read_to_string("/proc/self/io").unwrap();
    for line in txt.lines() {
        if let Some(v) = line.strip_prefix("syscr: ") {
            return v.trim().parse().unwrap();
        }
    }
    0
}

fn main() {
    let path = "/tmp/s17-probe.db";
    // Idempotent: clear any leftovers from a prior aborted run.
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file("/tmp/s17-probe.db-wal");
    let rows: i64 = 250_000;
    {
        let mut db = Database::open(path).unwrap();
        db.execute("PRAGMA journal_mode=WAL", []).unwrap();
        db.execute("PRAGMA synchronous=OFF", []).unwrap();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
            [],
        )
        .unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 1..=rows {
            db.execute(
                "INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
                [
                    Value::Text(format!("name{i}").into()),
                    Value::Integer(i),
                    Value::Real(i as f64 * 1.5),
                ],
            )
            .unwrap();
        }
        db.execute("COMMIT", []).unwrap();
    }
    let size = std::fs::metadata(path).unwrap().len() / 1024 / 1024;
    println!("file: {size} MB (drop happened: sidecar checkpointed+removed)");

    // Fresh handle + first query, syscr-delta measured around it.
    let before = syscr();
    let db = Database::open(path).unwrap();
    let out = db.query("SELECT SUM(val) FROM t", []).unwrap();
    let after = syscr();
    let sum = match &out[0][0] {
        Value::Integer(v) => *v,
        v => panic!("unexpected sum {v:?}"),
    };
    let expect: i64 = (1..=rows).sum();
    assert_eq!(sum, expect, "SUM must be correct");
    println!(
        "open+SUM syscr delta: {} (file pages ~{})",
        after - before,
        size * 1024 / 1024 * 256
    );
    drop(db);
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file("/tmp/s17-probe.db-wal");
}
