//! Diagnostic: phase-level timing of the 4-parallel-writer shape.
//!
//! Splits the wall time into: statement phase (4 writers × 250 INSERTs,
// parallel DML in shadows) vs commit phase (4 serialized commits), to
//! see whether the serial critical section or the parallel statements
//! dominate — the input for widening the concurrent-write margin.
//!
//! Run: cargo run --release --example probe_cw_phases

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Instant;

use rustqlite::{Database, Value};

fn main() {
    let n_writers: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);
    let per_writer: i64 = (1000 / n_writers as i64).max(1);
    let mut total_stmt = std::time::Duration::ZERO;
    let mut total_commit = std::time::Duration::ZERO;
    let rounds = 7;
    for r in 0..rounds {
        let path = std::env::temp_dir().join(format!("probe-cwph-{}.db", r));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.to_str().unwrap()));
        let mut db = Database::open(&path).unwrap();
        db.execute("PRAGMA journal_mode = WAL", []).unwrap();
        db.execute("PRAGMA synchronous = NORMAL", []).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)", [])
            .unwrap();
        let db = Arc::new(db);

        let barrier = Arc::new(Barrier::new(n_writers));
        let stmt_done = Arc::new(Barrier::new(n_writers));
        let mut handles = Vec::new();
        let t0 = Instant::now();
        for tid in 0..n_writers as i64 {
            let db = Arc::clone(&db);
            let barrier = Arc::clone(&barrier);
            let stmt_done = Arc::clone(&stmt_done);
            handles.push(thread::spawn(move || {
                Database::set_conn_identity((tid + 1) as u64);
                db.begin_concurrent_transaction().unwrap();
                barrier.wait(); // all writers' txns open
                let mut stmt = db.prepare("INSERT INTO t (id, v) VALUES (?, ?)").unwrap();
                let (mut t_bind, mut t_step, mut t_reset) = (
                    std::time::Duration::ZERO,
                    std::time::Duration::ZERO,
                    std::time::Duration::ZERO,
                );
                for i in 0..per_writer {
                    let a = Instant::now();
                    stmt.bind(1, Value::Integer(tid * 1_000_000 + i)).unwrap();
                    stmt.bind(2, Value::Integer(tid)).unwrap();
                    let b = Instant::now();
                    stmt.step().unwrap();
                    let c = Instant::now();
                    stmt.reset();
                    let d = Instant::now();
                    t_bind += b - a;
                    t_step += c - b;
                    t_reset += d - c;
                }
                if std::env::var_os("PROBE_PHASES").is_some() {
                    eprintln!("  [t{tid}] bind {t_bind:?} step {t_step:?} reset {t_reset:?}");
                }
                drop(stmt);
                stmt_done.wait(); // statements complete on all writers
                let c = Instant::now();
                db.commit_concurrent_transaction().unwrap();
                (c, Instant::now())
            }));
        }
        let mut stmt_end = t0;
        let mut commit_nanos = 0u128;
        for h in handles {
            let (c, done) = h.join().unwrap();
            stmt_end = stmt_end.max(c);
            commit_nanos += (done - c).as_nanos();
        }
        let all = t0.elapsed();
        let stmt_phase = stmt_end - t0;
        total_stmt += stmt_phase;
        total_commit += all - stmt_phase;
        let n = db.query("SELECT count(*) FROM t", []).unwrap()[0][0].as_integer();
        assert_eq!(n, n_writers as i64 * per_writer);
        println!(
            "round {r}: total {all:?} | parallel-statements {stmt_phase:?} | commit-phase {:?} (sum of per-thread commit spans {:?})",
            all - stmt_phase,
            std::time::Duration::from_nanos(commit_nanos as u64)
        );
        drop(db);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.to_str().unwrap()));
    }
    println!(
        "\nSUM over {rounds} rounds ({n_writers} writers x {per_writer} stmts): statements {:?} | commit-phase {:?} | stmt share {:.0}%",
        total_stmt,
        total_commit,
        total_stmt.as_secs_f64()
            / (total_stmt + total_commit).as_secs_f64()
            * 100.0
    );
}
