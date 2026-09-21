// Scratch probe (untracked): S17 shape phase breakdown — file-backed WAL
// load, clean drop (checkpoint+remove), then per-iteration open vs SUM
// timing, plus the in-memory SUM baseline for the same row count.
use rustqlite::{Database, Value};
use std::time::Instant;

fn main() {
    warm_probe();
    let path = "/tmp/s17ph.db";
    let delete_mode = std::env::args().any(|a| a == "--delete");
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file("/tmp/s17ph.db-wal");
    let rows = 250_000i64;
    let t0 = Instant::now();
    {
        let mut db = Database::open(path).unwrap();
        if !delete_mode {
            db.execute("PRAGMA journal_mode=WAL", []).unwrap();
        }
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
    println!("load+drop: {:.2} ms", t0.elapsed().as_secs_f64() * 1000.0);
    for it in 1..=4 {
        let t = Instant::now();
        let db = Database::open(path).unwrap();
        let open_ms = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        let out = db.query("SELECT SUM(val) FROM t", []).unwrap();
        let sum_ms = t.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(out[0][0].as_integer(), (1..=rows).sum::<i64>());
        println!(
            "iter {it}: open {open_ms:.3} ms + SUM {sum_ms:.3} ms = {total:.3} ms",
            total = open_ms + sum_ms
        );
        drop(db);
    }
    let _ = std::fs::remove_file(path);
}
// (appended) warm-cache variant: materialize all pages with a COUNT,
// then measure the SUM on a HOT cache.
#[allow(dead_code)]
fn warm_probe() {
    let path = "/tmp/s17ph-warm.db";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file("/tmp/s17ph-warm.db-wal");
    let rows = 250_000i64;
    {
        let mut db = Database::open(path).unwrap();
        db.execute("PRAGMA journal_mode=WAL", []).unwrap();
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
    let db = Database::open(path).unwrap();
    // Warm: full COUNT materializes the whole tree into the page cache.
    let t = Instant::now();
    let c = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    let warm_ms = t.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(c[0][0].as_integer(), rows);
    let mut best = f64::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        let out = db.query("SELECT SUM(val) FROM t", []).unwrap();
        assert_eq!(out[0][0].as_integer(), (1..=rows).sum::<i64>());
        best = best.min(t.elapsed().as_secs_f64() * 1000.0);
    }
    println!(
        "WARM cache: COUNT {warm_ms:.2} ms, SUM best-of-5 {best:.3} ms ({:.0} ns/row)",
        best * 1e6 / rows as f64
    );
    drop(db);
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file("/tmp/s17ph-warm.db-wal");
}
