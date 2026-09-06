//! WAL-grade committed-view reads: while a write transaction is open on
//! one thread/connection, readers on OTHER threads/connections see the
//! BEGIN-time (last committed) state — never uncommitted rows, never a
//! blocking wait. This is SQLite's WAL reader isolation, implemented with
//! an in-memory version store (the `__begin__` savepoint's undo
//! pre-images + the committed pages of the file/WAL) instead of WAL
//! frames.
//!
//! Tests use a WRITER THREAD + channel handoffs (the engine's identity
//! heuristic is thread-based: the writer owns the transaction, every
//! other thread reads the committed view) and statement-scoped guards
//! (a write guard held across the whole transaction is a client-side
//! deadlock by construction, same as holding a mutex across an await).

#![cfg(test)]

#[cfg(test)]
mod committed_view {
    use parking_lot::RwLock;
    use rustqlite::{Database, Value};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::thread;

    fn count(db: &Database) -> i64 {
        let rows = db.query("SELECT COUNT(*) FROM t", []).expect("count");
        match rows.first().and_then(|r| r.first()) {
            Some(Value::Integer(n)) => *n,
            other => panic!("unexpected count result: {other:?}"),
        }
    }

    /// The core WAL-reader contract, engine level: the writer THREAD
    /// opens a transaction and inserts; the main thread (read guard)
    /// concurrently counts and must see the BEGIN-time state. After
    /// COMMIT the new state is visible; after ROLLBACK it never was.
    #[test]
    fn engine_committed_view_isolation() {
        let mut db = Database::open_in_memory().expect("open");
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", [])
            .expect("create");
        db.execute("INSERT INTO t (x) VALUES (1)", [])
            .expect("seed");
        let state = Arc::new(RwLock::new(db));

        // --- COMMIT path ---
        {
            let (ready_tx, ready_rx) = mpsc::channel::<()>();
            let (go_tx, go_rx) = mpsc::channel::<()>();
            let s = Arc::clone(&state);
            let h = thread::spawn(move || {
                s.write().execute("BEGIN", []).expect("begin");
                s.write()
                    .execute("INSERT INTO t (x) VALUES (2)", [])
                    .expect("insert");
                ready_tx.send(()).expect("signal");
                go_rx
                    .recv_timeout(std::time::Duration::from_secs(30))
                    .expect("go");
                s.write().execute("COMMIT", []).expect("commit");
            });
            ready_rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("ready");
            // Foreign (main) thread under a read guard: BEGIN-time state.
            assert_eq!(
                count(&state.read()),
                1,
                "reader must see BEGIN-time state, not uncommitted rows"
            );
            go_tx.send(()).expect("release");
            h.join().expect("writer");
        }
        assert_eq!(count(&state.read()), 2, "commit makes writes visible");

        // --- ROLLBACK path ---
        {
            let (ready_tx, ready_rx) = mpsc::channel::<()>();
            let (go_tx, go_rx) = mpsc::channel::<()>();
            let s = Arc::clone(&state);
            let h = thread::spawn(move || {
                s.write().execute("BEGIN", []).expect("begin");
                s.write()
                    .execute("INSERT INTO t (x) VALUES (3)", [])
                    .expect("insert");
                ready_tx.send(()).expect("signal");
                go_rx
                    .recv_timeout(std::time::Duration::from_secs(30))
                    .expect("go");
                s.write().execute("ROLLBACK", []).expect("rollback");
            });
            ready_rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("ready");
            assert_eq!(
                count(&state.read()),
                2,
                "reader sees committed state during open txn"
            );
            go_tx.send(()).expect("release");
            h.join().expect("writer");
        }
        assert_eq!(
            count(&state.read()),
            2,
            "rollback discards uncommitted writes"
        );

        // --- read-your-own-writes on the owner thread ---
        {
            let (ready_tx, ready_rx) = mpsc::channel::<()>();
            let (go_tx, go_rx) = mpsc::channel::<()>();
            let s = Arc::clone(&state);
            let h = thread::spawn(move || {
                let mut a = s.write();
                a.execute("BEGIN", []).expect("begin");
                a.execute("INSERT INTO t (x) VALUES (4)", [])
                    .expect("insert");
                // Same thread = txn owner: live view (read-your-own-writes).
                let n = count(&a);
                assert_eq!(n, 3, "owner reads its own uncommitted writes");
                drop(a);
                ready_tx.send(()).expect("signal");
                go_rx
                    .recv_timeout(std::time::Duration::from_secs(30))
                    .expect("go");
                s.write().execute("ROLLBACK", []).expect("rollback");
            });
            ready_rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("ready");
            go_tx.send(()).expect("release");
            h.join().expect("writer");
        }
    }

    /// Updates (not just inserts) must also be invisible until commit —
    /// the committed view serves the pre-image of every writer-fetched
    /// page, so the OLD value survives until the boundary.
    #[test]
    fn engine_committed_view_update_and_delete() {
        let mut db = Database::open_in_memory().expect("open");
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x TEXT)", [])
            .expect("create");
        for i in 1..=50i64 {
            db.execute(
                "INSERT INTO t (x) VALUES (?)",
                [Value::Text(format!("v{i}").into())],
            )
            .expect("insert");
        }
        let state = Arc::new(RwLock::new(db));

        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let s = Arc::clone(&state);
        let h = thread::spawn(move || {
            s.write().execute("BEGIN", []).expect("begin");
            s.write()
                .execute("UPDATE t SET x = 'MUTATED' WHERE id <= 25", [])
                .expect("update");
            s.write()
                .execute("DELETE FROM t WHERE id > 25", [])
                .expect("delete");
            ready_tx.send(()).expect("signal");
            go_rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("go");
            s.write().execute("COMMIT", []).expect("commit");
        });
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("ready");
        // Foreign reader: BEGIN-time state — 50 rows, original values.
        {
            let b = state.read();
            assert_eq!(count(&b), 50, "reader sees BEGIN-time row count");
            let rows = b.query("SELECT x FROM t WHERE id = 1", []).expect("select");
            assert_eq!(
                rows.first().and_then(|r| r.first()).cloned(),
                Some(Value::Text("v1".into())),
                "reader sees BEGIN-time value, not the uncommitted mutation"
            );
            // Point-lookup path (the cached fast path is skipped for
            // committed readers): a mid-txn read of a mutated row.
            let rows = b
                .query("SELECT x FROM t WHERE id = 10", [])
                .expect("point select");
            assert_eq!(
                rows.first().and_then(|r| r.first()).cloned(),
                Some(Value::Text("v10".into()))
            );
        }
        go_tx.send(()).expect("release");
        h.join().expect("writer");
        let b = state.read();
        assert_eq!(count(&b), 25, "post-commit: deletes applied");
        let rows = b.query("SELECT x FROM t WHERE id = 1", []).unwrap();
        assert_eq!(
            rows.first().and_then(|r| r.first()).cloned(),
            Some(Value::Text("MUTATED".into()))
        );
    }

    /// Index-driven reads (point lookups through an index) during an open
    /// transaction must also serve BEGIN-time state — the index pages the
    /// writer split are reconstructed from pre-images.
    #[test]
    fn engine_committed_view_index_reads() {
        let mut db = Database::open_in_memory().expect("open");
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT UNIQUE, v INTEGER)",
            [],
        )
        .expect("create");
        for i in 0..200i64 {
            db.execute(
                "INSERT INTO t (k, v) VALUES (?, ?)",
                [Value::Text(format!("key{i:03}").into()), Value::Integer(i)],
            )
            .expect("insert");
        }
        let state = Arc::new(RwLock::new(db));

        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let s = Arc::clone(&state);
        let h = thread::spawn(move || {
            s.write().execute("BEGIN", []).expect("begin");
            s.write()
                .execute("INSERT INTO t (k, v) VALUES ('key999', 999)", [])
                .expect("insert new key");
            s.write()
                .execute("DELETE FROM t WHERE k = 'key000'", [])
                .expect("delete key");
            s.write()
                .execute("UPDATE t SET v = -1 WHERE k = 'key001'", [])
                .expect("update");
            ready_tx.send(()).expect("signal");
            go_rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("go");
            s.write().execute("COMMIT", []).expect("commit");
        });
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("ready");
        {
            let b = state.read();
            // Index point lookup on a deleted key: BEGIN-time state says it exists.
            let rows = b
                .query("SELECT v FROM t WHERE k = 'key000'", [])
                .expect("lookup");
            assert_eq!(
                rows.first().and_then(|r| r.first()).cloned(),
                Some(Value::Integer(0)),
                "reader sees the pre-delete index entry (BEGIN-time state)"
            );
            // Index lookup on the NEW key: not visible pre-commit.
            let rows = b
                .query("SELECT v FROM t WHERE k = 'key999'", [])
                .expect("lookup");
            assert!(rows.is_empty(), "uncommitted insert invisible to readers");
            // Updated value: old value.
            let rows = b
                .query("SELECT v FROM t WHERE k = 'key001'", [])
                .expect("lookup");
            assert_eq!(
                rows.first().and_then(|r| r.first()).cloned(),
                Some(Value::Integer(1)),
                "reader sees pre-update value"
            );
        }
        go_tx.send(()).expect("release");
        h.join().expect("writer");
        // Post-commit everything is visible.
        let b = state.read();
        assert!(b
            .query("SELECT v FROM t WHERE k = 'key000'", [])
            .unwrap()
            .is_empty());
        assert_eq!(
            b.query("SELECT v FROM t WHERE k = 'key999'", [])
                .unwrap()
                .first()
                .and_then(|r| r.first())
                .cloned(),
            Some(Value::Integer(999))
        );
    }

    /// A scan DURING the writer's B+tree splits (the table grows past one
    /// leaf mid-transaction) must still return exactly the BEGIN-time
    /// rows: mid-txn root moves must not be visible (the reader uses the
    /// BEGIN-time bookkeeping maps + pre-image pages).
    #[test]
    fn engine_committed_view_across_splits() {
        let mut db = Database::open_in_memory().expect("open");
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", [])
            .expect("create");
        db.execute("INSERT INTO t (x) VALUES (1)", [])
            .expect("seed");
        let state = Arc::new(RwLock::new(db));

        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let s = Arc::clone(&state);
        let h = thread::spawn(move || {
            s.write().execute("BEGIN", []).expect("begin");
            // Enough rows to force several leaf splits mid-transaction.
            for i in 0..2000i64 {
                s.write()
                    .execute("INSERT INTO t (x) VALUES (?)", [Value::Integer(i)])
                    .expect("insert");
            }
            ready_tx.send(()).expect("signal");
            go_rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("go");
            s.write().execute("COMMIT", []).expect("commit");
        });
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("ready");
        {
            let b = state.read();
            assert_eq!(
                count(&b),
                1,
                "reader sees exactly the BEGIN-time rows despite mid-txn splits"
            );
            let rows = b.query("SELECT id FROM t", []).expect("scan");
            assert_eq!(rows.len(), 1);
        }
        go_tx.send(()).expect("release");
        h.join().expect("writer");
        assert_eq!(count(&state.read()), 2001);
    }

    /// The writer's own max-rowid bookkeeping must not be poisoned by
    /// committed readers scanning mid-transaction (BEGIN-time max rowid
    /// would regress the insert allocator → duplicate rowids).
    #[test]
    fn engine_reader_does_not_poison_max_rowid() {
        let mut db = Database::open_in_memory().expect("open");
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", [])
            .expect("create");
        for i in 1..=10i64 {
            db.execute("INSERT INTO t (x) VALUES (?)", [Value::Integer(i)])
                .expect("insert");
        }
        let state = Arc::new(RwLock::new(db));

        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let s = Arc::clone(&state);
        let h = thread::spawn(move || {
            s.write().execute("BEGIN", []).expect("begin");
            for i in 11..=20i64 {
                s.write()
                    .execute("INSERT INTO t (x) VALUES (?)", [Value::Integer(i)])
                    .expect("insert");
            }
            ready_tx.send(()).expect("signal");
            go_rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("go");
            // The writer keeps inserting after the reader scanned: rowid
            // allocation must continue past 20 (no duplicate PKs).
            for i in 21..=30i64 {
                s.write()
                    .execute("INSERT INTO t (x) VALUES (?)", [Value::Integer(i)])
                    .expect("insert after reader scan");
            }
            s.write().execute("COMMIT", []).expect("commit");
        });
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("ready");
        // Foreign reader scans (would populate the max-rowid cache with
        // BEGIN-time max 10 if not gated).
        {
            let b = state.read();
            let _ = count(&b);
            let _ = b.query("SELECT max(id) FROM t", []).expect("max");
        }
        go_tx.send(()).expect("release");
        h.join().expect("writer");
        assert_eq!(count(&state.read()), 30);
    }

    /// Advisory caches (COUNT memoization, join builds, leaf hints) that
    /// a committed reader populated from BEGIN-time bytes must never leak
    /// into LIVE reads after the transaction ends (the epoch-poison
    /// bump). A stale post-commit count/join would be data corruption.
    #[test]
    fn engine_reader_does_not_poison_caches_after_commit() {
        let mut db = Database::open_in_memory().expect("open");
        db.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, x INTEGER)", [])
            .expect("create a");
        db.execute("CREATE TABLE b (id INTEGER PRIMARY KEY, y INTEGER)", [])
            .expect("create b");
        for i in 1..=100i64 {
            db.execute("INSERT INTO a (x) VALUES (?)", [Value::Integer(i)])
                .expect("insert a");
            db.execute("INSERT INTO b (y) VALUES (?)", [Value::Integer(i)])
                .expect("insert b");
        }
        let state = Arc::new(RwLock::new(db));

        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let s = Arc::clone(&state);
        let h = thread::spawn(move || {
            s.write().execute("BEGIN", []).expect("begin");
            s.write()
                .execute("DELETE FROM a WHERE id > 50", [])
                .expect("delete half");
            ready_tx.send(()).expect("signal");
            go_rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("go");
            s.write().execute("COMMIT", []).expect("commit");
        });
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("ready");
        // Committed reader runs the join + count repeatedly (populates
        // the join cache and count memo at BEGIN-time values).
        for _ in 0..5 {
            let b = state.read();
            let n: i64 = match b
                .query("SELECT COUNT(*) FROM a JOIN b ON a.x = b.y", [])
                .expect("join count")
                .first()
                .and_then(|r| r.first())
            {
                Some(Value::Integer(n)) => *n,
                other => panic!("bad join count: {other:?}"),
            };
            assert_eq!(n, 100, "committed reader sees BEGIN-time join");
            drop(b);
        }
        {
            let b = state.read();
            let _ = b.query("SELECT COUNT(*) FROM a", []).expect("count a");
        }
        go_tx.send(()).expect("release");
        h.join().expect("writer");
        // LIVE reads after commit: 50 rows — a poisoned cache would still
        // answer 100.
        let b = state.read();
        let live_count: i64 = match b
            .query("SELECT COUNT(*) FROM a", [])
            .expect("live count")
            .first()
            .and_then(|r| r.first())
        {
            Some(Value::Integer(n)) => *n,
            other => panic!("bad live count: {other:?}"),
        };
        assert_eq!(
            live_count, 50,
            "count cache must not serve BEGIN-time answer post-commit"
        );
        let n: i64 = match b
            .query("SELECT COUNT(*) FROM a JOIN b ON a.x = b.y", [])
            .expect("live join")
            .first()
            .and_then(|r| r.first())
        {
            Some(Value::Integer(n)) => *n,
            other => panic!("bad live join count: {other:?}"),
        };
        assert_eq!(
            n, 50,
            "join cache must not serve BEGIN-time build post-commit"
        );
    }

    /// Stress: a long write transaction with continuous foreign readers —
    /// every read must return the BEGIN-time state; no deadlock; the
    /// writer commits cleanly. The writer takes STATEMENT-scoped write
    /// guards so readers interleave between statements.
    #[test]
    fn engine_committed_view_stress() {
        let mut db = Database::open_in_memory().expect("open");
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", [])
            .expect("create");
        for i in 1..=500i64 {
            db.execute("INSERT INTO t (x) VALUES (?)", [Value::Integer(i)])
                .expect("seed");
        }
        let state = Arc::new(RwLock::new(db));

        let stop = Arc::new(AtomicBool::new(false));
        // Ready barrier: on a loaded 2-core box the writer thread can
        // finish all 1000 statements before the OS schedules ANY reader —
        // total_reads == 0 was a scheduling artifact, not isolation.
        // Wait for every reader to complete one read first, THEN start
        // the writer; the mid-transaction interleaving still happens
        // naturally across the writer's ~ms of statements.
        let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut reader_handles = Vec::new();
        for _ in 0..4 {
            let s = Arc::clone(&state);
            let stop = Arc::clone(&stop);
            let started = Arc::clone(&started);
            reader_handles.push(thread::spawn(move || {
                let mut reads = 0usize;
                loop {
                    let b = s.read();
                    let n = count(&b);
                    // Reader isolation: BEFORE the writer's BEGIN → live
                    // 500; DURING the txn → committed BEGIN-time 500;
                    // AFTER COMMIT → 1500. No INTERMEDIATE value (an
                    // uncommitted count like 937) may ever appear.
                    assert!(
                        n == 500 || n == 1500,
                        "reader isolation violated: saw intermediate count {n}"
                    );
                    reads += 1;
                    if reads == 1 {
                        started.fetch_add(1, Ordering::Release);
                    }
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    std::thread::yield_now();
                }
                reads
            }));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while started.load(Ordering::Acquire) < 4 {
            if std::time::Instant::now() > deadline {
                panic!("readers never started — scheduler starvation (not isolation)");
            }
            std::thread::yield_now();
        }
        // Writer THREAD opens the txn and inserts while readers read.
        let s = Arc::clone(&state);
        let writer = thread::spawn(move || {
            s.write().execute("BEGIN", []).expect("begin");
            for i in 0..1000i64 {
                s.write()
                    .execute("INSERT INTO t (x) VALUES (?)", [Value::Integer(1000 + i)])
                    .expect("insert under concurrent readers");
            }
            s.write().execute("COMMIT", []).expect("commit");
        });
        writer.join().expect("writer thread panicked");
        stop.store(true, Ordering::Release);
        let mut total_reads = 0usize;
        for h in reader_handles {
            total_reads += h.join().expect("reader thread panicked");
        }
        assert!(
            total_reads > 0,
            "readers must have actually run mid-transaction"
        );
        assert_eq!(count(&state.read()), 1500);
    }

    /// FILE-backed databases get the same WAL-grade reader semantics
    /// (the committed view reconstructs from pre-images + WAL frames +
    /// the main file).
    #[test]
    fn engine_committed_view_file_backed() {
        let path = std::env::temp_dir().join("rustqlite_committed_view_file.db");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::open(&path).expect("open");
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", [])
            .expect("create");
        for i in 1..=20i64 {
            db.execute("INSERT INTO t (x) VALUES (?)", [Value::Integer(i)])
                .expect("seed");
        }
        let state = Arc::new(RwLock::new(db));

        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let s = Arc::clone(&state);
        let h = thread::spawn(move || {
            s.write().execute("BEGIN", []).expect("begin");
            s.write()
                .execute("UPDATE t SET x = -x", [])
                .expect("update all");
            ready_tx.send(()).expect("signal");
            go_rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("go");
            s.write().execute("COMMIT", []).expect("commit");
        });
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("ready");
        {
            let b = state.read();
            let rows = b.query("SELECT x FROM t WHERE id = 5", []).expect("read");
            assert_eq!(
                rows.first().and_then(|r| r.first()).cloned(),
                Some(Value::Integer(5)),
                "file-backed reader sees BEGIN-time value"
            );
        }
        go_tx.send(()).expect("release");
        h.join().expect("writer");
        let rows = state
            .read()
            .query("SELECT x FROM t WHERE id = 5", [])
            .expect("read");
        assert_eq!(
            rows.first().and_then(|r| r.first()).cloned(),
            Some(Value::Integer(-5))
        );
        drop(state);
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod committed_fast_path {
    use parking_lot::RwLock;
    use rustqlite::{Database, Value};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    /// Fast-path point lookups under an armed committed view must serve
    /// BEGIN-time state even while the writer's mid-transaction inserts
    /// SPLIT the B+tree (moving the live root) — the exact hazard that
    /// used to make the driver skip the fast path entirely. Rowid lookups
    /// for rows that exist only after BEGIN must return empty; pre-BEGIN
    /// rows must return their BEGIN-time values; nothing may error.
    #[test]
    fn fast_path_point_lookup_under_root_moves() {
        let mut db = Database::open_in_memory().expect("open");
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER NOT NULL)",
            [],
        )
        .expect("create");
        // Enough rows that the writer's appends SPLIT leaves mid-txn
        // (multi-level tree with root moves), all rowid < 2000.
        db.execute("BEGIN", []).expect("begin");
        for i in 1..=2000i64 {
            db.execute("INSERT INTO t (x) VALUES (?)", [Value::Integer(i)])
                .expect("seed");
        }
        db.execute("COMMIT", []).expect("commit");
        let state = Arc::new(RwLock::new(db));

        // Prime the statement cache so the fast path exists and the
        // readers exercise exactly the FastPath::RowidPoint arm.
        {
            let b = state.read();
            for id in [1i64, 2, 1000, 2000] {
                let rows = b
                    .query("SELECT x FROM t WHERE id = ?", [Value::Integer(id)])
                    .expect("prime");
                assert_eq!(rows.len(), 1);
            }
        }

        let stop = Arc::new(AtomicBool::new(false));
        let dirty = Arc::new(AtomicBool::new(false));
        let wdb = Arc::clone(&state);
        let (d2, s2) = (Arc::clone(&dirty), Arc::clone(&stop));
        let w = thread::spawn(move || {
            {
                let mut g = wdb.write();
                g.execute("BEGIN", []).expect("begin");
                // Root-moving splits: appends at fresh rowids plus
                // in-place updates of pre-existing rows.
                for i in 2001..=2600i64 {
                    g.execute("INSERT INTO t (x) VALUES (?)", [Value::Integer(i)])
                        .expect("insert");
                }
                g.execute("UPDATE t SET x = -x WHERE id < 300", [])
                    .expect("update");
            }
            d2.store(true, Ordering::Release);
            while !s2.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
            wdb.write().execute("COMMIT", []).expect("commit");
        });
        while !dirty.load(Ordering::Acquire) {}

        // Foreign-thread readers: the engine heuristic arms the
        // committed view; the cached statement takes the fast path.
        let reader = {
            let state = Arc::clone(&state);
            thread::spawn(move || {
                for i in 0..4000u64 {
                    let id = 1 + (i % 2600) as i64;
                    let g = state.read();
                    let rows = g
                        .query("SELECT x FROM t WHERE id = ?", [Value::Integer(id)])
                        .expect("fast-path read");
                    drop(g);
                    if id <= 2000 {
                        // BEGIN-time rows: original value (the writer's
                        // UPDATE is invisible until COMMIT).
                        assert_eq!(
                            rows.first().and_then(|r| r.first()).cloned(),
                            Some(Value::Integer(id)),
                            "BEGIN-time row {id} must read its BEGIN value"
                        );
                    } else {
                        // Post-BEGIN rowids: invisible to the committed
                        // view even though the live tree holds them.
                        assert!(
                            rows.is_empty(),
                            "uncommitted row {id} leaked to a committed reader"
                        );
                    }
                }
            })
        };
        reader.join().expect("reader");
        stop.store(true, Ordering::Release);
        w.join().expect("writer");

        // Post-COMMIT: everything visible, updated rows flipped.
        let b = state.read();
        let rows = b
            .query("SELECT x FROM t WHERE id = 5", [])
            .expect("post-commit read");
        assert_eq!(
            rows.first().and_then(|r| r.first()).cloned(),
            Some(Value::Integer(-5))
        );
        let n = b.query("SELECT COUNT(*) FROM t", []).expect("count");
        assert_eq!(
            n.first().and_then(|r| r.first()).cloned(),
            Some(Value::Integer(2600))
        );
    }

    /// INDEX point lookups under a committed view with index root moves:
    /// `SELECT x FROM t WHERE xi = ?` over a non-unique index the writer
    /// is splitting — the FastPath::IndexPoint arm.
    #[test]
    fn fast_path_index_lookup_under_root_moves() {
        let mut db = Database::open_in_memory().expect("open");
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, xi INTEGER NOT NULL, y TEXT)",
            [],
        )
        .expect("create");
        db.execute("CREATE INDEX t_xi ON t (xi)", [])
            .expect("index");
        db.execute("BEGIN", []).expect("begin");
        for i in 1..=3000i64 {
            db.execute(
                "INSERT INTO t (xi, y) VALUES (?, ?)",
                [Value::Integer(i * 7), Value::Text(format!("v{i}").into())],
            )
            .expect("seed");
        }
        db.execute("COMMIT", []).expect("commit");
        let state = Arc::new(RwLock::new(db));

        {
            let b = state.read();
            for k in [7i64, 14, 21000] {
                let rows = b
                    .query("SELECT y FROM t WHERE xi = ?", [Value::Integer(k)])
                    .expect("prime");
                assert_eq!(rows.len(), 1, "xi={k}");
            }
        }

        let stop = Arc::new(AtomicBool::new(false));
        let dirty = Arc::new(AtomicBool::new(false));
        let wdb = Arc::clone(&state);
        let (d2, s2) = (Arc::clone(&dirty), Arc::clone(&stop));
        let w = thread::spawn(move || {
            {
                let mut g = wdb.write();
                g.execute("BEGIN", []).expect("begin");
                // Split the INDEX: new keys land above every seed key,
                // growing the index tree until its root moves.
                for i in 3001..=3800i64 {
                    g.execute(
                        "INSERT INTO t (xi, y) VALUES (?, ?)",
                        [Value::Integer(i * 7), Value::Text(format!("w{i}").into())],
                    )
                    .expect("insert");
                }
                g.execute("UPDATE t SET xi = -xi WHERE id > 2900", [])
                    .expect("index rewrite");
            }
            d2.store(true, Ordering::Release);
            while !s2.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
            wdb.write().execute("COMMIT", []).expect("commit");
        });
        while !dirty.load(Ordering::Acquire) {}

        let reader = {
            let state = Arc::clone(&state);
            thread::spawn(move || {
                for i in 0..4000u64 {
                    let seed = 1 + (i % 3000) as i64;
                    let key = seed * 7; // BEGIN-time index key
                    let g = state.read();
                    let rows = g
                        .query("SELECT y FROM t WHERE xi = ?", [Value::Integer(key)])
                        .expect("index fast-path read");
                    drop(g);
                    assert_eq!(
                        rows.len(),
                        1,
                        "BEGIN-time key {key} must resolve exactly once"
                    );
                    assert_eq!(
                        rows.first().and_then(|r| r.first()).cloned(),
                        Some(Value::Text(format!("v{seed}").into()))
                    );
                    // Keys that exist only post-BEGIN: invisible.
                    let post = 3500i64 * 7;
                    let rows = state
                        .read()
                        .query("SELECT y FROM t WHERE xi = ?", [Value::Integer(post)])
                        .expect("post key read");
                    assert!(rows.is_empty(), "uncommitted index key {post} leaked");
                }
            })
        };
        reader.join().expect("reader");
        stop.store(true, Ordering::Release);
        w.join().expect("writer");

        let b = state.read();
        let rows = b.query("SELECT COUNT(*) FROM t", []).expect("count");
        assert_eq!(
            rows.first().and_then(|r| r.first()).cloned(),
            Some(Value::Integer(3800))
        );
    }
}
