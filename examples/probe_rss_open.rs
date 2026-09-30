//! probe_rss_open — fine-grained attribution of the Database::open path's
//! RSS footprint, plus the reopen-cycle ratchet (open/close loops on an
//! existing file, with and without a parallel-scan query).
//!
//! Usage: cargo run --release --example probe_rss_open -- [mode]
//!   modes: fresh   (open fresh file, PRAGMA, CREATE, checkpoints)
//!          cycles  (open/close x6 on existing file, no queries)
//!          scans   (open/query-SUM/close x6 on existing file)
//!          serial  (like scans but parallel scan disabled via PRAGMA)

use rustqlite::{Database, Value};

#[cfg(target_os = "linux")]
fn rss_mb() -> f64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0)
                / 1024.0;
        }
    }
    0.0
}
#[cfg(not(target_os = "linux"))]
fn rss_mb() -> f64 {
    0.0
}

fn mark(tag: &str) {
    println!("RSS  {tag:44} {:#7.2} MB", rss_mb());
}

fn main() {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "fresh".to_string());
    if mode == "stats" {
        // attribution: engine live vs allocator-retained, at each phase
        let path = "/tmp/probe_stats.db";
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{path}-wal"));
        mark("baseline");
        eprintln!("=== mi stats: baseline ===");
        mi_stats();
        let mut db = Database::open(path).unwrap();
        mark("after open");
        eprintln!("=== mi stats: after open ===");
        mi_stats();
        db.execute("PRAGMA journal_mode=WAL", []).unwrap();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
            [],
        )
        .unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 1..=50_000i64 {
            db.execute(
                "INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
                [
                    Value::Text(format!("name{i}").into()),
                    Value::Integer(i),
                    Value::Real(1.5),
                ],
            )
            .unwrap();
        }
        db.execute("COMMIT", []).unwrap();
        mark("after 50k txn");
        eprintln!("=== mi stats: after txn ===");
        mi_stats();
        let _ = db.query("SELECT SUM(val) FROM t", []).unwrap();
        mark("after SUM");
        eprintln!("=== mi stats: after SUM ===");
        mi_stats();
        drop(db);
        mark("after drop");
        eprintln!("=== mi stats: after drop ===");
        mi_stats();
        return;
    }
    let path = match mode.as_str() {
        "cycles" | "scans" | "serial" => "/tmp/probe_open_cycle.db",
        _ => "/tmp/probe_open_fresh.db",
    };
    if mode == "fresh" {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{path}-wal"));
        let _ = std::fs::remove_file(format!("{path}-spill"));
        mark("baseline");
        let mut db = Database::open(path).unwrap();
        mark("after open (fresh file)");
        db.execute("PRAGMA journal_mode=WAL", []).unwrap();
        mark("after PRAGMA journal_mode=WAL");
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
            [],
        )
        .unwrap();
        mark("after CREATE TABLE");
        db.execute("BEGIN", []).unwrap();
        for i in 1..=50_000i64 {
            db.execute(
                "INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
                [
                    Value::Text(format!("name{i}").into()),
                    Value::Integer(i),
                    Value::Real(1.5),
                ],
            )
            .unwrap();
        }
        db.execute("COMMIT", []).unwrap();
        mark("after 50k-row txn + COMMIT");
        let _ = db.query("SELECT SUM(val) FROM t", []).unwrap();
        mark("after SUM (50k rows — below parallel threshold)");
        drop(db);
        mark("after drop");
        let db = Database::open(path).unwrap();
        mark("after reopen");
        let _ = db.query("SELECT SUM(val) FROM t", []).unwrap();
        mark("after SUM #2");
        drop(db);
        mark("final");
        return;
    }
    // cycles / scans / serial: build the file once (via a previous fresh run),
    // then measure the loop ratchet.
    mark("baseline");
    let n: usize = 6;
    for i in 0..n {
        let db = Database::open(path).unwrap();
        if mode == "scans" || mode == "serial" {
            let _ = db.query("SELECT SUM(val) FROM t", []).unwrap();
        }
        drop(db);
        mark(&format!(
            "cycle {i} open+{}+close",
            if mode == "cycles" { "NOTHING" } else { "SUM" }
        ));
    }
}

// mimalloc stats: what the ALLOCATOR holds (committed / in-use / freed-
// but-retained) vs what the engine holds live. `mi_stats_print_out`
// prints to stderr — capture per checkpoint with a marker.
#[allow(dead_code)]
fn mi_stats() {
    #[cfg(feature = "mimalloc")]
    unsafe {
        libmimalloc_sys::mi_stats_print_out(None, std::ptr::null_mut());
    }
}
