//! Regression: N reader threads + a plain autocommit writer thread on ONE
//! shared `Arc<Database>` over a WAL file (the `&self` prepared-statement
//! API), and on one `Database::open` handle per thread.
//!
//! The bug (found by a benchmark probe, ~50% of 8-reader runs): plain `&self`
//! DML mutates the LIVE page cache in place. A B-tree interior split
//! re-initializes the page and re-adds its cells across several page-lock
//! acquisitions, so a concurrent reader walking the same page saw
//! `Corruption("interior page N has no valid child for rowid R")` — and the
//! writer's child->parent lock order against the reader's parent->child
//! coupling could deadlock the whole process. The on-disk database was
//! never corrupt; readers observed (or blocked on) a half-rewritten tree.
//!
//! Fix under test: the plain DML statement takes the install gate's WRITE
//! side for its tree surgery (excluding the gated plain readers exactly like
//! a concurrent COMMIT's install does), and the gate is re-entrant and
//! phase-fair so readers keep making progress.
//!
//! Invariants checked from every reader, on every iteration:
//! - `SUM(bal)` over the accounts table is constant (every transfer is ONE
//!   autocommit statement moving money between two rows);
//! - `COUNT(*) == MAX(id)` over the append-only `log` table (ids are dense
//!   and never deleted — a torn split drops rows from the middle);
//! - the log count never regresses and every id <= a previously observed
//!   MAX still resolves by point lookup;
//! - no statement ever errors.

use rustqlite::{Database, StepResult, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

const ACCOUNTS: i64 = 2_000;
const START_BAL: i64 = 100;
/// Seed rows in `log`. With 200-byte payloads (~18 rows per 4 KiB leaf)
/// this makes a 3-level tree whose right-most interior page keeps
/// splitting as the writer appends — the shape that tore.
const SEED_LOG: i64 = 14_000;

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("rq-shared-rw-{}-{}.db", tag, std::process::id()));
    cleanup(&p);
    p
}

fn cleanup(p: &std::path::Path) {
    let s = p.to_str().unwrap();
    for suffix in ["", "-wal", "-shm", "-journal", "-rsqllock"] {
        let _ = std::fs::remove_file(format!("{s}{suffix}"));
    }
}

fn pad(i: i64) -> String {
    format!("{i:0>8}-{}", "x".repeat(190))
}

fn setup(path: &std::path::Path) {
    let mut db = Database::open(path).unwrap();
    db.execute("PRAGMA journal_mode=WAL", []).unwrap();
    db.execute("PRAGMA synchronous=NORMAL", []).unwrap();
    db.execute(
        "CREATE TABLE acct (id INTEGER PRIMARY KEY, bal INTEGER NOT NULL)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE log (id INTEGER PRIMARY KEY, pad TEXT, v INTEGER)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX log_v ON log(v)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=ACCOUNTS {
        db.execute(
            "INSERT INTO acct VALUES (?, ?)",
            [Value::Integer(i), Value::Integer(START_BAL)],
        )
        .unwrap();
    }
    for i in 1..=SEED_LOG {
        db.execute(
            "INSERT INTO log VALUES (?, ?, ?)",
            [
                Value::Integer(i),
                Value::Text(pad(i).into()),
                Value::Integer(i * 7),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db.execute("PRAGMA wal_checkpoint(TRUNCATE)", []).ok();
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

#[derive(Default)]
struct Tally {
    ops: u64,
    errors: Vec<String>,
}

/// One-row, N-column integer query helper over a prepared statement.
fn row2(st: &mut rustqlite::Statement<'_>) -> Result<Option<(i64, i64)>, String> {
    let out = match st.step().map_err(|e| format!("{e:?}"))? {
        StepResult::Row => Some((st.column_int(0), st.column_int(1))),
        StepResult::Done => None,
    };
    // Drain (exactly one row expected) and reset for reuse.
    while matches!(st.step().map_err(|e| format!("{e:?}"))?, StepResult::Row) {}
    st.reset();
    Ok(out)
}

fn reader(db: &Database, stop: &AtomicBool, seed: u64) -> Tally {
    let mut t = Tally::default();
    let fail = |t: &mut Tally, msg: String| {
        if t.errors.len() < 5 {
            t.errors.push(msg);
        }
    };
    let mut sum_st = db.prepare("SELECT SUM(bal), COUNT(*) FROM acct").unwrap();
    let mut log_st = db.prepare("SELECT COUNT(*), MAX(id) FROM log").unwrap();
    let mut pt_st = db.prepare("SELECT id, v FROM log WHERE id = ?").unwrap();
    let mut rng_st = db
        .prepare("SELECT COUNT(*), SUM(v) FROM log WHERE id >= ? AND id < ?")
        .unwrap();
    let mut rng = Rng(seed | 1);
    let (mut seen_count, mut seen_max) = (0i64, 0i64);
    while !stop.load(Ordering::Relaxed) {
        t.ops += 1;
        match row2(&mut sum_st) {
            Ok(Some((sum, n))) => {
                if sum != ACCOUNTS * START_BAL || n != ACCOUNTS {
                    fail(&mut t, format!("acct sum={sum} count={n}"));
                }
            }
            other => fail(&mut t, format!("acct sum: {other:?}")),
        }
        match row2(&mut log_st) {
            Ok(Some((count, max))) => {
                if count != max {
                    fail(&mut t, format!("log count={count} != max id={max}"));
                }
                if count < seen_count {
                    fail(
                        &mut t,
                        format!("log count regressed {seen_count} -> {count}"),
                    );
                }
                seen_count = count;
                seen_max = seen_max.max(max);
            }
            other => fail(&mut t, format!("log count: {other:?}")),
        }
        // Point lookups across the whole tree, biased to the hot right edge.
        for _ in 0..8 {
            let r = rng.next();
            let id = if r & 1 == 0 {
                (r >> 8) as i64 % seen_max.max(1) + 1
            } else {
                seen_max - ((r >> 8) as i64 % 64).min(seen_max - 1)
            };
            pt_st.bind(1, Value::Integer(id)).unwrap();
            match row2(&mut pt_st) {
                Ok(Some((rid, v))) => {
                    if rid != id || v != id * 7 {
                        fail(&mut t, format!("point id={id} got ({rid},{v})"));
                    }
                }
                other => fail(
                    &mut t,
                    format!("point id={id} (<= seen max {seen_max}): {other:?}"),
                ),
            }
        }
        // A 100-row range scan: dense ids => exactly 100 rows.
        let lo = (rng.next() >> 8) as i64 % (seen_max - 100).max(1) + 1;
        rng_st.bind(1, Value::Integer(lo)).unwrap();
        rng_st.bind(2, Value::Integer(lo + 100)).unwrap();
        match row2(&mut rng_st) {
            Ok(Some((n, _))) => {
                if n != 100 {
                    fail(
                        &mut t,
                        format!("range [{lo},{}) returned {n} rows", lo + 100),
                    );
                }
            }
            other => fail(&mut t, format!("range: {other:?}")),
        }
    }
    t
}

/// Plain autocommit writer: appends to `log` (page splits, interior
/// splits) and moves money between accounts (index-free in-place updates),
/// each as ONE statement.
fn writer(db: &Database, stop: &AtomicBool, commits: &AtomicU64) -> Tally {
    let mut t = Tally::default();
    let mut ins = db
        .prepare("INSERT INTO log (id, pad, v) VALUES (?, ?, ?)")
        .unwrap();
    let mut xfer = db
        .prepare(
            "UPDATE acct SET bal = CASE id WHEN ? THEN bal - ? ELSE bal + ? END \
             WHERE id = ? OR id = ?",
        )
        .unwrap();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut next_id = SEED_LOG + 1;
    while !stop.load(Ordering::Relaxed) {
        let res = if t.ops % 3 != 2 {
            ins.bind(1, Value::Integer(next_id)).unwrap();
            ins.bind(2, Value::Text(pad(next_id).into())).unwrap();
            ins.bind(3, Value::Integer(next_id * 7)).unwrap();
            let r = ins.step();
            ins.reset();
            if r.is_ok() {
                next_id += 1;
            }
            r
        } else {
            let a = (rng.next() >> 8) as i64 % ACCOUNTS + 1;
            let mut b = (rng.next() >> 8) as i64 % ACCOUNTS + 1;
            if b == a {
                b = a % ACCOUNTS + 1;
            }
            let amt = (rng.next() >> 8) as i64 % 50 + 1;
            xfer.bind(1, Value::Integer(a)).unwrap();
            xfer.bind(2, Value::Integer(amt)).unwrap();
            xfer.bind(3, Value::Integer(amt)).unwrap();
            xfer.bind(4, Value::Integer(a)).unwrap();
            xfer.bind(5, Value::Integer(b)).unwrap();
            let r = xfer.step();
            xfer.reset();
            r
        };
        t.ops += 1;
        match res {
            Ok(_) => {
                commits.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                if t.errors.len() < 5 {
                    t.errors.push(format!("write: {e:?}"));
                }
            }
        }
    }
    t
}

fn final_check(path: &std::path::Path, min_log: i64) {
    let db = Database::open(path).unwrap();
    let ic = db.query("PRAGMA integrity_check", []).unwrap();
    assert_eq!(ic[0][0].as_text(), "ok", "integrity_check: {ic:?}");
    let r = db.query("SELECT SUM(bal) FROM acct", []).unwrap();
    assert_eq!(r[0][0], Value::Integer(ACCOUNTS * START_BAL));
    let r = db.query("SELECT COUNT(*), MAX(id) FROM log", []).unwrap();
    assert_eq!(r[0][0], r[0][1], "log ids must stay dense");
    match &r[0][0] {
        Value::Integer(n) => assert!(*n >= min_log, "writer never appended: {n}"),
        v => panic!("count: {v:?}"),
    }
}

fn run(readers: usize, secs: f64, shared: bool, tag: &str) {
    let path = tmp_path(tag);
    setup(&path);
    let stop = Arc::new(AtomicBool::new(false));
    let commits = Arc::new(AtomicU64::new(0));
    let barrier = Arc::new(Barrier::new(readers + 2));
    let shared_db: Option<Arc<Database>> = if shared {
        let mut db = Database::open(&path).unwrap();
        db.execute("PRAGMA journal_mode=WAL", []).unwrap();
        db.execute("PRAGMA synchronous=NORMAL", []).unwrap();
        db.execute("PRAGMA busy_timeout=5000", []).unwrap();
        // Force the multi-threaded aggregate scan on the 2k-row table so the
        // worker-thread reader gate is exercised too.
        db.execute("PRAGMA parallel_scan=500", []).unwrap();
        Some(Arc::new(db))
    } else {
        None
    };

    let mut handles = Vec::new();
    for i in 0..readers + 1 {
        let is_writer = i == readers;
        let (stop, commits, barrier) = (stop.clone(), commits.clone(), barrier.clone());
        let sdb = shared_db.clone();
        let path = path.clone();
        handles.push(std::thread::spawn(move || {
            let db_owned;
            let db: &Database = match &sdb {
                Some(d) => d,
                None => {
                    let mut d = Database::open(&path).unwrap();
                    d.execute("PRAGMA journal_mode=WAL", []).unwrap();
                    d.execute("PRAGMA synchronous=NORMAL", []).unwrap();
                    d.execute("PRAGMA busy_timeout=5000", []).unwrap();
                    db_owned = d;
                    &db_owned
                }
            };
            barrier.wait();
            if is_writer {
                writer(db, &stop, &commits)
            } else {
                reader(db, &stop, i as u64 * 7919 + 13)
            }
        }));
    }
    barrier.wait();
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_secs_f64(secs));
    stop.store(true, Ordering::Relaxed);
    let tallies: Vec<Tally> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let el = t0.elapsed().as_secs_f64();

    let mut problems = Vec::new();
    for (i, t) in tallies.iter().enumerate() {
        for e in &t.errors {
            problems.push(format!(
                "{} {i}: {e}",
                if i == readers { "writer" } else { "reader" }
            ));
        }
    }
    let reads: u64 = tallies[..readers].iter().map(|t| t.ops).sum();
    let writes = commits.load(Ordering::Relaxed);
    eprintln!(
        "[{tag}] {readers} readers + 1 writer, {el:.1}s: {reads} reader iterations, {writes} commits"
    );
    drop(shared_db);
    assert!(problems.is_empty(), "invariant violations: {problems:#?}");
    assert!(writes > 50, "writer starved: {writes} commits");
    assert!(reads > 50, "readers starved: {reads} iterations");
    final_check(&path, SEED_LOG + 1);
    cleanup(&path);
}

#[test]
fn shared_database_eight_readers_one_plain_writer() {
    run(8, 3.0, true, "shared8");
}

#[test]
fn shared_database_sixteen_readers_one_plain_writer() {
    run(16, 2.5, true, "shared16");
}

#[test]
fn per_thread_connections_eight_readers_one_writer() {
    run(8, 2.5, false, "conns8");
}
