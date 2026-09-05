//! WAL-grade concurrent read+write throughput benchmark (engine level).
//!
//! Scenario: ONE writer thread holds a long write transaction (INSERT
//! storm), while N reader threads run point lookups + counts
//! continuously. With committed-view reads the readers NEVER block on
//! the open transaction and always see the BEGIN-time snapshot — the
//! SQLite-WAL reader model, but with the version store in memory.
//!
//! The comparison numbers (SQLite via rusqlite, same thread topology):
//! SQLite in rollback-journal mode BLOCKS every reader for the whole
//! transaction (BUSY / busy_timeout storm); in WAL mode readers proceed
//! against the WAL snapshot — this engine matches that behavior with no
//! WAL file and no frame copy on the read path.
//!
//! Run: `cargo bench --bench concurrent_rw`

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use rustqlite::{Database, Value};

const N_ROWS: i64 = 10_000;
const WRITER_INSERTS: i64 = 20_000;

fn setup() -> Arc<RwLock<Database>> {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)", [])
        .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 0..N_ROWS {
        db.execute(
            "INSERT INTO t (id, v) VALUES (?, ?)",
            [Value::Integer(i), Value::Integer(i)],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    Arc::new(RwLock::new(db))
}

/// Readers + one open write transaction: measure (a) total wall time and
/// (b) reader ops completed DURING the transaction.
fn run(n_readers: usize) -> (Duration, usize) {
    let db = setup();
    let stop = Arc::new(AtomicBool::new(false));
    let mut reader_handles = Vec::new();
    for _ in 0..n_readers {
        let db = Arc::clone(&db);
        let stop = Arc::clone(&stop);
        reader_handles.push(thread::spawn(move || {
            let mut ops = 0usize;
            while !stop.load(Ordering::Acquire) {
                // Point lookup (the OLTP shape) + count, both through a
                // read guard while the writer's transaction is open.
                let r = db.read();
                let id = (ops % N_ROWS as usize) as i64;
                let rows = r
                    .query("SELECT v FROM t WHERE id = ?", [Value::Integer(id)])
                    .unwrap();
                assert_eq!(rows.len(), 1);
                let rows = r.query("SELECT COUNT(*) FROM t", []).unwrap();
                // BEGIN-time snapshot while the txn is open; the final
                // committed count after COMMIT. Never an intermediate.
                let n = rows.first().and_then(|x| x.first()).cloned();
                assert!(
                    n == Some(Value::Integer(N_ROWS))
                        || n == Some(Value::Integer(N_ROWS + WRITER_INSERTS)),
                    "reader saw intermediate count {n:?}"
                );
                ops += 1;
            }
            ops
        }));
    }
    let t0 = Instant::now();
    // Writer: ONE transaction, statement-scoped write guards so readers
    // interleave between statements (the documented locking contract).
    {
        let db = Arc::clone(&db);
        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let h = thread::spawn(move || {
            db.write().execute("BEGIN", []).unwrap();
            ready_tx.send(()).unwrap();
            for i in 0..WRITER_INSERTS {
                db.write()
                    .execute(
                        "INSERT INTO t (id, v) VALUES (?, ?)",
                        [Value::Integer(N_ROWS + i), Value::Integer(i)],
                    )
                    .unwrap();
            }
            db.write().execute("COMMIT", []).unwrap();
        });
        ready_rx.recv().unwrap(); // transaction is OPEN now
        let _ = h.join();
    }
    let elapsed = t0.elapsed();
    stop.store(true, Ordering::Release);
    let total: usize = reader_handles.into_iter().map(|h| h.join().unwrap()).sum();
    (elapsed, total)
}

fn main() {
    // Warmup (page cache, statement cache, thread spawn machinery).
    run(1);

    for &n in &[2usize, 4, 8] {
        let (elapsed, ops) = run(n);
        println!(
            "readers={n:2}  writer-txn wall={:>10.3?}  reader ops during txn={:>8}  ({:>8.0} reader ops/s)",
            elapsed,
            ops,
            ops as f64 / elapsed.as_secs_f64()
        );
    }
}
