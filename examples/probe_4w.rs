//! Diagnostic: the 4-parallel-writer shape with per-commit observability.
//!
//! Reproduces `parallel_shared_api::four_parallel_writers_commit_all`
//! (tests/concurrent_writes.rs) but prints the committed row count after
//! EVERY commit (ordered by the commit mutex) plus each writer's commit
//! outcome — pinpointing WHICH commit loses rows and whether the loser
//! took the fast-install or the row-merge path.
//!
//! Run: cargo run --release --example probe_4w

use std::sync::{Arc, Barrier};
use std::thread;

use rustqlite::{Database, Value};

fn dump_wal(tag: &str, path: &std::path::Path) {
    let walp = format!("{}-wal", path.to_str().unwrap());
    match std::fs::read(&walp) {
        Ok(b) => {
            let psz = 4096usize;
            let fsz = 24 + psz;
            let n = if b.len() > 32 {
                (b.len() - 32) / fsz
            } else {
                0
            };
            let mut frames = Vec::new();
            for i in 0..n {
                let o = 32 + i * fsz;
                let pid = u32::from_be_bytes(b[o..o + 4].try_into().unwrap());
                let commit = u32::from_be_bytes(b[o + 4..o + 8].try_into().unwrap());
                frames.push(format!(
                    "{}{}(p{})",
                    if commit != 0 { "*" } else { "" },
                    i,
                    pid
                ));
            }
            println!(
                "[{tag}] WAL {} bytes, {} frames: {}",
                b.len(),
                n,
                frames.join(" ")
            );
        }
        Err(_) => println!("[{tag}] no WAL file"),
    }
}

fn main() {
    let path = std::env::temp_dir().join("probe-4w.db");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.to_str().unwrap()));
    let mut db = Database::open(&path).unwrap();
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)", [])
        .unwrap();
    let db = Arc::new(db);

    const N_THREADS: usize = 4;
    const PER_THREAD: i64 = 250;

    // Commit-order observability: a global mutex serializes the
    // count+print right after each commit.
    let report = Arc::new(std::sync::Mutex::new(0usize));
    let barrier = Arc::new(Barrier::new(N_THREADS));
    let mut handles = Vec::new();
    for tid in 0..N_THREADS as i64 {
        let db = Arc::clone(&db);
        let barrier = Arc::clone(&barrier);
        let report = Arc::clone(&report);
        handles.push(thread::spawn(move || {
            Database::set_conn_identity((tid + 1) as u64);
            db.begin_concurrent_transaction().unwrap();
            barrier.wait();
            let mut stmt = db.prepare("INSERT INTO t (id, v) VALUES (?, ?)").unwrap();
            for i in 0..PER_THREAD {
                let id = tid * 100_000 + i;
                stmt.bind(1, Value::Integer(id)).unwrap();
                stmt.bind(2, Value::Integer(tid)).unwrap();
                stmt.step().unwrap();
                stmt.reset();
            }
            drop(stmt);
            let r = db.commit_concurrent_transaction();
            let seq = {
                let mut g = report.lock().unwrap();
                *g += 1;
                *g
            };
            match r {
                Ok(()) => {
                    let n = db.query("SELECT count(*) FROM t", []).unwrap()[0][0].as_integer();
                    println!(
                        "[commit #{seq}] writer {tid}: Ok    -> count now {n} (expect {})",
                        seq as i64 * PER_THREAD
                    );
                }
                Err(e) => println!("[commit #{seq}] writer {tid}: ERR {e}"),
            }
            Database::set_conn_identity(0);
        }));
    }
    for h in handles {
        h.join().expect("writer panicked");
    }

    // ── Dump the raw WAL BEFORE any close/checkpoint: do all four
    // commits' frames exist on disk, in order, with commit markers?
    dump_wal("after-all-commits", &path);

    let n = db.query("SELECT count(*) FROM t", []).unwrap()[0][0].as_integer();
    println!(
        "FINAL count: {n} (expect {})",
        N_THREADS as i64 * PER_THREAD
    );
    let ok = db.query("PRAGMA integrity_check", []).unwrap();
    println!("integrity: {}", ok[0][0].as_text());
    if let Ok(rows) = db.query("SELECT rootpage FROM sqlite_schema WHERE name = 't'", []) {
        println!(
            "LIVE rootpage of t: {:?}",
            rows.first().and_then(|r| r.first().cloned())
        );
    }
    // Per-writer visibility.
    for tid in 0..N_THREADS as i64 {
        let n = db
            .query("SELECT count(*) FROM t WHERE v = ?", [Value::Integer(tid)])
            .unwrap()[0][0]
            .as_integer();
        println!("  writer {tid} rows visible: {n} (expect {PER_THREAD})");
    }
    drop(db);
    let db2 = Database::open(&path).unwrap();
    let n = db2.query("SELECT count(*) FROM t", []).unwrap()[0][0].as_integer();
    println!("REOPEN count: {n}");
    if let Ok(rows) = db2.query("SELECT rootpage FROM sqlite_schema WHERE name = 't'", []) {
        println!(
            "REOPEN rootpage of t: {:?}",
            rows.first().and_then(|r| r.first().cloned())
        );
    }
    let ok = db2.query("PRAGMA integrity_check", []).unwrap();
    println!("REOPEN integrity: {}", ok[0][0].as_text());
    let pc = db2.query("PRAGMA page_count", []).unwrap()[0][0].as_integer();
    println!("REOPEN page_count: {pc}");
    for tid in 0..N_THREADS as i64 {
        let n = db2
            .query("SELECT count(*) FROM t WHERE v = ?", [Value::Integer(tid)])
            .unwrap()[0][0]
            .as_integer();
        println!("  REOPEN writer {tid} rows: {n} (expect {PER_THREAD})");
    }
    drop(db2);
    // Dump the raw WAL frame table (post-reopen state: the reopen may
    // have scrubbed torn tails — the dump shows what survived).
    let walp = format!("{}-wal", path.to_str().unwrap());
    if let Ok(b) = std::fs::read(&walp) {
        let psz = 4096usize;
        let fsz = 24 + psz;
        let n = if b.len() > 32 {
            (b.len() - 32) / fsz
        } else {
            0
        };
        println!(
            "WAL file: {} bytes, header says page_size={}",
            b.len(),
            u32::from_be_bytes(b.get(20..24).unwrap_or(&[0; 4]).try_into().unwrap())
        );
        let mut frames = Vec::new();
        for i in 0..n {
            let o = 32 + i * fsz;
            let pid = u32::from_be_bytes(b[o..o + 4].try_into().unwrap());
            let commit = u32::from_be_bytes(b[o + 4..o + 8].try_into().unwrap());
            frames.push(format!(
                "{}{}(p{})",
                if commit != 0 { "*" } else { "" },
                i,
                pid
            ));
        }
        println!("WAL frames (i, *commit, page): {}", frames.join(" "));
    } else {
        println!("(no WAL file at {walp})");
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&walp);
}
