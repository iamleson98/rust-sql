//! probe_rss_floor — attribute the engine's peak-RSS floor vs SQLite,
//! phase by phase, on the torture matrix's own shapes.
//!
//! The CI torture matrix reports per-section peak RSS in isolated child
//! processes; this probe prints an RSS CHECKPOINT WALK through one
//! representative workload (S17's build shape: WAL + synchronous=OFF,
//! 1M-row single-txn load, then reopen + full-scan aggregate) so the
//! floor's components can be attributed:
//!
//!   baseline -> open -> CREATE -> mid-txn (25/50/75/100%) -> COMMIT
//!     -> drop -> reopen -> scan -> drop
//!
//! Usage:
//!   cargo run --release --example probe_rss_floor -- [rows] [rq|sq]
//!
//! Environment: mirrors the CI torture job (MIMALLOC_ARENA_RESERVE=64M,
//! MIMALLOC_ALLOW_THP=0 are set by the CI env; set them locally too for
//! a faithful floor).

use rustqlite::{Database, Value};

#[cfg(target_os = "linux")]
fn rss_and_hwm_mb() -> (f64, f64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let mut rss = 0u64;
    let mut hwm = 0u64;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            rss = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("VmHWM:") {
            hwm = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        }
    }
    (rss as f64 / 1024.0, hwm as f64 / 1024.0)
}

#[cfg(not(target_os = "linux"))]
fn rss_and_hwm_mb() -> (f64, f64) {
    // Non-Linux: current RSS only (the torture harness has the full
    // per-OS matrix; this probe is for the Linux floor walk).
    (0.0, 0.0)
}

fn mark(tag: &str) {
    let (rss, hwm) = rss_and_hwm_mb();
    println!("RSS  {tag:28} cur={rss:7.2} MB  hwm={hwm:7.2} MB");
}

fn main() {
    let rows: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000_000);
    let which = std::env::args().nth(2).unwrap_or_else(|| "rq".to_string());

    // Deterministic values like S17 (name/val/score).
    if which == "sq" {
        let path = "/tmp/probe_rss_floor.sq.db";
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{path}-wal"));
        let _ = std::fs::remove_file(format!("{path}-shm"));
        use rusqlite::params;
        mark("sq baseline");
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=OFF;")
            .unwrap();
        conn.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
            [],
        )
        .unwrap();
        mark("sq after create");
        conn.execute("BEGIN", []).unwrap();
        let mut stmt = conn
            .prepare("INSERT INTO t (name, val, score) VALUES (?1, ?2, ?3)")
            .unwrap();
        for i in 1..=rows {
            stmt.execute(params![format!("name{i}"), i as i64, i as f64 * 1.5])
                .unwrap();
            if i % (rows / 4).max(1) == 0 {
                mark(&format!("sq txn {}%", 100 * i / rows));
            }
        }
        drop(stmt);
        conn.execute("COMMIT", []).unwrap();
        mark("sq after commit");
        let v: i64 = conn
            .query_row("SELECT SUM(val) FROM t", [], |r| r.get(0))
            .unwrap();
        println!("sum={v}");
        mark("sq after sum (same conn)");
        drop(conn);
        mark("sq after drop");
        let conn = rusqlite::Connection::open(path).unwrap();
        let v: i64 = conn
            .query_row("SELECT SUM(val) FROM t", [], |r| r.get(0))
            .unwrap();
        println!("sum={v}");
        mark("sq after reopen+sum");
        drop(conn);
        mark("sq final");
        return;
    }

    let path = "/tmp/probe_rss_floor.rq.db";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let _ = std::fs::remove_file(format!("{path}-spill"));
    mark("rq baseline");
    let mut db = Database::open(path).unwrap();
    db.execute("PRAGMA journal_mode=WAL", []).unwrap();
    db.execute("PRAGMA synchronous=OFF", []).unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    mark("rq after create");
    db.execute("BEGIN", []).unwrap();
    for i in 1..=rows {
        db.execute(
            "INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [
                Value::Text(format!("name{i}").into()),
                Value::Integer(i as i64),
                Value::Real(i as f64 * 1.5),
            ],
        )
        .unwrap();
        if i % (rows / 4).max(1) == 0 {
            mark(&format!("rq txn {}%", 100 * i / rows));
        }
    }
    db.execute("COMMIT", []).unwrap();
    mark("rq after commit");
    let out = db.query("SELECT SUM(val) FROM t", []).unwrap();
    if let Some(r) = out.first() {
        println!("sum={}", r[0]);
    }
    mark("rq after sum (same conn)");
    drop(db);
    mark("rq after drop");
    let db = Database::open(path).unwrap();
    let out = db.query("SELECT SUM(val) FROM t", []).unwrap();
    if let Some(r) = out.first() {
        println!("sum={}", r[0]);
    }
    mark("rq after reopen+sum");
    drop(db);
    mark("rq final");
}
