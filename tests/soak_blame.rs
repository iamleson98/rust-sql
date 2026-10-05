//! Soak blame probe: on a 10M-row file, run (a) readers-only, (b)
//! writers-only, (c) both — measuring peak RSS per phase.
use rustqlite::{Database, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Instant;

fn rss_mb() -> f64 {
    if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                if let Some(kb) = rest.split_whitespace().next() {
                    return kb.parse::<u64>().unwrap_or(0) as f64 / 1024.0;
                }
            }
        }
    }
    0.0
}
fn peak_mb() -> f64 {
    if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("VmHWM:") {
                if let Some(kb) = rest.split_whitespace().next() {
                    return kb.parse::<u64>().unwrap_or(0) as f64 / 1024.0;
                }
            }
        }
    }
    0.0
}

// ---- counting allocator (SB_COUNT=1) --------------------------------
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicUsize;
use std::sync::Mutex;

#[allow(dead_code)] // --no-default-features diagnostic builds attach it as the global allocator
struct CountingAlloc;
static COUNT_ON: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicU64 = AtomicU64::new(0);
static HIST: Mutex<Option<BTreeMap<usize, (u64, u64)>>> = Mutex::new(None); // size -> (count, bytes)

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc(layout);
        if p.is_null() {
            return p;
        }
        if COUNT_ON.load(std::sync::atomic::Ordering::Relaxed) == 1 {
            LIVE.fetch_add(layout.size() as u64, std::sync::atomic::Ordering::Relaxed);
            let mut h = HIST.lock().unwrap();
            let h = h.get_or_insert_with(BTreeMap::new);
            let e = h.entry(layout.size()).or_insert((0, 0));
            e.0 += 1;
            e.1 += layout.size() as u64;
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if COUNT_ON.load(std::sync::atomic::Ordering::Relaxed) == 1 {
            LIVE.fetch_sub(layout.size() as u64, std::sync::atomic::Ordering::Relaxed);
        }
        System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = System.realloc(ptr, layout, new_size);
        if !p.is_null() && COUNT_ON.load(std::sync::atomic::Ordering::Relaxed) == 1 {
            if new_size >= layout.size() {
                LIVE.fetch_add(
                    (new_size - layout.size()) as u64,
                    std::sync::atomic::Ordering::Relaxed,
                );
                let mut h = HIST.lock().unwrap();
                let h = h.get_or_insert_with(BTreeMap::new);
                let e = h.entry(new_size).or_insert((0, 0));
                e.0 += 1;
                e.1 += new_size as u64;
            } else {
                LIVE.fetch_sub(
                    (layout.size() - new_size) as u64,
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
        }
        p
    }
}

// allocator detached (mimalloc owns the default build; the counting
// variant needs --no-default-features)
// static GLOBAL_UNUSED: CountingAlloc = CountingAlloc;

fn count_dump(tag: &str) {
    if COUNT_ON.load(std::sync::atomic::Ordering::Relaxed) != 1 {
        return;
    }
    let h = HIST.lock().unwrap();
    println!(
        "[sb] COUNT {tag}: live={:.1}MB",
        LIVE.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1048576.0
    );
    if let Some(h) = h.as_ref() {
        let mut v: Vec<(&usize, &(u64, u64))> = h.iter().collect();
        v.sort_by_key(|(_, (_, bytes))| std::cmp::Reverse(*bytes));
        let total: u64 = h.values().map(|(_, b)| *b).sum();
        println!(
            "[sb] COUNT {tag}: total-allocated={:.1}MB distinct-sizes={}",
            total as f64 / 1048576.0,
            h.len()
        );
        for (sz, (n, bytes)) in v.iter().take(12) {
            println!(
                "[sb] COUNT {tag}: size={:>9} n={:>8} total={:>9.1}MB",
                sz,
                n,
                *bytes as f64 / 1048576.0
            );
        }
    }
}

fn count_on() {
    COUNT_ON.store(1, std::sync::atomic::Ordering::Relaxed);
}

#[test]
fn soak_blame() {
    let rows: u64 = std::env::var("MEGA_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000_000);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("soak.db");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("PRAGMA journal_mode = WAL", []).unwrap();
        db.execute("PRAGMA synchronous = NORMAL", []).unwrap();
        db.execute(
            "CREATE TABLE events (id INTEGER PRIMARY KEY, k INTEGER, note TEXT)",
            [],
        )
        .unwrap();
        let noix = std::env::var("SB_NOIX").is_ok_and(|v| v == "1");
        if !noix {
            db.execute("CREATE INDEX ix_events_k ON events (k)", [])
                .unwrap();
        }
        db.execute("CREATE TABLE tiny (id INTEGER PRIMARY KEY, v INT)", [])
            .unwrap();
        let t = Instant::now();
        let batch = 5000u64;
        let mut next = 0u64;
        while next < rows {
            let n = batch.min(rows - next);
            let mut sql = String::with_capacity(48 * n as usize + 48);
            sql.push_str("INSERT INTO events (id, k, note) VALUES ");
            for j in 0..n {
                let i = next + j + 1;
                if j > 0 {
                    sql.push(',');
                }
                sql.push_str(&format!(
                    "({}, {}, 'n{:08x}')",
                    i,
                    i % 1_000_000,
                    (i * 2654435761) & 0xffff_ffff
                ));
            }
            db.execute("BEGIN", []).unwrap();
            db.execute(&sql, []).unwrap();
            db.execute("COMMIT", []).unwrap();
            next += n;
        }
        println!(
            "[sb] build {rows} in {:.0}s rss={:.0}MB",
            t.elapsed().as_secs_f64(),
            rss_mb()
        );
        drop(db);
    }

    let db = {
        let mut d = Database::open(&path).unwrap();
        d.execute("PRAGMA journal_mode = WAL", []).unwrap();
        d.execute("PRAGMA synchronous = NORMAL", []).unwrap();
        Arc::new(d)
    };

    // (b0.5) variant dispatch: SB_VARIANT selects ONE experiment; each
    // runs in a fresh process so every RSS baseline is clean.
    let variant = std::env::var("SB_VARIANT").unwrap_or_default();
    if !variant.is_empty() && variant != "stage" {
        let p0 = peak_mb();
        if std::env::var("SB_COUNT").is_ok_and(|v| v == "1") {
            count_on();
        }
        let insert_step = |db: &Database, sql: &str, id: i64| {
            let mut s = db.prepare(sql).unwrap();
            s.bind(1, Value::Integer(id)).unwrap();
            if sql.contains("note") {
                s.bind(2, Value::Integer(id % 1_000_000)).unwrap();
                s.bind(3, Value::Text("n00000064".into())).unwrap();
            } else {
                s.bind(2, Value::Integer(1)).unwrap();
            }
            s.step().unwrap();
        };
        match variant.as_str() {
            "big" | "tiny" | "noix" | "auto" => {
                let target_events = variant != "tiny";
                let sql = if target_events {
                    "INSERT INTO events (id, k, note) VALUES (?, ?, ?)"
                } else {
                    "INSERT INTO tiny (id, v) VALUES (?, ?)"
                };
                if variant == "auto" {
                    insert_step(&db, sql, 900_000_001);
                    println!("[sb] AUTO autocommit insert peak-d={:.0}MB", peak_mb() - p0);
                } else {
                    db.begin_concurrent_transaction().unwrap();
                    println!("[sb] {variant}: after BEGIN peak-d={:.0}MB", peak_mb() - p0);
                    insert_step(&db, sql, 900_000_001);
                    println!("[sb] {variant}: after STEP1 peak-d={:.0}MB", peak_mb() - p0);
                    count_dump("step1");
                    db.commit_concurrent_transaction().unwrap();
                    println!(
                        "[sb] {variant}: after COMMIT peak-d={:.0}MB",
                        peak_mb() - p0
                    );
                    count_dump("commit");
                }
            }
            "noconc" => {
                // Sole mutable handle (the Arc must go away first so the
                // WAL writer lease is free).
                drop(db);
                let mut d2 = Database::open(&path).unwrap();
                d2.execute("BEGIN", []).unwrap();
                println!("[sb] noconc: after BEGIN peak-d={:.0}MB", peak_mb() - p0);
                d2.execute(
                    "INSERT INTO events (id, k, note) VALUES (900000001, 5, 'x')",
                    [],
                )
                .unwrap();
                println!("[sb] noconc: after INSERT peak-d={:.0}MB", peak_mb() - p0);
                d2.execute("COMMIT", []).unwrap();
                println!("[sb] noconc: after COMMIT peak-d={:.0}MB", peak_mb() - p0);
                let d = Database::open(&path).unwrap();
                let got = d
                    .query("SELECT count(*) FROM events WHERE id >= 900000001", [])
                    .unwrap();
                println!("[sb] noconc verify: {:?}", got[0][0].as_integer());
                return;
            }
            "scantxn" => {
                drop(db);
                let mut d2 = Database::open(&path).unwrap();
                let p1 = peak_mb();
                d2.execute("BEGIN", []).unwrap();
                let got = d2.query("SELECT count(*), sum(k) FROM events", []).unwrap();
                println!(
                    "[sb] scantxn: in-txn aggregate n={:?} s={:?} peak-d={:.0}MB rss={:.0}MB",
                    got[0][0].as_integer(),
                    got[0][1].as_integer(),
                    peak_mb() - p1,
                    rss_mb()
                );
                d2.execute("COMMIT", []).unwrap();
                println!("[sb] scantxn: after COMMIT peak-d={:.0}MB", peak_mb() - p1);
                return;
            }
            "m6" => {
                // M6 shape at probe scale: checkpoint + mass-delete + VACUUM.
                // Fresh sole mutable handle (drop the Arc first).
                drop(db);
                let mut d2 = Database::open(&path).unwrap();
                let p0 = peak_mb();
                d2.execute("PRAGMA wal_checkpoint(TRUNCATE)", []).unwrap();
                println!(
                    "[sb] m6: after checkpoint peak-d={:.0}MB rss={:.0}MB",
                    peak_mb() - p0,
                    rss_mb()
                );
                let t0 = std::time::Instant::now();
                d2.execute(
                    &format!("DELETE FROM events WHERE id <= {rows} AND id % 10 < 4"),
                    [],
                )
                .unwrap();
                println!(
                    "[sb] m6: after DELETE ({:.0}s) peak-d={:.0}MB rss={:.0}MB",
                    t0.elapsed().as_secs_f64(),
                    peak_mb() - p0,
                    rss_mb()
                );
                let t1 = std::time::Instant::now();
                // Timeline sampler: 50ms RSS/HWM during the VACUUM.
                let samples: std::sync::Arc<std::sync::Mutex<Vec<(f64, f64, f64)>>> =
                    std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
                let sx = samples.clone();
                let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let dx = done.clone();
                std::thread::spawn(move || {
                    while !dx.load(std::sync::atomic::Ordering::Relaxed) {
                        let t = t1.elapsed().as_secs_f64();
                        let r = rss_mb();
                        let pk = peak_mb();
                        if let Ok(mut v) = sx.lock() {
                            v.push((t, r, pk));
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                });
                eprintln!("[sb] VACUUM start t=0");
                d2.execute("VACUUM", []).unwrap();
                done.store(true, std::sync::atomic::Ordering::Relaxed);
                println!(
                    "[sb] m6: after VACUUM ({:.0}s) peak-d={:.0}MB rss={:.0}MB",
                    t1.elapsed().as_secs_f64(),
                    peak_mb() - p0,
                    rss_mb()
                );
                if let Ok(v) = samples.lock() {
                    let mut last_r = -1.0;
                    for (t, r, pk) in v.iter() {
                        if (r - last_r).abs() >= 1.0 {
                            println!(
                                "[sb] vac-timeline t={:5.2}s rss={:6.1}MB peak={:6.1}MB",
                                t, r, pk
                            );
                            last_r = *r;
                        }
                    }
                    if let Some((t, r, pk)) = v.last() {
                        println!(
                            "[sb] vac-timeline END t={:5.2}s rss={:6.1}MB peak={:6.1}MB",
                            t, r, pk
                        );
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
                println!("[sb] m6: +0.5s settle rss={:.0}MB", rss_mb());
                let got = d2.query("SELECT count(*) FROM events", []).unwrap();
                println!("[sb] m6: survivors={:?}", got[0][0].as_integer());
                return;
            }
            other => panic!("unknown SB_VARIANT {other}"),
        }
        let got = db
            .query("SELECT count(*) FROM events WHERE id >= 900000001", [])
            .unwrap();
        println!(
            "[sb] {variant} verify: {:?} rss={:.0}MB",
            got[0][0].as_integer(),
            rss_mb()
        );
        return;
    }

    // (a) readers-only: 2 threads x 40 scans
    {
        let barrier = Arc::new(Barrier::new(2));
        let mut hs = Vec::new();
        let p0 = peak_mb();
        for t in 0..2 {
            let db = Arc::clone(&db);
            let b = Arc::clone(&barrier);
            hs.push(std::thread::spawn(move || {
                Database::set_conn_identity(100 + t as u64);
                b.wait();
                for _ in 0..40 {
                    let got = db
                        .query(
                            &format!("SELECT count(*), sum(k) FROM events WHERE id <= {rows}"),
                            [],
                        )
                        .unwrap();
                    let _ = got[0][0].as_integer();
                }
            }));
        }
        for h in hs {
            h.join().unwrap();
        }
        println!("[sb] READERS-ONLY peak delta = {:.0}MB", peak_mb() - p0);
    }

    // (b0) stage trace: one concurrent txn walked stage by stage on the
    // MAIN thread — RSS after each stage names the allocating stage.
    {
        let p0 = peak_mb();
        println!("[sb] stage0 rss={:.0}MB", rss_mb());
        let r = db.begin_concurrent_transaction();
        println!(
            "[sb] after BEGIN rss={:.0}MB peak-d={:.0}MB (begin={:?})",
            rss_mb(),
            peak_mb() - p0,
            r.is_ok()
        );
        let mut stmt = db
            .prepare("INSERT INTO events (id, k, note) VALUES (?, ?, ?)")
            .ok();
        if let Some(stmt) = stmt.as_mut() {
            println!(
                "[sb] after PREPARE rss={:.0}MB peak-d={:.0}MB",
                rss_mb(),
                peak_mb() - p0
            );
            stmt.bind(1, Value::Integer(900_000_001)).unwrap();
            stmt.bind(2, Value::Integer(123)).unwrap();
            stmt.bind(3, Value::Text("n00000064".into())).unwrap();
            match stmt.step() {
                Ok(_) => println!(
                    "[sb] after STEP1 rss={:.0}MB peak-d={:.0}MB",
                    rss_mb(),
                    peak_mb() - p0
                ),
                Err(e) => println!("[sb] step1 err: {e}"),
            }
            stmt.reset();
            stmt.bind(1, Value::Integer(900_000_002)).unwrap();
            stmt.bind(2, Value::Integer(124)).unwrap();
            stmt.bind(3, Value::Text("n00000065".into())).unwrap();
            match stmt.step() {
                Ok(_) => println!(
                    "[sb] after STEP2 rss={:.0}MB peak-d={:.0}MB",
                    rss_mb(),
                    peak_mb() - p0
                ),
                Err(e) => println!("[sb] step2 err: {e}"),
            }
        }
        match db.commit_concurrent_transaction() {
            Ok(()) => println!(
                "[sb] after COMMIT rss={:.0}MB peak-d={:.0}MB",
                rss_mb(),
                peak_mb() - p0
            ),
            Err(e) => println!("[sb] commit err: {e}"),
        }
        // Second txn, same stages: re-clone cost or one-time machinery?
        db.begin_concurrent_transaction().unwrap();
        let mut s2 = db
            .prepare("INSERT INTO events (id, k, note) VALUES (?, ?, ?)")
            .unwrap();
        s2.bind(1, Value::Integer(900_000_003)).unwrap();
        s2.bind(2, Value::Integer(125)).unwrap();
        s2.bind(3, Value::Text("n00000066".into())).unwrap();
        s2.step().unwrap();
        drop(s2);
        db.commit_concurrent_transaction().unwrap();
        println!(
            "[sb] after TXN2 rss={:.0}MB peak-d={:.0}MB",
            rss_mb(),
            peak_mb() - p0
        );
        let got = db
            .query("SELECT count(*) FROM events WHERE id >= 900000001", [])
            .unwrap();
        println!(
            "[sb] verify rows={:?} rss={:.0}MB",
            got[0][0].as_integer(),
            rss_mb()
        );
    }

    // (b) writers-only: SB_WRITERS x SB_TXNS txns x 25 rows (defaults
    // 4 x 20, the marathon's M5 shape). Writer 0 traces RSS per txn.
    {
        let nw: usize = std::env::var("SB_WRITERS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4);
        let ntx: i64 = std::env::var("SB_TXNS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20);
        let attempts = Arc::new(AtomicU64::new(0));
        let commits = Arc::new(AtomicU64::new(0));
        let barrier = Arc::new(Barrier::new(nw));
        let mut hs = Vec::new();
        let p0 = peak_mb();
        for tid in 0..nw as i64 {
            let db = Arc::clone(&db);
            let b = Arc::clone(&barrier);
            let attempts = Arc::clone(&attempts);
            let commits = Arc::clone(&commits);
            hs.push(std::thread::spawn(move || {
                Database::set_conn_identity((tid + 1) as u64);
                b.wait();
                for t in 0..ntx {
                    for _attempt in 0..64 {
                        attempts.fetch_add(1, Ordering::Relaxed);
                        if db.begin_concurrent_transaction().is_err() {
                            continue;
                        }
                        let mut stmt = db
                            .prepare("INSERT INTO events (id, k, note) VALUES (?, ?, ?)")
                            .unwrap();
                        let mut ok = true;
                        for j in 0..25i64 {
                            let id = 1_000_000_000 + tid * 10_000_000 + t * 25 + j + 1;
                            stmt.bind(1, Value::Integer(id)).unwrap();
                            stmt.bind(2, Value::Integer(id % 1_000_000)).unwrap();
                            stmt.bind(3, Value::Text(format!("n{:08x}", id & 0xffff).into()))
                                .unwrap();
                            if stmt.step().is_err() {
                                ok = false;
                                break;
                            }
                            stmt.reset();
                        }
                        if !ok {
                            let _ = db.rollback_concurrent_transaction();
                            continue;
                        }
                        if db.commit_concurrent_transaction().is_ok() {
                            commits.fetch_add(1, Ordering::Relaxed);
                            if tid == 0 {
                                println!(
                                    "[sb] w0 txn {t} rss={:.0}MB peak={:.0}MB",
                                    rss_mb(),
                                    peak_mb()
                                );
                            }
                            break;
                        }
                    }
                }
            }));
        }
        for h in hs {
            h.join().unwrap();
        }
        println!(
            "[sb] WRITERS-ONLY peak delta = {:.0}MB (attempts={} commits={})",
            peak_mb() - p0,
            attempts.load(Ordering::Relaxed),
            commits.load(Ordering::Relaxed)
        );
    }

    // (c) both together
    {
        let barrier = Arc::new(Barrier::new(6));
        let mut hs = Vec::new();
        let p0 = peak_mb();
        let writers_live = Arc::new(AtomicU64::new(4));
        for tid in 0..4i64 {
            let db = Arc::clone(&db);
            let b = Arc::clone(&barrier);
            let wl = Arc::clone(&writers_live);
            hs.push(std::thread::spawn(move || {
                Database::set_conn_identity((tid + 1) as u64);
                b.wait();
                for t in 0..20i64 {
                    for _attempt in 0..64 {
                        if db.begin_concurrent_transaction().is_err() {
                            continue;
                        }
                        let mut stmt = db
                            .prepare("INSERT INTO events (id, k, note) VALUES (?, ?, ?)")
                            .unwrap();
                        let mut ok = true;
                        for j in 0..25i64 {
                            let id = 2_000_000_000 + tid * 10_000_000 + t * 25 + j + 1;
                            stmt.bind(1, Value::Integer(id)).unwrap();
                            stmt.bind(2, Value::Integer(id % 1_000_000)).unwrap();
                            stmt.bind(3, Value::Text(format!("n{:08x}", id & 0xffff).into()))
                                .unwrap();
                            if stmt.step().is_err() {
                                ok = false;
                                break;
                            }
                            stmt.reset();
                        }
                        if !ok {
                            let _ = db.rollback_concurrent_transaction();
                            continue;
                        }
                        if db.commit_concurrent_transaction().is_ok() {
                            break;
                        }
                    }
                }
                wl.fetch_sub(1, Ordering::Relaxed);
            }));
        }
        for _ in 0..2 {
            let db = Arc::clone(&db);
            let b = Arc::clone(&barrier);
            let wl = Arc::clone(&writers_live);
            hs.push(std::thread::spawn(move || {
                b.wait();
                while wl.load(Ordering::Relaxed) > 0 {
                    let _ = db
                        .query(
                            &format!("SELECT count(*), sum(k) FROM events WHERE id <= {rows}"),
                            [],
                        )
                        .unwrap();
                }
            }));
        }
        for h in hs {
            h.join().unwrap();
        }
        println!("[sb] BOTH peak delta = {:.0}MB", peak_mb() - p0);
    }
    println!("[sb] done rss={:.0}MB", rss_mb());
}
