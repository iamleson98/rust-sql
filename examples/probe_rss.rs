//! RSS parity probe: engine vs SQLite on the same workloads, measuring
//! peak resident set (VmHWM) of the CURRENT process (page cache, row
//! materialization, streaming — the whole engine footprint) via
//! /proc/self/status. Scenarios mirror the user's memory goal: less than
//! SQLite, else parity.
//!
//! SQLite side runs in a CHILD process (linked rusqlite) so its peak RSS
//! is measured cleanly; we parse its VmHWM from /proc/<pid>/status at exit.
use std::process::{Command, Stdio};

fn vm_hwm_kb() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            return rest.trim().trim_end_matches("kB").trim().parse().ok();
        }
    }
    None
}

fn fmt_kb(kb: Option<u64>) -> String {
    match kb {
        Some(k) if k >= 1024 * 1024 => format!("{:.2} GiB", k as f64 / 1048576.0),
        Some(k) if k >= 1024 => format!("{:.1} MiB", k as f64 / 1024.0),
        Some(k) => format!("{k} KiB"),
        None => "n/a".into(),
    }
}

/// Baseline: the process before touching any DB (argv/alloc/libs).
fn baseline_hwm() -> u64 {
    vm_hwm_kb().unwrap_or(0)
}

/// File-backed: seed N rows in one txn, reopen (cold cache), full scan.
/// Peak RSS = engine + 512-page cache + streaming row budget.
fn engine_file_scan(path: &str, n: i64) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let _ = std::fs::remove_file(format!("{path}-shm"));
    {
        let mut db = rustqlite::Database::open(path).unwrap();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b REAL, c TEXT)",
            [],
        )
        .unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 1..=n {
            db.execute(
                "INSERT INTO t (a, b, c) VALUES (?, ?, ?)",
                [
                    rustqlite::Value::Integer(i * 3),
                    rustqlite::Value::Real(i as f64 / 7.0),
                    rustqlite::Value::Text(format!("payload-{i:08}").into()),
                ],
            )
            .unwrap();
        }
        db.execute("COMMIT", []).unwrap();
    }
    eprintln!("HWM_KB_POST_SEED={}", vm_hwm_kb().unwrap_or(0));
    // Cold-cache reopen + full streaming scan + a point-lookup phase.
    let db = rustqlite::Database::open(path).unwrap();
    let mut stmt = db.prepare("SELECT id, a, b, c FROM t").unwrap();
    stmt.bind_all(&[]).unwrap();
    let mut seen: i64 = 0;
    let mut sink = 0i64;
    while let rustqlite::StepResult::Row = stmt.step().unwrap() {
        let v = stmt.column_int(0);
        sink += v / 7;
        seen += 1;
    }
    assert_eq!(seen, n, "scan must see every row");
    std::hint::black_box(sink);
}

/// In-memory: the cache IS the store — peak RSS ~ DB size + overhead.
fn engine_memory(n: i64) {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b REAL, c TEXT)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=n {
        db.execute(
            "INSERT INTO t (a, b, c) VALUES (?, ?, ?)",
            [
                rustqlite::Value::Integer(i * 3),
                rustqlite::Value::Real(i as f64 / 7.0),
                rustqlite::Value::Text(format!("payload-{i:08}").into()),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let rows = db.query("SELECT COUNT(*), SUM(a) FROM t", []).unwrap();
    std::hint::black_box(&rows);
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let n: i64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000);
    match mode.as_str() {
        "engine-file" => engine_file_scan("/home/z/my-project/rss_probe_file.db", n),
        "engine-mem" => engine_memory(n),
        "child-sqlite-file" => {
            // Runs in the child: mirror the sqlite side. Exit code 0.
            let path = "/home/z/my-project/rss_probe_sqlite.db";
            sqlite_file_scan(path, n);
        }
        "child-sqlite-mem" => {
            sqlite_memory(n);
        }
        _ => {
            // Parent: baseline, then each scenario in children, reading
            // their VmHWM right before they exit is unreliable — instead
            // each child prints its own HWM at the end to stderr and the
            // parent captures it.
            let base = baseline_hwm();
            println!("parent baseline HWM: {}", fmt_kb(Some(base)));
            for &(label, mode, n) in &[
                ("file 100k scan", "engine-file", 100_000i64),
                ("file 1M scan", "engine-file", 1_000_000),
                ("mem 100k", "engine-mem", 100_000),
                ("mem 1M", "engine-mem", 1_000_000),
            ] {
                let out = Command::new(std::env::current_exe().unwrap())
                    .args([&mode.to_string(), &n.to_string()])
                    .stderr(Stdio::piped())
                    .output()
                    .unwrap();
                let hwm = String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .find_map(|l| l.strip_prefix("HWM_KB=").map(|v| v.to_string()))
                    .and_then(|v| v.parse::<u64>().ok());
                let seed = String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .find_map(|l| l.strip_prefix("HWM_KB_POST_SEED=").map(|v| v.to_string()))
                    .and_then(|v| v.parse::<u64>().ok());
                if !out.status.success() {
                    eprintln!(
                        "engine child failed: {}",
                        String::from_utf8_lossy(&out.stderr)
                    );
                }
                println!(
                    "engine {label:18} peak: {} (post-seed {})",
                    fmt_kb(hwm),
                    fmt_kb(seed)
                );
            }
            for &(label, mode, n) in &[
                ("file 100k scan", "child-sqlite-file", 100_000i64),
                ("file 1M scan", "child-sqlite-file", 1_000_000),
                ("mem 100k", "child-sqlite-mem", 100_000),
                ("mem 1M", "child-sqlite-mem", 1_000_000),
            ] {
                let out = Command::new(std::env::current_exe().unwrap())
                    .args([&mode.to_string(), &n.to_string()])
                    .stderr(Stdio::piped())
                    .output()
                    .unwrap();
                let hwm = String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .find_map(|l| l.strip_prefix("HWM_KB=").map(|v| v.to_string()))
                    .and_then(|v| v.parse::<u64>().ok());
                println!("sqlite {label:18} peak: {}", fmt_kb(hwm));
            }
        }
    }
    if let Some(kb) = vm_hwm_kb() {
        eprintln!("HWM_KB={kb}");
    }
}

// ---- SQLite side (child process; rusqlite is a dev-dependency) ----

fn sqlite_file_scan(path: &str, n: i64) {
    use rusqlite::Connection;
    let _ = std::fs::remove_file(path);
    {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b REAL, c TEXT)")
            .unwrap();
        conn.execute_batch("BEGIN").unwrap();
        {
            let mut stmt = conn
                .prepare("INSERT INTO t (a, b, c) VALUES (?, ?, ?)")
                .unwrap();
            for i in 1..=n {
                stmt.execute(rusqlite::params![
                    i * 3,
                    i as f64 / 7.0,
                    format!("payload-{i:08}")
                ])
                .unwrap();
            }
        }
        conn.execute_batch("COMMIT").unwrap();
    }
    // Cold reopen + full scan + aggregate.
    let conn = Connection::open(path).unwrap();
    let mut seen = 0i64;
    {
        let mut stmt = conn.prepare("SELECT id, a, b, c FROM t").unwrap();
        let mut rows = stmt.query([]).unwrap();
        while let Some(r) = rows.next().unwrap() {
            let v: i64 = r.get(0).unwrap();
            std::hint::black_box(v);
            seen += 1;
        }
    }
    assert_eq!(seen, n);
}

fn sqlite_memory(n: i64) {
    use rusqlite::Connection;
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b REAL, c TEXT)")
        .unwrap();
    conn.execute_batch("BEGIN").unwrap();
    {
        let mut stmt = conn
            .prepare("INSERT INTO t (a, b, c) VALUES (?, ?, ?)")
            .unwrap();
        for i in 1..=n {
            stmt.execute(rusqlite::params![
                i * 3,
                i as f64 / 7.0,
                format!("payload-{i:08}")
            ])
            .unwrap();
        }
    }
    conn.execute_batch("COMMIT").unwrap();
    let (cnt, sum): (i64, i64) = conn
        .query_row("SELECT COUNT(*), SUM(a) FROM t", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    std::hint::black_box((cnt, sum));
}
