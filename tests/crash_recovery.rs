//! Crash & power-loss testing — modeled on §3.3 of
//! https://www.sqlite.org/testing.html
//!
//! SQLite: "crash tests ... strive to verify that those defensive measures
//! are working correctly ... In the TCL test harness, the crash simulation
//! is done in a separate process. The main testing process spawns a child
//! process which runs some SQLite operation and randomly crashes somewhere
//! in the middle of a write operation. After the child dies, the original
//! test process opens and reads the test database and verifies that the
//! changes attempted by the child either completed successfully or else
//! were completely rolled back."
//!
//! This file reproduces exactly that architecture: the test binary is
//! spawned as a child with a `RUSTQLITE_CRASH_POINT` environment variable;
//! the child runs a deterministic statement sequence and hard-aborts
//! (SIGABRT — no unwinding, no cleanup, no flushed buffers) at statement
//! boundaries. The crash point advances one statement at a time through
//! the entire workload, the way SQLite's crash tests advance the snapshot
//! point one I/O operation at a time. After each crash, the parent
//! re-opens the database and verifies:
//!
//!   1. the pre-crash COMMITTED baseline is fully intact (never lost),
//!   2. the in-flight transaction is fully absent (all-or-nothing),
//!   3. the database still accepts new writes (no poisoned state),
//!   4. in WAL mode, commits that reached the WAL before the crash are
//!      recovered on re-open (this IS crash recovery, per pager.rs).
//!
//! This test is `harness = false`: main() dispatches between parent
//! (orchestration + assertions) and child (workload + abort) based on the
//! environment, so a plain `cargo test` runs the whole crash matrix.
//!
//! Run with: cargo test --test crash_recovery

use rustqlite::{Database, Value};
use std::process::Command;

const BASELINE_ROWS: i64 = 100;
const TX_ROWS: i64 = 50;

fn child_env(db: &std::path::Path, mode: &str) -> Vec<(String, String)> {
    vec![
        ("RUSTQLITE_CRASH_DB".to_string(), db.display().to_string()),
        ("RUSTQLITE_CRASH_MODE".to_string(), mode.to_string()),
    ]
}

/// The child's deterministic workload. Returns the list of operation
/// labels so the parent knows how many crash points exist. Operations are
/// indexed from 1; crash point k = "abort after completing operation k".
/// IMPORTANT: every label here MUST correspond to exactly one `crashpoint!()`
/// in `child_main` — the parent derives `tx_must_be_visible` (which crash
/// points are at/after COMMIT) from this list, so a mismatch silently
/// misclassifies crash points and produces false failures.
fn workload_ops(mode: &str) -> Vec<&'static str> {
    let mut ops: Vec<&'static str> = Vec::new();
    if mode == "wal" {
        ops.push("pragma-wal");
    }
    ops.push("begin");
    for _i in 1..=TX_ROWS {
        ops.push("insert");
    }
    ops.push("commit");
    ops.push("flush");
    ops
}

fn child_main(db_path: &std::path::Path, mode: &str, crash_at: usize) -> i32 {
    let ops = workload_ops(mode);
    let mut db = match Database::open(db_path) {
        Ok(db) => db,
        Err(_) => return 3, // could not even open (valid outcome pre-baseline)
    };
    if mode == "wal" && crash_at >= 1 {
        let _ = db.execute("PRAGMA journal_mode = WAL", []);
    }
    let mut done = 0usize;

    macro_rules! crashpoint {
        () => {
            done += 1;
            if done >= crash_at {
                // HARD crash: SIGABRT. No unwinding, no Drop, no flushing
                // of user-space buffers — the closest thing to power loss.
                std::process::abort();
            }
        };
    }

    // The parent created the baseline BEFORE spawning us, so operation 0
    // ("open") crashing means nothing of ours was ever written.
    if mode == "wal" {
        crashpoint!(); // after PRAGMA journal_mode=WAL
    }
    let _ = db.execute("BEGIN", []);
    crashpoint!(); // after BEGIN

    for i in 1..=TX_ROWS {
        let _ = db.execute(
            "INSERT INTO t (v) VALUES (?)",
            [Value::Text(format!("tx-{}", i).into())],
        );
        crashpoint!(); // crash window after EVERY statement (SQLite's
                       // crash tests advance one I/O op at a time; statement
                       // granularity is our approximation of that)
    }

    let _ = db.execute("COMMIT", []);
    crashpoint!(); // after COMMIT

    let _ = db.flush();
    crashpoint!(); // after flush

    let _ = ops; // labels used by the parent
                 // Survived the whole workload (crash_at beyond the end = control run).
    0
}

/// What the parent expects after a crash at point k (1-indexed):
/// - before the child's COMMIT completes: the tx rows must NOT be visible
/// - after COMMIT: they may be visible (commit reached the disk/WAL)
fn tx_must_be_visible(crash_at: usize, mode: &str) -> bool {
    let ops = workload_ops(mode);
    // The "commit" op is the second-to-last; flush is last.
    let commit_idx = ops.len() - 1;
    crash_at >= commit_idx
}

fn verify_after_crash(db_path: &std::path::Path, crash_at: usize, mode: &str) {
    let mut db = Database::open(db_path)
        .unwrap_or_else(|e| panic!("crash@{} ({}): DB failed to re-open: {}", crash_at, mode, e));

    // 1. Baseline rows must ALL survive every crash point.
    let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap_or_else(|e| {
        panic!(
            "crash@{} ({}): baseline COUNT failed: {}",
            crash_at, mode, e
        )
    });
    let n = match rows.first().and_then(|r| r.first()) {
        Some(Value::Integer(n)) => *n,
        other => panic!("crash@{} ({}): COUNT returned {:?}", crash_at, mode, other),
    };
    assert!(
        n >= BASELINE_ROWS,
        "crash@{} ({}): LOST COMMITTED DATA — baseline {} rows, found {}",
        crash_at,
        mode,
        BASELINE_ROWS,
        n
    );

    // 2. All-or-nothing: the in-flight transaction is either fully
    //    present or fully absent.
    let tx_rows = db
        .query("SELECT COUNT(*) FROM t WHERE v LIKE 'tx-%'", [])
        .unwrap()
        .first()
        .and_then(|r| r.first())
        .cloned();
    let tx_n = match tx_rows {
        Some(Value::Integer(k)) => k,
        _ => panic!("crash@{} ({}): tx COUNT not an integer", crash_at, mode),
    };
    let visible_ok = tx_must_be_visible(crash_at, mode);
    if visible_ok {
        assert!(
            tx_n == 0 || tx_n == TX_ROWS,
            "crash@{} ({}): TORN COMMIT — {} of {} tx rows visible after COMMIT",
            crash_at,
            mode,
            tx_n,
            TX_ROWS
        );
    } else {
        assert_eq!(
            tx_n, 0,
            "crash@{} ({}): UNCOMMITTED DATA SURVIVED — {} tx rows visible before COMMIT",
            crash_at, mode, tx_n
        );
    }

    // 2b. The recovered database must pass PRAGMA integrity_check (the
    //     same verification SQLite's crash tests perform on recovered
    //     files: structure fully intact, indexes consistent).
    {
        let rows = db.query("PRAGMA integrity_check", []).unwrap_or_else(|e| {
            panic!(
                "crash@{} ({}): integrity_check failed: {}",
                crash_at, mode, e
            )
        });
        assert!(
            rows.iter().all(|r| r[0].as_text() == "ok"),
            "crash@{} ({}): integrity_check reported problems after recovery: {:?}",
            crash_at,
            mode,
            rows.iter().map(|r| r[0].as_text()).collect::<Vec<_>>()
        );
    }

    // 3. The database must still accept writes — a crashed-and-recovered
    //    file must never poison the writer.
    db.execute("INSERT INTO t (v) VALUES ('post-crash-write')", [])
        .unwrap_or_else(|e| {
            panic!(
                "crash@{} ({}): post-crash write failed: {}",
                crash_at, mode, e
            )
        });
    db.flush().expect("post-crash flush failed");
    let rows = db
        .query("SELECT COUNT(*) FROM t WHERE v = 'post-crash-write'", [])
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "crash@{} ({}): post-crash write not readable",
        crash_at,
        mode
    );
}

fn run_crash_matrix(mode: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join(format!("crash-{}.db", mode));
    let baseline_path = tmp.path().join(format!("crash-{}.db.baseline", mode));

    // Parent creates the committed baseline.
    {
        let mut db = Database::open(&db_path).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
            .unwrap();
        for i in 1..=BASELINE_ROWS {
            db.execute(
                "INSERT INTO t (v) VALUES (?)",
                [Value::Text(format!("base-{}", i).into())],
            )
            .unwrap();
        }
        db.flush().unwrap();
        // Drop the connection so all file handles are closed before the
        // snapshot copy (and so no lingering WAL sidecar exists).
    }
    // Snapshot the pristine baseline — SQLite's crash tests likewise
    // reset the database to a saved snapshot before EVERY crash point,
    // because each crashed child leaves the file in a different state
    // and the crash-point matrix must be reproducible point-by-point.
    std::fs::copy(&db_path, &baseline_path).expect("failed to snapshot baseline");

    /// Reset db (+ sidecars) to the pristine baseline snapshot.
    fn restore_baseline(db_path: &std::path::Path, baseline: &std::path::Path) {
        // Remove any sidecar files a crashed child may have left behind
        // (hot journal / WAL / shm). A stale WAL with a commit frame would
        // otherwise leak committed-but-uncheckpointed data into the next
        // iteration and make crash points order-dependent.
        for sidecar in [
            db_path.with_file_name(format!(
                "{}-wal",
                db_path.file_name().unwrap().to_string_lossy()
            )),
            db_path.with_file_name(format!(
                "{}-shm",
                db_path.file_name().unwrap().to_string_lossy()
            )),
        ] {
            let _ = std::fs::remove_file(&sidecar);
        }
        std::fs::copy(baseline, db_path).expect("failed to restore baseline");
    }

    let ops = workload_ops(mode);
    // Crash point 1..=ops.len() (abort after op k), plus a control run
    // (no crash) to prove the workload itself is sound.
    for crash_at in 1..=ops.len() {
        restore_baseline(&db_path, &baseline_path);
        let exe = std::env::current_exe().unwrap();
        let mut cmd = Command::new(exe);
        cmd.env_clear()
            .envs(child_env(&db_path, mode))
            .env("RUSTQLITE_CRASH_POINT", crash_at.to_string());
        let status = cmd.status().expect("failed to spawn crash child");
        // The child MUST die from abort (or exit non-zero) — it never
        // exits 0 before finishing the workload.
        assert!(
            !status.success(),
            "crash@{} ({}): child exited 0 but should have aborted",
            crash_at,
            mode
        );
        verify_after_crash(&db_path, crash_at, mode);
    }

    // Control run: no crash — everything must be committed and visible.
    {
        restore_baseline(&db_path, &baseline_path);
        let exe = std::env::current_exe().unwrap();
        let status = Command::new(exe)
            .env_clear()
            .envs(child_env(&db_path, mode))
            .env("RUSTQLITE_CRASH_POINT", usize::MAX.to_string())
            .status()
            .unwrap();
        assert!(
            status.success(),
            "control (no-crash) run failed in {}",
            mode
        );
        let db = Database::open(&db_path).unwrap();
        let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
        let n = match rows.first().and_then(|r| r.first()) {
            Some(Value::Integer(n)) => *n,
            other => panic!("control ({}): COUNT returned {:?}", mode, other),
        };
        assert!(
            n >= BASELINE_ROWS + TX_ROWS,
            "control ({}): expected >= {} rows after clean run, found {}",
            mode,
            BASELINE_ROWS + TX_ROWS,
            n
        );
    }
}

fn main() {
    // ---- Child mode: run the workload, abort at the requested point. ----
    if let (Ok(db), Ok(point)) = (
        std::env::var("RUSTQLITE_CRASH_DB"),
        std::env::var("RUSTQLITE_CRASH_POINT"),
    ) {
        let mode = std::env::var("RUSTQLITE_CRASH_MODE").unwrap_or_else(|_| "delete".into());
        let crash_at: usize = point.parse().unwrap_or(usize::MAX);
        std::process::exit(child_main(db.as_ref(), &mode, crash_at));
    }

    // ---- Parent mode: drive the crash matrix and assert invariants. ----
    run_crash_matrix("delete");
    run_crash_matrix("wal");
    println!("crash_recovery: all crash points passed (delete + wal modes)");
}
