//! EXHAUSTIVE local OOM fault-sweep driver (research tool — the CI
//! `oom_fault` test samples this; run locally for full campaigns).
//!
//! Spawns the test binary's exact child workload (see tests/oom_fault.rs)
//! at EVERY allocation number in a range — each crashed child is followed
//! by an un-injected CONTROL child that must complete the whole workload
//! cleanly. Any control failure means a torn write survived a crash:
//! file the corruption before anything else reopens the database.
//!
//!     cargo run --no-default-features --features oom-injection \
//!         --example oom_sweep -- 1 2000
//!
//! State accumulates across fault points (each control run grows oom_t),
//! so different start ranges exercise different allocation landscapes.

use rustqlite::oom_alloc::set_fail_at;
use rustqlite::{Database, Value};
use std::process::Command;

fn child_workload(db_path: &std::path::Path) -> i32 {
    let mut db = match Database::open(db_path) {
        Ok(d) => d,
        Err(_) => return 3,
    };
    if db
        .execute(
            "CREATE TABLE IF NOT EXISTS oom_t (id INTEGER PRIMARY KEY, v TEXT, r REAL)",
            [],
        )
        .is_err()
    {
        return 4;
    }
    if db
        .execute("CREATE INDEX IF NOT EXISTS idx_oom_v ON oom_t(v)", [])
        .is_err()
    {
        return 5;
    }
    for i in 1..=50i64 {
        if let Err(e) = db.execute(
            "INSERT INTO oom_t (v, r) VALUES (?, ?)",
            [
                Value::Text(format!("child-{}", i).into()),
                Value::Real(i as f64 / 4.0),
            ],
        ) {
            eprintln!("oom_fault child: INSERT {} failed: {}", i, e);
            return 6;
        }
    }
    let _ = db.query("SELECT COUNT(*), SUM(r) FROM oom_t", []);
    let _ = db.query(
        "SELECT * FROM oom_t WHERE v LIKE 'child-%' ORDER BY r DESC LIMIT 10",
        [],
    );
    let _ = db.execute("UPDATE oom_t SET r = r + 1 WHERE id <= 25", []);
    let _ = db.execute("DELETE FROM oom_t WHERE id > 40", []);
    let _ = db.execute("BEGIN", []);
    let _ = db.execute("INSERT INTO oom_t (v, r) VALUES ('in-tx', 0)", []);
    let _ = db.execute("COMMIT", []);
    match db.flush() {
        Ok(()) => 0,
        Err(_) => 7,
    }
}

fn main() {
    if let Ok(at) = std::env::var("RUSTQLITE_OOM_AT") {
        let db_path = std::env::var("RUSTQLITE_OOM_DB").unwrap_or_default();
        let path = std::path::PathBuf::from(&db_path);
        if at != "none" {
            let n: usize = at.parse().unwrap_or(usize::MAX);
            set_fail_at(n);
        }
        std::process::exit(child_workload(&path));
    }
    let dir = std::path::PathBuf::from("/tmp/oomdbg");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("oom.db");
    let start = std::env::args()
        .nth(1)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1);
    let end = std::env::args()
        .nth(2)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1000);
    {
        let mut db = Database::open(&db_path).unwrap();
        db.execute("CREATE TABLE keep (id INTEGER PRIMARY KEY, v TEXT)", [])
            .unwrap();
        for i in 1..=200i64 {
            db.execute(
                "INSERT INTO keep (v) VALUES (?)",
                [Value::Text(format!("keep-{}", i).into())],
            )
            .unwrap();
        }
        db.flush().unwrap();
    }
    let exe = std::env::current_exe().unwrap();
    for at in start..=end {
        let status = Command::new(&exe)
            .env_clear()
            .env("RUSTQLITE_OOM_DB", &db_path)
            .env("RUSTQLITE_OOM_AT", at.to_string())
            .status()
            .unwrap();
        let probe = Command::new(&exe)
            .env_clear()
            .env("RUSTQLITE_OOM_DB", &db_path)
            .env("RUSTQLITE_OOM_AT", "none")
            .status()
            .unwrap();
        if !probe.success() {
            println!(
                "fault {}: crash={:?} CONTROL={:?} <<< BROKEN",
                at,
                status.code(),
                probe.code()
            );
            return;
        }
        if at % 250 == 0 {
            println!("fault {}: ok", at);
        }
    }
    println!("no control failure in range {}..={}", start, end);
}
