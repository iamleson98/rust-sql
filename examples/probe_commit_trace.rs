//! PROBE: calibrate per-commit syscalls against a no-op baseline.
use rustqlite::{Database, Value};

fn counters() -> (u64, u64) {
    let s = std::fs::read_to_string("/proc/self/io").unwrap();
    let mut rd = 0u64;
    let mut wr = 0u64;
    for line in s.lines() {
        if let Some((k, v)) = line.split_once(": ") {
            let v: u64 = v.parse().unwrap();
            if k == "syscr" {
                rd = v;
            } else if k == "syscw" {
                wr = v;
            }
        }
    }
    (rd, wr)
}

fn main() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.db");
    let mut db = Database::open_sqlite_format(&path).unwrap();
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute("PRAGMA synchronous = OFF", []).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INT)", [])
        .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=2000 {
        db.execute(
            "INSERT INTO t(a, b) VALUES (?, ?)",
            [Value::Text(format!("a{i}").into()), Value::Integer(i)],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    // Baseline: reads/writes of an EMPTY loop (no statements at all).
    let b0 = counters();
    for _ in 0..1000 {
        std::hint::black_box(());
    }
    let a0 = counters();
    println!(
        "no-op loop: +{} rd +{} wr per 1000 iter",
        a0.0 - b0.0,
        a0.1 - b0.1
    );

    // SELECT loop (read-only statements, no commit).
    let b1 = counters();
    for i in 0..1000 {
        let _ = db
            .query(
                "SELECT b FROM t WHERE id = ?",
                [Value::Integer(1 + (i % 2000))],
            )
            .unwrap();
    }
    let a1 = counters();
    println!(
        "SELECT loop: {} rd {} wr per 2000 stmts (2 autocheckpoints)",
        (a1.0 - b1.0),
        (a1.1 - b1.1)
    );

    // Autocommit INSERT loop.
    let b2 = counters();
    let t0 = std::time::Instant::now();
    for i in 0..2000 {
        db.execute(
            "INSERT INTO t(a, b) VALUES (?, ?)",
            [
                Value::Text(format!("n{i}").into()),
                Value::Integer(100_000 + i),
            ],
        )
        .unwrap();
    }
    let wall = (t0.elapsed().as_secs_f64() / 1000.0) * 1e6;
    let a2 = counters();
    println!(
        "autocommit INSERT loop: {} rd {} wr per 2000 stmts (2 autocheckpoints) ({wall:.2} us/stmt)",
        a2.0 - b2.0,
        a2.1 - b2.1
    );
    let ph = rustqlite::publish_phase_snapshot();
    println!(
        "phases: mutate {:.2} us | splice {:.2} us | tail {:.2} us | commit {:.2} us | checkpoint {:.2} us",
        ph[0] as f64 / 2000.0 / 1000.0,
        ph[1] as f64 / 2000.0 / 1000.0,
        ph[2] as f64 / 2000.0 / 1000.0,
        ph[3] as f64 / 2000.0 / 1000.0,
        ph[4] as f64 / 2000.0 / 1000.0,
    );
    let c = rustqlite::reader_counter_snapshot();
    println!(
        "reader: overlay_hits {} | cache_hits {} | file_opens {} | file_reads {}",
        c[0], c[1], c[2], c[3]
    );
    let t = rustqlite::commit_timer_snapshot();
    println!(
        "timers: can_splice {:.2} us | guard_probe {:.2} us | append_fast {:.2} us | append_slow {:.2} us | publish_total {:.2} us | publishes {}",
        t[0] as f64 / 2000.0 / 1000.0,
        t[1] as f64 / 2000.0 / 1000.0,
        t[2] as f64 / 2000.0 / 1000.0,
        t[3] as f64 / 2000.0 / 1000.0,
        t[4] as f64 / 2000.0 / 1000.0,
        t[5],
    );
}
