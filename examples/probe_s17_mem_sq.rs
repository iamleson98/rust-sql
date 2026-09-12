//! S17 memory forensics — SQLite side: identical shape as probe_s17_mem.rs.

use rusqlite::{params, Connection};

fn mem() -> (f64, f64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let mut rss = 0.0;
    let mut hwm = 0.0;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            rss = rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<f64>()
                .unwrap()
                / 1024.0;
        }
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            hwm = rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<f64>()
                .unwrap()
                / 1024.0;
        }
    }
    (rss, hwm)
}

fn show(stage: &str) {
    let (rss, hwm) = mem();
    println!("{stage:44} rss={rss:6.1}MB hwm={hwm:6.1}MB");
}

fn main() {
    let rows: i64 = 1_000_000;
    let path = "/tmp/s17probe.sq.db";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    show("start");
    {
        let conn = Connection::open(path).unwrap();
        let _ = conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=OFF;");
        conn.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
            [],
        )
        .unwrap();
        show("after open+create");
        conn.execute("BEGIN", []).unwrap();
        for i in 1..=rows {
            conn.execute(
                "INSERT INTO t (name, val, score) VALUES (?1, ?2, ?3)",
                params![format!("name{i}"), i, i as f64 * 1.5],
            )
            .unwrap();
        }
        conn.execute("COMMIT", []).unwrap();
        show("after 1M-row insert txn (file)");
    }
    show("after drop Connection");

    let t = std::time::Instant::now();
    let conn = Connection::open(path).unwrap();
    show("after reopen");
    let v: i64 = conn
        .query_row("SELECT SUM(val) FROM t", [], |r| r.get(0))
        .unwrap();
    println!("sum = {v}");
    show("after SUM query");
    let conn2 = Connection::open(path).unwrap();
    let c: i64 = conn2
        .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
        .unwrap();
    println!("count = {c}");
    show("after 2nd open+COUNT");
    println!("query time: {:.1}ms", t.elapsed().as_secs_f64() * 1000.0);
}
