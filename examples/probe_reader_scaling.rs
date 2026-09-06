//! Reader-scaling isolation: N threads x point-lookup `query` on ONE shared
//! `Arc<RwLock<Database>>` (in-memory store, warm cache). Compares aggregate
//! query throughput at 1 vs 2 vs 4 vs 8 readers — a flat aggregate curve
//! means a serialization point (page-cache lock word, driver lock, statement
//! cache), a linear curve means the 8-conn parity is elsewhere (sqlx/pool
//! overhead). Also measures the memoized committed-view path with an open
//! writer transaction (the f518de0 fast tier).
use std::sync::Arc;
use std::time::Instant;

fn main() {
    variant_b();
    let rows: i64 = 50_000;
    let per_reader: u64 = 20_000;
    let rounds = 3;

    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER NOT NULL, b REAL, c TEXT)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in (0..rows).step_by(500) {
        let mut batch = String::from("INSERT INTO t (a, b, c) SELECT ");
        for j in 0..500 {
            let v = i + j;
            if j > 0 {
                batch.push_str(" UNION ALL SELECT ");
            }
            batch.push_str(&format!("{v}, {v}.5, 'name-{v}'"));
        }
        db.execute(&batch, []).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    // Warm the page cache: one full scan touches every page once.
    let _ = db.query("SELECT COUNT(*) FROM t", []).unwrap();

    // Seed ONE shared engine; the probe is read-only until the last
    // scenario, and reusing it keeps every round's cache equally warm.
    let db = Arc::new(parking_lot::RwLock::new(db));
    for n in [1usize, 2, 4, 8] {
        let mut best = f64::MAX;
        for _ in 0..rounds {
            let t = Instant::now();
            let mut handles = Vec::new();
            for r in 0..n {
                let db = Arc::clone(&db);
                handles.push(std::thread::spawn(move || {
                    for i in 0..per_reader {
                        let key = 1 + ((i * 37 + r as u64 * 11) % (rows as u64 - 2));
                        let g = db.read();
                        let rs = g
                            .query(
                                "SELECT a, b, c FROM t WHERE id = ?",
                                [rustqlite::Value::Integer(key as i64)],
                            )
                            .unwrap();
                        assert_eq!(rs.len(), 1);
                        drop(g);
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
            best = best.min(t.elapsed().as_secs_f64() * 1e3);
        }
        let total = n as f64 * per_reader as f64;
        println!(
            "{n:2} readers: {best:8.1} ms  ({:8.0} q/s agg, {:6.2} us/q/thread)",
            total / (best / 1e3),
            best * 1e3 / total
        );
    }

    // Committed-view scaling with an OPEN writer txn (the f518de0 memo tier):
    // foreign-thread readers + idle-dirty writer, 8 readers.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let wdb = Arc::clone(&db);
    let dirty = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (d2, s2) = (Arc::clone(&dirty), Arc::clone(&stop));
    let w = std::thread::spawn(move || {
        {
            let mut g = wdb.write();
            g.execute("BEGIN", []).unwrap();
            g.execute("INSERT INTO t (a, b, c) VALUES (999, 0.5, 'w')", [])
                .unwrap();
        }
        d2.store(true, std::sync::atomic::Ordering::Release);
        while !s2.load(std::sync::atomic::Ordering::Acquire) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let mut g = wdb.write();
        let _ = g.execute("COMMIT", []);
    });
    while !dirty.load(std::sync::atomic::Ordering::Acquire) {}
    let n = 8usize;
    let t = Instant::now();
    let mut handles = Vec::new();
    for r in 0..n {
        let db = Arc::clone(&db);
        handles.push(std::thread::spawn(move || {
            for i in 0..per_reader {
                let key = 1 + ((i * 37 + r as u64 * 11) % (rows as u64 - 2));
                let g = db.read();
                let rs = g
                    .query(
                        "SELECT a, b, c FROM t WHERE id = ?",
                        [rustqlite::Value::Integer(key as i64)],
                    )
                    .unwrap();
                assert_eq!(rs.len(), 1);
                drop(g);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let ms = t.elapsed().as_secs_f64() * 1e3;
    stop.store(true, std::sync::atomic::Ordering::Release);
    w.join().unwrap();
    let total = n as f64 * per_reader as f64;
    println!(
        "8 readers + idle-dirty writer: {ms:8.1} ms  ({:8.0} q/s agg, {:6.2} us/q/thread)",
        total / (ms / 1e3),
        ms * 1e3 / total
    );
}

/// Variant B: Arc<Database> with NO engine-level RwLock — `query` takes
/// `&self`, so read-only workloads need no lock at all. Isolates the
/// page-cache lock contention from the engine-lock contention.
#[allow(dead_code)]
fn variant_b() {
    let rows: i64 = 50_000;
    let per_reader: u64 = 20_000;
    let rounds = 3;
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER NOT NULL, b REAL, c TEXT)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in (0..rows).step_by(500) {
        let mut batch = String::from("INSERT INTO t (a, b, c) SELECT ");
        for j in 0..500 {
            let v = i + j;
            if j > 0 {
                batch.push_str(" UNION ALL SELECT ");
            }
            batch.push_str(&format!("{v}, {v}.5, 'name-{v}'"));
        }
        db.execute(&batch, []).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let _ = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    let db = Arc::new(db);
    for n in [1usize, 2, 4, 8] {
        let mut best = f64::MAX;
        for _ in 0..rounds {
            let t = Instant::now();
            let mut handles = Vec::new();
            for r in 0..n {
                let db = Arc::clone(&db);
                handles.push(std::thread::spawn(move || {
                    for i in 0..per_reader {
                        let key = 1 + ((i * 37 + r as u64 * 11) % (rows as u64 - 2));
                        let rs = db
                            .query(
                                "SELECT a, b, c FROM t WHERE id = ?",
                                [rustqlite::Value::Integer(key as i64)],
                            )
                            .unwrap();
                        assert_eq!(rs.len(), 1);
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
            best = best.min(t.elapsed().as_secs_f64() * 1e3);
        }
        let total = n as f64 * per_reader as f64;
        println!(
            "[no-RwLock] {n:2} readers: {best:8.1} ms  ({:8.0} q/s agg, {:6.2} us/q/thread)",
            total / (best / 1e3),
            best * 1e3 / total
        );
    }
}
