//! Isolation-anomaly probes for the multi-writer regime.
//!
//! These pin the guarantees a production OLTP application relies on when it
//! runs read-modify-write transactions through `BEGIN CONCURRENT` (or the
//! implicit-join autocommit path):
//!
//! * no LOST UPDATE — two transactions that each read a row and write a
//!   value derived from it can never both commit;
//! * no WRITE SKEW — SQLite's begin-concurrent contract validates the
//!   transaction's READ set as well as its write set ("if the transaction
//!   does read or write a page modified by a concurrent transaction, it ...
//!   fails with SQLITE_BUSY_SNAPSHOT"), so a transaction whose reads were
//!   invalidated by a sibling commit must not commit;
//! * conservation under a randomized multi-threaded transfer storm, with
//!   readers asserting the invariant on every snapshot they see.

use rustqlite::{Database, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

fn waldb(name: &str) -> (Database, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!("iso_anom_{}_{}.db", name, std::process::id()));
    cleanup(&path);
    let mut db = Database::open(&path).unwrap();
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    (db, path)
}

fn cleanup(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let p = path.to_str().unwrap();
    for suffix in ["-wal", "-shm", "-journal", "-rsqllock"] {
        let _ = std::fs::remove_file(format!("{p}{suffix}"));
    }
}

fn int(db: &Database, sql: &str) -> i64 {
    db.query(sql, []).unwrap()[0][0].as_integer()
}

fn is_busy_snapshot(e: &rustqlite::Error) -> bool {
    let s = e.to_string();
    s.contains("BUSY") || s.contains("517") || s.contains("conflict")
}

/// Classic write skew (the on-call doctors shape): both transactions read
/// BOTH rows, check an invariant over them, and each writes a DIFFERENT
/// row. Snapshot isolation commits both and breaks the invariant; SQLite's
/// begin-concurrent read-set validation rejects the second committer.
#[test]
fn write_skew_is_rejected() {
    let (mut db, path) = waldb("skew");
    db.execute(
        "CREATE TABLE oncall (id INTEGER PRIMARY KEY, on_duty INTEGER)",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO oncall VALUES (1, 1), (2, 1)", [])
        .unwrap();

    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    let c1 = int(&db, "SELECT count(*) FROM oncall WHERE on_duty = 1");
    Database::set_conn_identity(2);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    let c2 = int(&db, "SELECT count(*) FROM oncall WHERE on_duty = 1");
    assert_eq!((c1, c2), (2, 2));

    // Each "doctor" goes off duty because the OTHER one is still on.
    Database::set_conn_identity(1);
    db.execute("UPDATE oncall SET on_duty = 0 WHERE id = 1", [])
        .unwrap();
    Database::set_conn_identity(2);
    db.execute("UPDATE oncall SET on_duty = 0 WHERE id = 2", [])
        .unwrap();

    Database::set_conn_identity(1);
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(2);
    let second = db.execute("COMMIT", []);
    if second.is_err() {
        let _ = db.execute("ROLLBACK", []);
    }
    Database::set_conn_identity(0);
    let on = int(&db, "SELECT count(*) FROM oncall WHERE on_duty = 1");
    drop(db);
    cleanup(&path);
    assert!(
        second.is_err(),
        "write skew: the second committer's read set was invalidated by the first commit, \
         yet it committed (on-duty count now {on}, invariant was >= 1)"
    );
    assert_eq!(on, 1);
}

/// Lost update through an application-side read-modify-write: both read
/// the balance, both write `balance - x` computed in the application.
#[test]
fn lost_update_is_rejected() {
    let (mut db, path) = waldb("lost");
    db.execute("CREATE TABLE acc (id INTEGER PRIMARY KEY, bal INTEGER)", [])
        .unwrap();
    db.execute("INSERT INTO acc VALUES (1, 100)", []).unwrap();

    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    let b1 = int(&db, "SELECT bal FROM acc WHERE id = 1");
    Database::set_conn_identity(2);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    let b2 = int(&db, "SELECT bal FROM acc WHERE id = 1");

    Database::set_conn_identity(1);
    db.execute(
        "UPDATE acc SET bal = ? WHERE id = 1",
        [Value::Integer(b1 - 10)],
    )
    .unwrap();
    Database::set_conn_identity(2);
    db.execute(
        "UPDATE acc SET bal = ? WHERE id = 1",
        [Value::Integer(b2 - 20)],
    )
    .unwrap();

    Database::set_conn_identity(1);
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(2);
    let second = db.execute("COMMIT", []);
    if second.is_err() {
        let _ = db.execute("ROLLBACK", []);
    }
    Database::set_conn_identity(0);
    let bal = int(&db, "SELECT bal FROM acc WHERE id = 1");
    drop(db);
    cleanup(&path);
    assert!(
        second.is_err(),
        "lost update: both read-modify-write transactions committed (bal={bal})"
    );
    assert_eq!(bal, 90);
}

/// A transaction that reads a row a sibling then changes and commits, and
/// writes something derived from that read into a DIFFERENT row (a
/// "read-dependency" anomaly — e.g. copying a balance into a ledger).
#[test]
fn stale_read_dependency_is_rejected() {
    let (mut db, path) = waldb("readdep");
    db.execute("CREATE TABLE kv (k INTEGER PRIMARY KEY, v INTEGER)", [])
        .unwrap();
    db.execute("INSERT INTO kv VALUES (1, 10), (2, 0)", [])
        .unwrap();

    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    // T1 copies k=1 into k=2 (reads k=1).
    db.execute(
        "UPDATE kv SET v = (SELECT v FROM kv WHERE k = 1) WHERE k = 2",
        [],
    )
    .unwrap();
    Database::set_conn_identity(2);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("UPDATE kv SET v = 99 WHERE k = 1", []).unwrap();
    db.execute("COMMIT", []).unwrap();

    Database::set_conn_identity(1);
    let r = db.execute("COMMIT", []);
    if r.is_err() {
        let _ = db.execute("ROLLBACK", []);
    }
    Database::set_conn_identity(0);
    let rows = db.query("SELECT v FROM kv ORDER BY k", []).unwrap();
    let v: Vec<i64> = rows.iter().map(|r| r[0].as_integer()).collect();
    drop(db);
    cleanup(&path);
    // Serializable outcomes: T2 then T1 → [99, 99]; T1 then T2 → [99, 10].
    // T1 committing after T2 with the stale read is [99, 10] — the order
    // T1-before-T2, which is a valid serial order ONLY if T1 committed
    // first. Here T2 committed first, so T1 must have failed (SQLite) or
    // the result must equal the T2-then-T1 serial order.
    assert!(
        r.is_err() || v == vec![99, 99],
        "T1 committed after T2 with a stale read of k=1: result {v:?} matches no serial order \
         consistent with the commit order"
    );
}

/// Read skew: inside ONE transaction, two reads of rows living on
/// DIFFERENT pages must come from the same snapshot, even when a sibling
/// transaction commits a change to both rows between the two reads.
#[test]
fn read_skew_inside_concurrent_txn() {
    let (mut db, path) = waldb("readskew");
    db.execute(
        "CREATE TABLE acc (id INTEGER PRIMARY KEY, bal INTEGER, pad TEXT)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=60 {
        db.execute(
            "INSERT INTO acc VALUES (?, 100, ?)",
            [Value::Integer(i), Value::Text("x".repeat(1200).into())],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    let a = int(&db, "SELECT bal FROM acc WHERE id = 1");

    // Sibling transfers 10 from id=1 to id=60 and commits.
    Database::set_conn_identity(2);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("UPDATE acc SET bal = bal - 10 WHERE id = 1", [])
        .unwrap();
    db.execute("UPDATE acc SET bal = bal + 10 WHERE id = 60", [])
        .unwrap();
    db.execute("COMMIT", []).unwrap();

    // T1 reads the far row for the first time AFTER the sibling commit.
    Database::set_conn_identity(1);
    let b = db.query("SELECT bal FROM acc WHERE id = 60", []);
    let res = match b {
        Ok(rows) => {
            let b = rows[0][0].as_integer();
            let _ = db.execute("COMMIT", []);
            Some(b)
        }
        // Failing the read (snapshot no longer servable) is an acceptable,
        // SQLite-compatible answer; serving a mixed snapshot is not.
        Err(_) => {
            let _ = db.execute("ROLLBACK", []);
            None
        }
    };
    Database::set_conn_identity(0);
    drop(db);
    cleanup(&path);
    if let Some(b) = res {
        assert_eq!(
            a + b,
            200,
            "read skew: one transaction observed id=1 at {a} (pre-transfer) and id=60 at {b} (post-transfer)"
        );
    }
}

/// Read-your-own-writes inside a concurrent transaction, after the same
/// thread warmed the point-lookup hint caches OUTSIDE the transaction
/// (hints hold PageRefs; a live-page hint must never serve a read that
/// belongs to the transaction's private shadow).
#[test]
fn own_writes_visible_after_prewarmed_hints() {
    let (mut db, path) = waldb("ownwrites");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=50 {
        db.execute("INSERT INTO t VALUES (?, 'old')", [Value::Integer(i)])
            .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(1);
    // Warm every hint path on this thread with plain reads.
    for _ in 0..3 {
        let r = db.query("SELECT v FROM t WHERE id = 5", []).unwrap();
        assert_eq!(r[0][0].as_text(), "old");
    }
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("UPDATE t SET v = 'new' WHERE id = 5", [])
        .unwrap();
    let inside = db.query("SELECT v FROM t WHERE id = 5", []).unwrap()[0][0]
        .as_text()
        .to_string();
    let inside_scan = db
        .query("SELECT v FROM t WHERE v = 'new'", [])
        .unwrap()
        .len();
    // A sibling reads the committed state meanwhile.
    Database::set_conn_identity(2);
    let sibling = db.query("SELECT v FROM t WHERE id = 5", []).unwrap()[0][0]
        .as_text()
        .to_string();
    Database::set_conn_identity(1);
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(0);
    let after = db.query("SELECT v FROM t WHERE id = 5", []).unwrap()[0][0]
        .as_text()
        .to_string();
    drop(db);
    cleanup(&path);
    assert_eq!(
        inside, "new",
        "transaction did not see its own UPDATE through a point lookup"
    );
    assert_eq!(
        inside_scan, 1,
        "transaction did not see its own UPDATE through a scan"
    );
    assert_eq!(sibling, "old", "a sibling saw an uncommitted write");
    assert_eq!(after, "new");
}

/// Randomized transfer storm: W writers move money between accounts with
/// application-side read-modify-write inside BEGIN CONCURRENT (retrying on
/// BUSY_SNAPSHOT), R readers assert conservation on every snapshot.
#[test]
fn transfer_storm_conserves_money() {
    let (mut db, path) = waldb("storm");
    db.execute(
        "CREATE TABLE acc (id INTEGER PRIMARY KEY, bal INTEGER NOT NULL)",
        [],
    )
    .unwrap();
    const N: i64 = 24;
    const START: i64 = 1000;
    db.execute("BEGIN", []).unwrap();
    for i in 0..N {
        db.execute(
            "INSERT INTO acc VALUES (?, ?)",
            [Value::Integer(i), Value::Integer(START)],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let db = Arc::new(db);

    let writers = 4u64;
    let readers = 2u64;
    let per_writer = 150u64;
    let stop = Arc::new(AtomicBool::new(false));
    let bad_reads = Arc::new(AtomicU64::new(0));
    let commits = Arc::new(AtomicU64::new(0));
    let conflicts = Arc::new(AtomicU64::new(0));
    let barrier = Arc::new(Barrier::new((writers + readers) as usize));
    let mut handles = Vec::new();

    for w in 0..writers {
        let db = Arc::clone(&db);
        let commits = Arc::clone(&commits);
        let conflicts = Arc::clone(&conflicts);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            Database::set_conn_identity(100 + w);
            let mut seed = 0x9E37_79B9u64.wrapping_mul(w + 1) | 1;
            let mut next = move || {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed
            };
            barrier.wait();
            let mut done = 0;
            let mut attempts = 0;
            while done < per_writer {
                attempts += 1;
                assert!(attempts < per_writer * 200, "writer {w} starved");
                let a = (next() % N as u64) as i64;
                let mut b = (next() % N as u64) as i64;
                if a == b {
                    b = (b + 1) % N;
                }
                let amt = (next() % 50) as i64 + 1;
                if let Err(e) = db.begin_concurrent_transaction() {
                    panic!("begin failed: {e}");
                }
                let step = (|| -> rustqlite::Result<()> {
                    let ba = db.query("SELECT bal FROM acc WHERE id = ?", [Value::Integer(a)])?[0]
                        [0]
                    .as_integer();
                    let bb = db.query("SELECT bal FROM acc WHERE id = ?", [Value::Integer(b)])?[0]
                        [0]
                    .as_integer();
                    let mut s = db.prepare("UPDATE acc SET bal = ? WHERE id = ?")?;
                    s.bind(1, Value::Integer(ba - amt))?;
                    s.bind(2, Value::Integer(a))?;
                    s.step()?;
                    s.reset();
                    s.bind(1, Value::Integer(bb + amt))?;
                    s.bind(2, Value::Integer(b))?;
                    s.step()?;
                    drop(s);
                    db.commit_concurrent_transaction()
                })();
                match step {
                    Ok(()) => {
                        done += 1;
                        commits.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        let _ = db.rollback_concurrent_transaction();
                        assert!(is_busy_snapshot(&e), "unexpected writer error: {e}");
                        conflicts.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Database::set_conn_identity(0);
        }));
    }
    for r in 0..readers {
        let db = Arc::clone(&db);
        let stop = Arc::clone(&stop);
        let bad = Arc::clone(&bad_reads);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            Database::set_conn_identity(500 + r);
            barrier.wait();
            while !stop.load(Ordering::Acquire) {
                let rows = db.query("SELECT sum(bal), count(*) FROM acc", []).unwrap();
                let (s, c) = (rows[0][0].as_integer(), rows[0][1].as_integer());
                if s != N * START || c != N {
                    bad.fetch_add(1, Ordering::Relaxed);
                    eprintln!("reader saw sum={s} count={c}");
                }
            }
            Database::set_conn_identity(0);
        }));
    }
    let readers_handles: Vec<_> = handles.drain(writers as usize..).collect();
    for h in handles {
        h.join().expect("writer panicked");
    }
    stop.store(true, Ordering::Release);
    for h in readers_handles {
        h.join().expect("reader panicked");
    }
    let total = int(&db, "SELECT sum(bal) FROM acc");
    let n = int(&db, "SELECT count(*) FROM acc");
    eprintln!(
        "commits={} conflicts={} bad_reads={}",
        commits.load(Ordering::Relaxed),
        conflicts.load(Ordering::Relaxed),
        bad_reads.load(Ordering::Relaxed)
    );
    let ic = db.query("PRAGMA integrity_check", []).unwrap()[0][0]
        .as_text()
        .to_string();
    drop(db);
    // Reopen: WAL recovery must reproduce the same committed state.
    let db2 = Database::open(&path).unwrap();
    let total2 = int(&db2, "SELECT sum(bal) FROM acc");
    drop(db2);
    cleanup(&path);
    assert_eq!(ic, "ok");
    assert_eq!(n, N);
    assert_eq!(
        total,
        N * START,
        "money was created/destroyed by concurrent transfers"
    );
    assert_eq!(total2, N * START, "reopen changed the committed total");
    assert_eq!(
        bad_reads.load(Ordering::Relaxed),
        0,
        "readers observed non-conserving snapshots"
    );
}

/// Two concurrent transactions insert the SAME value into a UNIQUE
/// column (different rowids): the row-level merge must never let both
/// commit — the second's uniqueness probe read a key the first wrote.
#[test]
fn concurrent_unique_duplicates_never_both_commit() {
    let (mut db, path) = waldb("uniq");
    db.execute(
        "CREATE TABLE u (id INTEGER PRIMARY KEY, email TEXT UNIQUE)",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO u (email) VALUES ('seed')", [])
        .unwrap();
    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("INSERT INTO u (email) VALUES ('dup@x')", [])
        .unwrap();
    Database::set_conn_identity(2);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("INSERT INTO u (email) VALUES ('dup@x')", [])
        .unwrap();
    Database::set_conn_identity(1);
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(2);
    let second = db.execute("COMMIT", []);
    if second.is_err() {
        let _ = db.execute("ROLLBACK", []);
    }
    Database::set_conn_identity(0);
    let n = int(&db, "SELECT count(*) FROM u WHERE email = 'dup@x'");
    let ic = db.query("PRAGMA integrity_check", []).unwrap()[0][0]
        .as_text()
        .to_string();
    drop(db);
    cleanup(&path);
    assert!(
        second.is_err(),
        "a duplicate UNIQUE key committed through the merge path"
    );
    assert_eq!(n, 1);
    assert_eq!(ic, "ok");
}

/// Disjoint indexed point updates on ONE hot index leaf still MERGE: the
/// equality probes are tracked per key, not per page.
#[test]
fn disjoint_indexed_updates_on_hot_leaf_merge() {
    let (mut db, path) = waldb("idxmerge");
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER, v INTEGER)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX t_k ON t(k)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=20 {
        db.execute("INSERT INTO t (k, v) VALUES (?, 0)", [Value::Integer(i)])
            .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("UPDATE t SET v = v + 1 WHERE k = 3", [])
        .unwrap();
    Database::set_conn_identity(2);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("UPDATE t SET v = v + 10 WHERE k = 7", [])
        .unwrap();
    Database::set_conn_identity(1);
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(2);
    let r = db.execute("COMMIT", []);
    Database::set_conn_identity(0);
    let v3 = int(&db, "SELECT v FROM t WHERE k = 3");
    let v7 = int(&db, "SELECT v FROM t WHERE k = 7");
    drop(db);
    cleanup(&path);
    assert!(r.is_ok(), "disjoint indexed updates conflicted: {r:?}");
    assert_eq!((v3, v7), (1, 10));
}

/// Read-only BEGIN CONCURRENT transactions summing balances while
/// writers transfer: every successfully returned sum must be the
/// conserved total (a failed read is fine — a wrong one is not).
#[test]
fn concurrent_readers_never_see_partial_transfers() {
    let (mut db, path) = waldb("rdtxn");
    db.execute(
        "CREATE TABLE acc (id INTEGER PRIMARY KEY, bal INTEGER NOT NULL, pad TEXT)",
        [],
    )
    .unwrap();
    const N: i64 = 40;
    db.execute("BEGIN", []).unwrap();
    for i in 0..N {
        db.execute(
            "INSERT INTO acc VALUES (?, 100, ?)",
            [Value::Integer(i), Value::Text("p".repeat(300).into())],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let db = Arc::new(db);
    let stop = Arc::new(AtomicBool::new(false));
    let bad = Arc::new(AtomicU64::new(0));
    let good = Arc::new(AtomicU64::new(0));
    let mut hs = Vec::new();
    for w in 0..3u64 {
        let db = Arc::clone(&db);
        hs.push(thread::spawn(move || {
            Database::set_conn_identity(10 + w);
            let mut x = 0x1234_5678u64 ^ (w * 7919);
            for _ in 0..200 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let a = (x % N as u64) as i64;
                let b = ((x >> 20) % N as u64) as i64;
                if a == b {
                    continue;
                }
                db.begin_concurrent_transaction().unwrap();
                let r = (|| -> rustqlite::Result<()> {
                    let mut s = db.prepare("UPDATE acc SET bal = bal - 1 WHERE id = ?")?;
                    s.bind(1, Value::Integer(a))?;
                    s.step()?;
                    drop(s);
                    let mut s = db.prepare("UPDATE acc SET bal = bal + 1 WHERE id = ?")?;
                    s.bind(1, Value::Integer(b))?;
                    s.step()?;
                    drop(s);
                    db.commit_concurrent_transaction()
                })();
                if r.is_err() {
                    let _ = db.rollback_concurrent_transaction();
                }
            }
            Database::set_conn_identity(0);
        }));
    }
    let readers: Vec<_> = (0..2u64)
        .map(|r| {
            let db = Arc::clone(&db);
            let stop = Arc::clone(&stop);
            let bad = Arc::clone(&bad);
            let good = Arc::clone(&good);
            thread::spawn(move || {
                Database::set_conn_identity(50 + r);
                while !stop.load(Ordering::Acquire) {
                    db.begin_concurrent_transaction().unwrap();
                    // Two separate statements over the two halves: a
                    // mixed snapshot would show up as a non-conserving
                    // total.
                    let lo = db.query("SELECT sum(bal) FROM acc WHERE id < 20", []);
                    let hi = db.query("SELECT sum(bal) FROM acc WHERE id >= 20", []);
                    let _ = db.commit_concurrent_transaction();
                    if let (Ok(lo), Ok(hi)) = (lo, hi) {
                        let s = lo[0][0].as_integer() + hi[0][0].as_integer();
                        if s == N * 100 {
                            good.fetch_add(1, Ordering::Relaxed);
                        } else {
                            bad.fetch_add(1, Ordering::Relaxed);
                            eprintln!("partial snapshot: {s}");
                        }
                    }
                }
                Database::set_conn_identity(0);
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    stop.store(true, Ordering::Release);
    for h in readers {
        h.join().unwrap();
    }
    let total = int(&db, "SELECT sum(bal) FROM acc");
    drop(db);
    cleanup(&path);
    assert_eq!(total, N * 100);
    assert_eq!(
        bad.load(Ordering::Relaxed),
        0,
        "a read-only transaction saw a partial transfer"
    );
    assert!(
        good.load(Ordering::Relaxed) > 0,
        "no reader transaction completed"
    );
}

/// PHANTOM double booking: both transactions check that a slot is FREE
/// (an absent-row read through a non-unique index), both insert a booking
/// for it. No constraint stops the duplicate — only read-set validation
/// can: the second committer's "no booking" read was invalidated by the
/// first commit's insert into the very index range it probed.
#[test]
fn phantom_double_booking_is_rejected() {
    let (mut db, path) = waldb("phantom_book");
    db.execute(
        "CREATE TABLE booking (id INTEGER PRIMARY KEY, room INTEGER, slot INTEGER, who TEXT)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX booking_room_slot ON booking(room, slot)", [])
        .unwrap();
    // Unrelated rows, so the probe lands in a populated index.
    for r in 1..=50 {
        db.execute(
            "INSERT INTO booking(room, slot, who) VALUES (?, 9, 'x')",
            [Value::Integer(r)],
        )
        .unwrap();
    }
    let free = "SELECT count(*) FROM booking WHERE room = 7 AND slot = 10";

    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    let f1 = int(&db, free);
    Database::set_conn_identity(2);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    let f2 = int(&db, free);
    assert_eq!((f1, f2), (0, 0));

    Database::set_conn_identity(1);
    db.execute(
        "INSERT INTO booking(room, slot, who) VALUES (7, 10, 'alice')",
        [],
    )
    .unwrap();
    Database::set_conn_identity(2);
    db.execute(
        "INSERT INTO booking(room, slot, who) VALUES (7, 10, 'bob')",
        [],
    )
    .unwrap();

    Database::set_conn_identity(1);
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(2);
    let second = db.execute("COMMIT", []);
    if second.is_err() {
        let _ = db.execute("ROLLBACK", []);
    }
    Database::set_conn_identity(0);
    let n = int(&db, free);
    drop(db);
    cleanup(&path);
    if let Err(e) = &second {
        assert!(is_busy_snapshot(e), "unexpected error: {e}");
    }
    assert!(
        second.is_err(),
        "phantom: both 'slot is free' transactions committed ({n} bookings for one slot)"
    );
    assert_eq!(n, 1);
}

/// PHANTOM through a range aggregate: a transaction derives a summary row
/// from `count(*)` over a key range while a sibling commits an insert INTO
/// that range; committing the stale summary must fail.
#[test]
fn phantom_range_summary_is_rejected() {
    let (mut db, path) = waldb("phantom_range");
    db.execute("CREATE TABLE ev (id INTEGER PRIMARY KEY, day INTEGER)", [])
        .unwrap();
    db.execute("CREATE INDEX ev_day ON ev(day)", []).unwrap();
    db.execute(
        "CREATE TABLE daily (day INTEGER PRIMARY KEY, n INTEGER)",
        [],
    )
    .unwrap();
    for i in 0..300 {
        db.execute("INSERT INTO ev(day) VALUES (?)", [Value::Integer(i % 30)])
            .unwrap();
    }

    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    let n = int(&db, "SELECT count(*) FROM ev WHERE day BETWEEN 10 AND 12");
    assert_eq!(n, 30);

    // The sibling inserts into the counted range and commits first (an
    // autocommit statement joining the concurrent regime).
    Database::set_conn_identity(2);
    db.execute("INSERT INTO ev(day) VALUES (11)", []).unwrap();

    Database::set_conn_identity(1);
    db.execute("INSERT INTO daily VALUES (10, ?)", [Value::Integer(n)])
        .unwrap();
    let commit = db.execute("COMMIT", []);
    if commit.is_err() {
        let _ = db.execute("ROLLBACK", []);
    }
    Database::set_conn_identity(0);
    let summaries = int(&db, "SELECT count(*) FROM daily");
    let actual = int(&db, "SELECT count(*) FROM ev WHERE day BETWEEN 10 AND 12");
    drop(db);
    cleanup(&path);
    if let Err(e) = &commit {
        assert!(is_busy_snapshot(e), "unexpected error: {e}");
    }
    assert!(
        commit.is_err(),
        "phantom: a summary computed before a committed insert into its range committed \
         (summary {n}, actual {actual})"
    );
    assert_eq!((summaries, actual), (0, 31));
}

/// Absence of a ROWID is a read too: a transaction that saw `id = 500`
/// missing and acts on it must not commit after a sibling created it.
#[test]
fn absent_rowid_read_is_validated() {
    let (mut db, path) = waldb("absent_rowid");
    db.execute("CREATE TABLE item (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    db.execute("CREATE TABLE log (id INTEGER PRIMARY KEY, note TEXT)", [])
        .unwrap();
    for i in 1..=200 {
        db.execute("INSERT INTO item VALUES (?, 'v')", [Value::Integer(i)])
            .unwrap();
    }

    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    let seen = db
        .query("SELECT v FROM item WHERE id = 500", [])
        .unwrap()
        .len();
    assert_eq!(seen, 0);

    Database::set_conn_identity(2);
    db.execute("INSERT INTO item VALUES (500, 'new')", [])
        .unwrap();

    Database::set_conn_identity(1);
    db.execute("INSERT INTO log(note) VALUES ('500 was missing')", [])
        .unwrap();
    let commit = db.execute("COMMIT", []);
    if commit.is_err() {
        let _ = db.execute("ROLLBACK", []);
    }
    Database::set_conn_identity(0);
    let logs = int(&db, "SELECT count(*) FROM log");
    drop(db);
    cleanup(&path);
    if let Err(e) = &commit {
        assert!(is_busy_snapshot(e), "unexpected error: {e}");
    }
    assert!(
        commit.is_err(),
        "a commit acting on 'id 500 is missing' succeeded after id 500 was committed"
    );
    assert_eq!(logs, 0);
}
