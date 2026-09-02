//! Out-of-memory fault injection — modeled on §3.1 of
//! https://www.sqlite.org/testing.html
//!
//! SQLite: "SQLite allows an application to substitute an alternative
//! malloc() implementation ... These instrumented mallocs can be rigged to
//! fail after a certain number of allocations. OOM tests are done in a
//! loop. On the first iteration of the loop, the instrumented malloc is
//! rigged to fail on the first allocation. Then some SQLite operation is
//! carried out and checks are done to make sure SQLite handled the OOM
//! error correctly. Then the time-to-failure counter on the instrumented
//! malloc is increased by one and the test is repeated."
//!
//! This file implements that loop for rustqlite using the library's
//! `oom-injection` feature (the memsys2 equivalent — see
//! `src/oom_alloc.rs`):
//!
//!   - The test binary is spawned as a child with `RUSTQLITE_OOM_AT=N`:
//!     every allocation from #N on returns null ("fail continuously after
//!     the first failure", in SQLite's words). Allocation failure aborts
//!     the child process.
//!   - After every child death the parent re-opens the database and
//!     verifies the committed baseline survived and the file is still
//!     writable — an OOM must never corrupt or poison anything.
//!   - A calibration run measures the workload's total allocation count
//!     so the fault point can sweep the entire allocation range.
//!
//! This test requires the `oom-injection` feature (it swaps the global
//! allocator), so run it as:
//!
//!     cargo test --features oom-injection --test oom_fault
//!     cargo test --features oom-injection          # whole suite + OOM loop
//!
//! (Plain `cargo test` skips this target — mirroring how SQLite's OOM
//! tests need the special SQLITE_MEMDEBUG build.)

use rustqlite::oom_alloc::{allocation_count, reset_allocation_count, set_fail_at};
use rustqlite::{Database, Value};
use std::process::Command;

const BASELINE_ROWS: i64 = 200;

/// The fixed workload every child runs against the parent's baseline DB:
/// a representative mix of DDL, index build, parameterized DML, scans,
/// aggregation, and transactions.
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
        if db
            .execute(
                "INSERT INTO oom_t (v, r) VALUES (?, ?)",
                [
                    Value::Text(format!("child-{}", i).into()),
                    Value::Real(i as f64 / 4.0),
                ],
            )
            .is_err()
        {
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
    // ---- Child mode: rig the allocator from the env, run the workload. ----
    if let Ok(at) = std::env::var("RUSTQLITE_OOM_AT") {
        let db_path = std::env::var("RUSTQLITE_OOM_DB").unwrap_or_default();
        let path = std::path::PathBuf::from(&db_path);
        if at != "none" {
            let n: usize = at.parse().unwrap_or(usize::MAX);
            set_fail_at(n);
        }
        let code = child_workload(&path);
        // Reaching here with a fail point set means the workload never
        // allocated that far (or completed under pressure) — still a valid
        // outcome the parent understands.
        std::process::exit(code);
    }

    // ---- Parent mode: build baseline, calibrate, run the fault loop. ----
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("oom.db");
    {
        let mut db = Database::open(&db_path).unwrap();
        db.execute("CREATE TABLE keep (id INTEGER PRIMARY KEY, v TEXT)", [])
            .unwrap();
        for i in 1..=BASELINE_ROWS {
            db.execute(
                "INSERT INTO keep (v) VALUES (?)",
                [Value::Text(format!("keep-{}", i).into())],
            )
            .unwrap();
        }
        db.flush().unwrap();
    }

    let exe = std::env::current_exe().unwrap();
    let spawn = |oom_at: &str| -> std::process::ExitStatus {
        Command::new(&exe)
            .env_clear()
            .env("RUSTQLITE_OOM_DB", &db_path)
            .env("RUSTQLITE_OOM_AT", oom_at)
            .status()
            .expect("failed to spawn OOM child")
    };

    // Calibrate: run the workload IN THIS PROCESS with injection off and
    // count the allocations it uses (the child's own process-startup
    // allocations shift the mapping slightly; every fault point is still a
    // valid OOM exercise, which is all the invariant below needs).
    reset_allocation_count();
    let code = child_workload(&db_path);
    assert_eq!(code, 0, "in-process calibration workload failed: {}", code);
    let total = allocation_count();
    println!(
        "oom_fault: workload uses ~{} allocations; sweeping fault points",
        total
    );

    // Sweep: dense over the low range (startup + engine init), strided
    // over the rest. RUSTQLITE_OOM_SAMPLES=big for a near-exhaustive
    // SQLite-style one-at-a-time loop.
    let samples_env = std::env::var("RUSTQLITE_OOM_SAMPLES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(150);
    let stride = (total / samples_env).max(1);
    let mut points: Vec<usize> = (1..=total.min(400)).collect();
    let mut n = 401usize;
    while n <= total {
        points.push(n);
        n += stride;
    }

    let mut aborted = 0usize;
    let mut survived = 0usize;
    for &at in &points {
        let status = spawn(&at.to_string());
        if status.success() {
            survived += 1; // fault point beyond the child's actual range
        } else {
            aborted += 1;
        }
        // INVARIANT: no matter where the allocation failed, the committed
        // baseline survives and the file stays openable + writable.
        let mut db = Database::open(&db_path)
            .unwrap_or_else(|e| panic!("OOM@{}: baseline DB failed to re-open: {}", at, e));
        let rows = db
            .query("SELECT COUNT(*) FROM keep", [])
            .unwrap_or_else(|e| panic!("OOM@{}: baseline read failed: {}", at, e));
        let cnt = match rows.first().and_then(|r| r.first()) {
            Some(Value::Integer(n)) => *n,
            other => panic!("OOM@{}: COUNT(*) returned {:?}", at, other),
        };
        assert_eq!(
            cnt, BASELINE_ROWS,
            "OOM@{}: committed baseline corrupted ({} rows, expected {})",
            at, cnt, BASELINE_ROWS
        );
        db.execute("INSERT INTO keep (v) VALUES ('oom-probe')", [])
            .unwrap_or_else(|e| panic!("OOM@{}: post-failure write failed: {}", at, e));
        db.execute("DELETE FROM keep WHERE v = 'oom-probe'", [])
            .unwrap_or_else(|e| panic!("OOM@{}: post-failure delete failed: {}", at, e));
        db.flush().unwrap();
    }

    // Control: with injection off, the workload must complete cleanly.
    let status = spawn("none");
    assert!(
        status.success(),
        "control (no OOM) run failed: {:?}",
        status
    );

    println!(
        "oom_fault: {} fault points exercised ({} aborted children, {} beyond range) — baseline intact at every point",
        points.len(),
        aborted,
        survived
    );
}
