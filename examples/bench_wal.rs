//! File-backed commit-throughput benchmark: journal_mode=delete vs WAL
//! vs WAL+NORMAL vs SQLite (delete & WAL). Demonstrates WAL-served reads
//! (readers resolve pages through the committed-frame map) and the commit
//! cost of each mode.
use rusqlite::Connection;
use std::time::Instant;

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs() as f64 * 1e3 + d.as_nanos() as f64 / 1e6
}

fn bench_rustqlite(mode: &str, sync: Option<&str>, n_txns: usize, rows_per_txn: usize) -> f64 {
    let path = format!("/tmp/bench_wal_rql_{}.db", mode.replace(['=', ' '], "_"));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path));
    let mut db = rustqlite::Database::open(&path).unwrap();
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    if let Some(s) = sync {
        db.execute(&format!("PRAGMA synchronous = {}", s), []).unwrap();
    }
    if mode != "wal" {
        db.execute("PRAGMA journal_mode = DELETE", []).unwrap();
    }
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER, s TEXT)", []).unwrap();

    let start = Instant::now();
    for t in 0..n_txns {
        db.execute("BEGIN", []).unwrap();
        for i in 0..rows_per_txn {
            let id = t * rows_per_txn + i + 1;
            db.execute(
                "INSERT INTO t (v, s) VALUES (?, ?)",
                [rustqlite::Value::Integer(id as i64),
                 rustqlite::Value::Text(format!("row{id}").into())],
            )
            .unwrap();
        }
        db.execute("COMMIT", []).unwrap();
    }
    let elapsed = ms(start.elapsed());

    // Reads after commits WITHOUT a checkpoint: pages resolve through the
    // WAL frame map (WAL-served reads) or the main file (delete mode).
    let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), (n_txns * rows_per_txn) as i64);
    let q_start = Instant::now();
    for k in 1..=200 {
        let _ = db.query("SELECT s FROM t WHERE id = ?", [rustqlite::Value::Integer(k)]).unwrap();
    }
    let q = ms(q_start.elapsed());
    drop(db);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path));
    println!(
        "{:<28} {:>9.2} ms commits ({:.1} µs/txn)   + {:>7.2} ms reads",
        mode,
        elapsed,
        elapsed * 1000.0 / n_txns as f64,
        q
    );
    elapsed
}

fn bench_sqlite(mode: &str, sync: &str, n_txns: usize, rows_per_txn: usize) -> f64 {
    let path = format!("/tmp/bench_wal_sql_{}.db", mode.replace(['=', ' '], "_"));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path));
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "PRAGMA journal_mode = {mode}; PRAGMA synchronous = {sync};"
    ))
    .unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER, s TEXT)", []).unwrap();

    let start = Instant::now();
    for t in 0..n_txns {
        conn.execute_batch("BEGIN").unwrap();
        for i in 0..rows_per_txn {
            let id = t * rows_per_txn + i + 1;
            conn.execute(
                "INSERT INTO t (v, s) VALUES (?1, ?2)",
                rusqlite::params![id as i64, format!("row{id}")],
            )
            .unwrap();
        }
        conn.execute_batch("COMMIT").unwrap();
    }
    let elapsed = ms(start.elapsed());

    let q_start = Instant::now();
    let mut stmt = conn.prepare("SELECT s FROM t WHERE id = ?1").unwrap();
    for k in 1..=200i64 {
        let _ = stmt.query_row(rusqlite::params![k], |_| Ok(())).ok();
    }
    let q = ms(q_start.elapsed());
    drop(stmt);
    drop(conn);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path));
    println!(
        "{:<28} {:>9.2} ms commits ({:.1} µs/txn)   + {:>7.2} ms reads",
        format!("sqlite {mode}/{sync}"),
        elapsed,
        elapsed * 1000.0 / n_txns as f64,
        q
    );
    elapsed
}

fn main() {
    let n_txns = 200;
    let rows = 20;

    println!("== rustqlite file-backed commit throughput ({} txns x {} rows) ==", n_txns, rows);
    let del = bench_rustqlite("delete", None, n_txns, rows);
    let wal_full = bench_rustqlite("wal sync=FULL", Some("FULL"), n_txns, rows);
    let wal_normal = bench_rustqlite("wal sync=NORMAL", Some("NORMAL"), n_txns, rows);

    println!();
    println!("== sqlite file-backed commit throughput ==");
    let s_del = bench_sqlite("DELETE", "FULL", n_txns, rows);
    let s_wal_full = bench_sqlite("WAL", "FULL", n_txns, rows);
    let s_wal_normal = bench_sqlite("WAL", "NORMAL", n_txns, rows);

    println!();
    println!("== ratios (lower is faster) ==");
    println!("rustqlite delete/sqlite delete:      {:.2}x", del / s_del);
    println!("rustqlite wal-full/sqlite wal-full:  {:.2}x", wal_full / s_wal_full);
    println!("rustqlite wal-normal/sqlite wal-norm:{:.2}x", wal_normal / s_wal_normal);
    println!("rustqlite wal-normal vs own delete:  {:.2}x", wal_normal / del);
}
