//! PROBE: exact syscall accounting (via /proc/self/io) for autocommit
//! commits on the SQLite-format container, fast vs slow append path.
use rustqlite::{Database, Value};
use std::time::Instant;

fn io_counters() -> (u64, u64, u64, u64) {
    // (rchar, wchar, syscr, syscw)
    let s = std::fs::read_to_string("/proc/self/io").unwrap();
    let mut r = (0u64, 0u64, 0u64, 0u64);
    for line in s.lines() {
        let (k, v) = line.split_once(": ").unwrap();
        let v: u64 = v.parse().unwrap();
        match k {
            "rchar" => r.0 = v,
            "wchar" => r.1 = v,
            "syscr" => r.2 = v,
            "syscw" => r.3 = v,
            _ => {}
        }
    }
    r
}

fn main() {
    let rows: i64 = 20_000;
    let m: i64 = 3_000;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.db");
    let mut db = Database::open_sqlite_format(&path).unwrap();
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute("PRAGMA synchronous = OFF", []).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INT)", [])
        .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=rows {
        db.execute(
            "INSERT INTO t(a, b) VALUES (?, ?)",
            [Value::Text(format!("a{i}").into()), Value::Integer(i)],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    // Autocommit INSERTs — one publish per statement. Measure wall
    // time AND the /proc/self/io counter deltas.
    let before = io_counters();
    let t0 = Instant::now();
    for i in 0..m {
        db.execute(
            "INSERT INTO t(a, b) VALUES (?, ?)",
            [
                Value::Text(format!("n{i}").into()),
                Value::Integer(rows + i),
            ],
        )
        .unwrap();
    }
    let dt = (t0.elapsed().as_secs_f64() / m as f64) * 1e6;
    let after = io_counters();
    println!("autocommit INSERT: {dt:.2} us/stmt",);
    println!(
        "  io deltas: rchar={} wchar={} read-syscalls={} write-syscalls={} (per commit: {:.1} rd / {:.1} wr)",
        after.0 - before.0,
        after.1 - before.1,
        after.2 - before.2,
        after.3 - before.3,
        (after.2 - before.2) as f64 / m as f64,
        (after.3 - before.3) as f64 / m as f64,
    );
}
