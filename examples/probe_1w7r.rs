//! Isolate 1W+7R contention costs: reader throughput (a) uncontended,
//! (b) with an open+dirty (but idle) writer txn, (c) with an active writer.
use std::sync::Arc;
use std::time::Instant;

fn main() {
    let n_readers = 7;
    let per_reader = 400;

    // (a) uncontended baseline
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE bench (id INTEGER PRIMARY KEY, a INTEGER NOT NULL, b REAL NOT NULL, c TEXT NOT NULL)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 0..5000i64 {
        db.execute(
            "INSERT INTO bench (a, b, c) VALUES (?, ?, ?)",
            [
                rustqlite::Value::Integer(i),
                rustqlite::Value::Real(i as f64 * 0.5),
                rustqlite::Value::Text(format!("name-{i}").into()),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let db = Arc::new(parking_lot::RwLock::new(db));

    let run_readers = |label: &str| {
        let t = Instant::now();
        let mut handles = Vec::new();
        for r in 0..n_readers {
            let db = Arc::clone(&db);
            handles.push(std::thread::spawn(move || {
                let mut hits = 0usize;
                for i in 0..per_reader {
                    let key = ((i * 37 + r * 11) % 4000) as i64;
                    let guard = db.read();
                    let rows = guard
                        .query(
                            "SELECT a FROM bench WHERE a BETWEEN ? AND ? LIMIT 1",
                            [
                                rustqlite::Value::Integer(key),
                                rustqlite::Value::Integer(key + 100),
                            ],
                        )
                        .unwrap();
                    if !rows.is_empty() {
                        hits += 1;
                    }
                    drop(guard);
                }
                hits
            }));
        }
        let hits: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
        let ms = t.elapsed().as_secs_f64() * 1e3;
        println!("{label:34} {ms:8.1} ms  ({hits} hits)");
        ms
    };

    let a = run_readers("(a) uncontended readers");

    // (b) open+dirty but idle writer transaction held by another thread
    let writer_db = Arc::clone(&db);
    let dirty = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let wdirty = Arc::clone(&dirty);
    let wstop = Arc::clone(&stop);
    let w = std::thread::spawn(move || {
        // BEGIN + one insert to make it dirty, then park until told to stop.
        {
            let mut g = writer_db.write();
            g.execute("BEGIN", []).unwrap();
            g.execute(
                "INSERT INTO bench (a, b, c) VALUES (?, ?, ?)",
                [
                    rustqlite::Value::Integer(900_000),
                    rustqlite::Value::Real(0.5),
                    rustqlite::Value::Text("bw".into()),
                ],
            )
            .unwrap();
        }
        wdirty.store(true, std::sync::atomic::Ordering::Release);
        while !wstop.load(std::sync::atomic::Ordering::Acquire) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let mut g = writer_db.write();
        let _ = g.execute("COMMIT", []);
    });
    while !dirty.load(std::sync::atomic::Ordering::Acquire) {}
    // NOTE: readers here do NOT use the committed-view escape (that lives in
    // the sqlx driver); they run plain `query`, which serves LIVE pages —
    // this isolates raw lock interference, not isolation semantics.
    let b = run_readers("(b) idle-dirty writer (live reads)");
    stop.store(true, std::sync::atomic::Ordering::Release);
    w.join().unwrap();

    // (c) active writer: exactly the bench shape — 40 txns x 10 inserts
    let writer_db = Arc::clone(&db);
    let w = std::thread::spawn(move || {
        let mut next = 500_000i64;
        for _ in 0..40 {
            {
                let mut g = writer_db.write();
                g.execute("BEGIN", []).unwrap();
                for _ in 0..10 {
                    next += 1;
                    g.execute(
                        "INSERT INTO bench (a, b, c) VALUES (?, ?, ?)",
                        [
                            rustqlite::Value::Integer(next),
                            rustqlite::Value::Real(0.5),
                            rustqlite::Value::Text("bw".into()),
                        ],
                    )
                    .unwrap();
                }
                let _ = g.execute("COMMIT", []);
            }
        }
    });
    let c = run_readers("(c) active writer + readers");
    w.join().unwrap();

    // (d) lock-churn only: same write-lock frequency, NO insert work
    let writer_db = Arc::clone(&db);
    let stop3 = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let wstop3 = Arc::clone(&stop3);
    let w = std::thread::spawn(move || {
        let mut n = 0;
        while !wstop3.load(std::sync::atomic::Ordering::Acquire) {
            let _g = writer_db.write();
            std::hint::black_box(&n);
            n += 1;
        }
        n
    });
    let d = run_readers("(d) write-lock churn only");
    stop3.store(true, std::sync::atomic::Ordering::Release);
    let churn = w.join().unwrap();
    println!("(d) writer acquisitions: {churn}");

    println!("---");
    println!("uncontended: {a:.1}ms | idle-dirty: {b:.1}ms ({:.1}x) | active-writer: {c:.1}ms ({:.1}x) | churn-only: {d:.1}ms ({:.1}x)", b / a, c / a, d / a);
}
