//! DML per-op overhead probe: WHY do cached single-row UPDATE/INSERT/DELETE
//! statements cost more here than in SQLite? Measures the same op stream
//! through every engine path side by side:
//!   (a) rustqlite `db.execute`      — the engine statement-cache path
//!   (b) rustqlite prepare/step      — the sqlite3_prepare/step model
//!   (c) rusqlite `prepare_cached`   — SQLite's own statement cache
//!
//! Run: cargo run --release --example probe_dml_overhead
use rusqlite::params;
use rustqlite::{Database, Value};
use std::time::Instant;

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
        * 1000.0
}
fn build_rustqlite() -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10_000i64 {
        db.execute(
            "INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [
                Value::Text(format!("name{}", i).into()),
                Value::Integer(i),
                Value::Real(i as f64 * 1.5),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db
}
fn build_rusqlite() -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    conn.execute("BEGIN", []).unwrap();
    for i in 1..=10_000i64 {
        conn.execute(
            "INSERT INTO t (name, val, score) VALUES (?1, ?2, ?3)",
            params![format!("name{}", i), i, i as f64 * 1.5],
        )
        .unwrap();
    }
    conn.execute("COMMIT", []).unwrap();
    conn
}

fn best_of<F: FnMut() -> f64>(mut f: F, iters: usize) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..iters {
        let t = f();
        if t < best {
            best = t;
        }
    }
    best
}

fn main() {
    let n = 1000i64;
    println!(
        "probe_dml_overhead (best-of-7 passes, µs/op) @{}",
        now_ms() as u64
    );
    let _ = std::io::Write::flush(&mut std::io::stdout());

    // ---------------- UPDATE by PK ----------------
    {
        eprintln!("[phase] update: building...");
        let mut db = build_rustqlite();
        let conn = build_rusqlite();
        eprintln!("[phase] update: (a) db.execute...");
        // (a) db.execute cached
        let us_a = best_of(
            || {
                let t = Instant::now();
                for i in 1..=n {
                    db.execute(
                        "UPDATE t SET val = ? WHERE id = ?",
                        [Value::Integer(i * 2), Value::Integer(i)],
                    )
                    .unwrap();
                }
                t.elapsed().as_secs_f64() * 1e6 / n as f64
            },
            7,
        );
        eprintln!("[phase] update: (b) prepare/step...");
        // (b) prepare/step
        let us_b = best_of(
            || {
                let t = Instant::now();
                let mut stmt = db.prepare("UPDATE t SET val = ? WHERE id = ?").unwrap();
                for i in 1..=n {
                    stmt.bind(1, Value::Integer(i * 2)).unwrap();
                    stmt.bind(2, Value::Integer(i)).unwrap();
                    stmt.step().unwrap();
                    stmt.reset();
                }
                t.elapsed().as_secs_f64() * 1e6 / n as f64
            },
            7,
        );
        eprintln!("[phase] update: (c) sqlite...");
        // (c) SQLite prepared
        let us_c = best_of(
            || {
                let t = Instant::now();
                let mut stmt = conn
                    .prepare_cached("UPDATE t SET val = ?1 WHERE id = ?2")
                    .unwrap();
                for i in 1..=n {
                    stmt.execute(params![i * 2, i]).unwrap();
                }
                t.elapsed().as_secs_f64() * 1e6 / n as f64
            },
            7,
        );
        println!(
            "UPDATE by PK : rq execute {us_a:6.2}  rq step {us_b:6.2}  sqlite {us_c:6.2}   ratio(a/c) {:.2}x (b/c) {:.2}x",
            us_a / us_c, us_b / us_c
        );
    }

    // ---------------- INSERT in txn (fresh table per pass) ----------------
    {
        eprintln!("[phase] insert: setup...");
        let mut db = Database::open_in_memory().unwrap();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        db.execute(
            "CREATE TABLE iu (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
            [],
        )
        .unwrap();
        conn.execute(
            "CREATE TABLE iu (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
            [],
        )
        .unwrap();
        eprintln!("[phase] insert: (a)...");
        // (a) db.execute cached, inside a txn
        let us_a = best_of(
            || {
                db.execute("DELETE FROM iu", []).unwrap();
                let t = Instant::now();
                db.execute("BEGIN", []).unwrap();
                for i in 1..=n {
                    db.execute(
                        "INSERT INTO iu (name, val) VALUES (?, ?)",
                        [Value::Text(format!("name{}", i).into()), Value::Integer(i)],
                    )
                    .unwrap();
                }
                db.execute("COMMIT", []).unwrap();
                t.elapsed().as_secs_f64() * 1e6 / n as f64
            },
            7,
        );
        eprintln!("[phase] insert: (b)...");
        // (b) prepare/step inside txn
        let us_b = best_of(
            || {
                db.execute("DELETE FROM iu", []).unwrap();
                let t = Instant::now();
                db.execute("BEGIN", []).unwrap();
                let mut stmt = db
                    .prepare("INSERT INTO iu (name, val) VALUES (?, ?)")
                    .unwrap();
                for i in 1..=n {
                    stmt.bind(1, Value::Text(format!("name{}", i).into()))
                        .unwrap();
                    stmt.bind(2, Value::Integer(i)).unwrap();
                    stmt.step().unwrap();
                    stmt.reset();
                }
                drop(stmt);
                db.execute("COMMIT", []).unwrap();
                t.elapsed().as_secs_f64() * 1e6 / n as f64
            },
            7,
        );
        eprintln!("[phase] insert: (c)...");
        // (c) SQLite prepared inside txn
        let us_c = best_of(
            || {
                conn.execute("DELETE FROM iu", []).unwrap();
                let t = Instant::now();
                conn.execute("BEGIN", []).unwrap();
                let mut stmt = conn
                    .prepare_cached("INSERT INTO iu (name, val) VALUES (?1, ?2)")
                    .unwrap();
                for i in 1..=n {
                    stmt.execute(params![format!("name{}", i), i]).unwrap();
                }
                drop(stmt);
                conn.execute("COMMIT", []).unwrap();
                t.elapsed().as_secs_f64() * 1e6 / n as f64
            },
            7,
        );
        println!(
            "INSERT in txn: rq execute {us_a:6.2}  rq step {us_b:6.2}  sqlite {us_c:6.2}   ratio(a/c) {:.2}x (b/c) {:.2}x",
            us_a / us_c, us_b / us_c
        );
    }

    // ---------------- DELETE + INSERT cycle ----------------
    {
        let mut db = build_rustqlite();
        let conn = build_rusqlite();
        let us_a = best_of(
            || {
                let t = Instant::now();
                for i in 1..=n {
                    db.execute("DELETE FROM t WHERE id = ?", [Value::Integer(i)])
                        .unwrap();
                    db.execute(
                        "INSERT INTO t (id, name, val, score) VALUES (?, ?, ?, ?)",
                        [
                            Value::Integer(i),
                            Value::Text(format!("name{}", i).into()),
                            Value::Integer(i),
                            Value::Real(i as f64),
                        ],
                    )
                    .unwrap();
                }
                t.elapsed().as_secs_f64() * 1e6 / (n as f64 * 2.0)
            },
            7,
        );
        let us_b = best_of(
            || {
                let t = Instant::now();
                let mut del = db.prepare("DELETE FROM t WHERE id = ?").unwrap();
                let mut ins = db
                    .prepare("INSERT INTO t (id, name, val, score) VALUES (?, ?, ?, ?)")
                    .unwrap();
                for i in 1..=n {
                    del.bind(1, Value::Integer(i)).unwrap();
                    del.step().unwrap();
                    del.reset();
                    ins.bind(1, Value::Integer(i)).unwrap();
                    ins.bind(2, Value::Text(format!("name{}", i).into()))
                        .unwrap();
                    ins.bind(3, Value::Integer(i)).unwrap();
                    ins.bind(4, Value::Real(i as f64)).unwrap();
                    ins.step().unwrap();
                    ins.reset();
                }
                t.elapsed().as_secs_f64() * 1e6 / (n as f64 * 2.0)
            },
            7,
        );
        let us_c = best_of(
            || {
                let t = Instant::now();
                let mut del = conn.prepare_cached("DELETE FROM t WHERE id = ?1").unwrap();
                let mut ins = conn
                    .prepare_cached("INSERT INTO t (id, name, val, score) VALUES (?1, ?2, ?3, ?4)")
                    .unwrap();
                for i in 1..=n {
                    del.execute(params![i]).unwrap();
                    ins.execute(params![i, format!("name{}", i), i, i as f64])
                        .unwrap();
                }
                t.elapsed().as_secs_f64() * 1e6 / (n as f64 * 2.0)
            },
            7,
        );
        println!(
            "DEL+INS cycle: rq execute {us_a:6.2}  rq step {us_b:6.2}  sqlite {us_c:6.2}   ratio(a/c) {:.2}x (b/c) {:.2}x",
            us_a / us_c, us_b / us_c
        );
    }
}
