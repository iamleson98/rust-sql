//! Scoreboard probe: file-backed WAL concurrency, ours vs SQLite, one engine
//! per process.
//!
//! usage: probe_score_conc <engine> <scenario> [readers] [writers] [secs] [batch]
//!   engine   ours-shared  one Arc<Database>, per-thread prepared &self stmts
//!            ours-conns   one Database::open handle per thread (conn-per-thread)
//!            sqlite       rusqlite, one Connection per thread
//!   scenario rw           R readers + W writers for <secs>; reports ops/s + p99
//!            oc           <readers> threads x <secs>*100 open/query/close cycles
//!   batch    rows per writer transaction (1 = autocommit)
//! Setup (both engines): WAL, synchronous=NORMAL, busy_timeout=5000, table
//! t(id pk, name, val indexed, score) with 100k rows.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use rustqlite::{Database, StepResult, Value};

const ROWS: i64 = 100_000;

#[derive(Default)]
struct Out {
    ops: u64,
    errs: u64,
    first_err: Option<String>,
    lat_ns: Vec<u32>,
}

fn pct(v: &mut [u32], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_unstable();
    let i = ((v.len() as f64 - 1.0) * p) as usize;
    v[i] as f64 / 1e3
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
}

fn jm() -> String {
    format!(
        "PRAGMA journal_mode={};",
        std::env::var("JMODE").unwrap_or_else(|_| "WAL".into())
    )
}

fn setup(engine: &str, path: &str) {
    for s in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{path}{s}"));
    }
    if engine == "sqlite" {
        let c = rusqlite::Connection::open(path).unwrap();
        c.execute_batch(&format!(
            "{} PRAGMA synchronous=NORMAL;
             CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL);
             CREATE INDEX t_val ON t(val); BEGIN;",
            jm()
        ))
        .unwrap();
        {
            let mut s = c.prepare("INSERT INTO t VALUES (?1,?2,?3,?4)").unwrap();
            for i in 1..=ROWS {
                s.execute(rusqlite::params![
                    i,
                    format!("name{i}"),
                    i * 2,
                    i as f64 * 1.5
                ])
                .unwrap();
            }
        }
        c.execute_batch("COMMIT").unwrap();
    } else {
        let mut db = Database::open(path).unwrap();
        db.execute(jm().trim_end_matches(';'), []).unwrap();
        db.execute("PRAGMA synchronous=NORMAL", []).unwrap();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
            [],
        )
        .unwrap();
        db.execute("CREATE INDEX t_val ON t(val)", []).unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 1..=ROWS {
            db.execute(
                "INSERT INTO t VALUES (?,?,?,?)",
                [
                    Value::Integer(i),
                    Value::Text(format!("name{i}").into()),
                    Value::Integer(i * 2),
                    Value::Real(i as f64 * 1.5),
                ],
            )
            .unwrap();
        }
        db.execute("COMMIT", []).unwrap();
        db.execute("PRAGMA wal_checkpoint(TRUNCATE)", []).ok();
    }
}

fn open_ours(path: &str) -> Database {
    let mut db = Database::open(path).unwrap();
    db.execute(jm().trim_end_matches(';'), []).unwrap();
    db.execute("PRAGMA synchronous=NORMAL", []).unwrap();
    db.execute("PRAGMA busy_timeout=5000", []).unwrap();
    if std::env::var("SCORE_PREWRITE").is_ok() {
        db.execute("UPDATE t SET val = val WHERE id = 1", [])
            .unwrap();
    }
    if let Ok(c) = std::env::var("SCORE_CACHE") {
        db.execute(&format!("PRAGMA cache_size={c}"), []).unwrap();
    }
    db
}

fn open_sqlite(path: &str) -> rusqlite::Connection {
    let c = rusqlite::Connection::open(path).unwrap();
    c.busy_timeout(Duration::from_millis(5000)).unwrap();
    c.execute_batch(&format!("{} PRAGMA synchronous=NORMAL;", jm()))
        .unwrap();
    c
}

#[derive(Clone, Copy)]
struct Cfg {
    readers: usize,
    writers: usize,
    secs: f64,
    batch: usize,
}

// ---- reader / writer bodies -------------------------------------------------

fn reader_ours(db: &Database, stop: &AtomicBool, seed: u64) -> Out {
    let mut o = Out::default();
    let mut p1 = db.prepare("SELECT name, val FROM t WHERE id = ?").unwrap();
    let mut p2 = db
        .prepare("SELECT id, val FROM t WHERE id >= ? AND id < ? + 100")
        .unwrap();
    let mut r = Rng(seed);
    o.lat_ns.reserve(2_000_000);
    while !stop.load(Ordering::Relaxed) {
        let id = (r.next() % ROWS as u64) as i64 + 1;
        let t = Instant::now();
        if o.ops % 10 == 9 {
            p2.bind(1, Value::Integer(id)).unwrap();
            p2.bind(2, Value::Integer(id)).unwrap();
            while let StepResult::Row = p2.step().unwrap() {
                let _ = p2.column_int(0);
                let _ = p2.column_int(1);
            }
            p2.reset();
        } else {
            p1.bind(1, Value::Integer(id)).unwrap();
            while let StepResult::Row = p1.step().unwrap() {
                let _ = p1.column_text(0);
                let _ = p1.column_int(1);
            }
            p1.reset();
        }
        o.lat_ns.push(t.elapsed().as_nanos() as u32);
        o.ops += 1;
    }
    o
}

fn reader_sqlite(path: &str, stop: &AtomicBool, seed: u64) -> Out {
    let c = open_sqlite(path);
    let mut o = Out::default();
    let mut p1 = c.prepare("SELECT name, val FROM t WHERE id = ?1").unwrap();
    let mut p2 = c
        .prepare("SELECT id, val FROM t WHERE id >= ?1 AND id < ?1 + 100")
        .unwrap();
    let mut r = Rng(seed);
    o.lat_ns.reserve(2_000_000);
    while !stop.load(Ordering::Relaxed) {
        let id = (r.next() % ROWS as u64) as i64 + 1;
        let t = Instant::now();
        if o.ops % 10 == 9 {
            let mut rows = p2.query([id]).unwrap();
            while let Some(row) = rows.next().unwrap() {
                let _ = row.get::<_, i64>(0).unwrap();
                let _ = row.get::<_, i64>(1).unwrap();
            }
        } else {
            let mut rows = p1.query([id]).unwrap();
            while let Some(row) = rows.next().unwrap() {
                let _ = row.get::<_, String>(0).unwrap();
                let _ = row.get::<_, i64>(1).unwrap();
            }
        }
        o.lat_ns.push(t.elapsed().as_nanos() as u32);
        o.ops += 1;
    }
    o
}

/// Writer: mix of INSERT (disjoint id range per writer) and indexed UPDATE.
fn writer_ours(
    db: &Database,
    shared: bool,
    stop: &AtomicBool,
    w: usize,
    batch: usize,
    nw: usize,
) -> Out {
    let mut o = Out::default();
    // multi-statement txns on our side use BEGIN CONCURRENT (prepared plain
    // BEGIN is rejected by the &self statement API)
    let use_conc = batch > 1 || (shared && nw > 1);
    if use_conc {
        Database::set_conn_identity((w + 1) as u64);
    }

    let mut ins = db
        .prepare("INSERT INTO t (id, name, val, score) VALUES (?, ?, ?, ?)")
        .unwrap();
    let mut upd = db
        .prepare("UPDATE t SET val = val + 1 WHERE id = ?")
        .unwrap();
    let mut r = Rng(0x9E3779B97F4A7C15 ^ (w as u64 * 77 + 1));
    let base = 1_000_000_000i64 * (w as i64 + 1);
    let mut n = 0i64;
    o.lat_ns.reserve(1_000_000);
    'outer: while !stop.load(Ordering::Relaxed) {
        let t = Instant::now();
        if use_conc {
            db.begin_concurrent_transaction().unwrap();
        }
        let mut failed = false;
        for _ in 0..batch {
            let res = if n % 2 == 0 {
                ins.bind(1, Value::Integer(base + n)).unwrap();
                ins.bind(2, Value::Text(format!("w{n}").into())).unwrap();
                ins.bind(3, Value::Integer(n * 3)).unwrap();
                ins.bind(4, Value::Real(n as f64)).unwrap();
                let r = ins.step();
                ins.reset();
                r
            } else {
                upd.bind(1, Value::Integer((r.next() % ROWS as u64) as i64 + 1))
                    .unwrap();
                let r = upd.step();
                upd.reset();
                r
            };
            n += 1;
            if let Err(e) = &res {
                if o.first_err.is_none() {
                    o.first_err = Some(format!("{e:?}"));
                }
                failed = true;
                break;
            }
        }
        if use_conc {
            let c = db.commit_concurrent_transaction().map(|_| ());
            if let Err(e) = &c {
                if o.first_err.is_none() {
                    o.first_err = Some(format!("commit {e:?}"));
                }
                failed = true;
            }
        }
        if failed {
            o.errs += 1;
            if use_conc {
                let _ = db.rollback_concurrent_transaction();
            }
            if o.errs > 100_000 {
                break 'outer;
            }
        } else {
            o.lat_ns.push(t.elapsed().as_nanos() as u32);
            o.ops += batch as u64;
        }
    }
    if use_conc {
        Database::set_conn_identity(0);
    }
    let _ = shared;
    o
}

fn writer_sqlite(path: &str, stop: &AtomicBool, w: usize, batch: usize) -> Out {
    let c = open_sqlite(path);
    let mut o = Out::default();
    let mut ins = c
        .prepare("INSERT INTO t (id, name, val, score) VALUES (?1, ?2, ?3, ?4)")
        .unwrap();
    let mut upd = c
        .prepare("UPDATE t SET val = val + 1 WHERE id = ?1")
        .unwrap();
    let mut r = Rng(0x9E3779B97F4A7C15 ^ (w as u64 * 77 + 1));
    let base = 1_000_000_000i64 * (w as i64 + 1);
    let mut n = 0i64;
    o.lat_ns.reserve(1_000_000);
    while !stop.load(Ordering::Relaxed) {
        let t = Instant::now();
        if batch > 1 && c.execute_batch("BEGIN").is_err() {
            o.errs += 1;
            continue;
        }
        let mut failed = false;
        for _ in 0..batch {
            let res = if n % 2 == 0 {
                ins.execute(rusqlite::params![
                    base + n,
                    format!("w{n}"),
                    n * 3,
                    n as f64
                ])
            } else {
                upd.execute([(r.next() % ROWS as u64) as i64 + 1])
            };
            n += 1;
            if res.is_err() {
                failed = true;
                break;
            }
        }
        if batch > 1 {
            if failed {
                let _ = c.execute_batch("ROLLBACK");
            } else if c.execute_batch("COMMIT").is_err() {
                failed = true;
                let _ = c.execute_batch("ROLLBACK");
            }
        }
        if failed {
            o.errs += 1;
        } else {
            o.lat_ns.push(t.elapsed().as_nanos() as u32);
            o.ops += batch as u64;
        }
    }
    o
}

fn run_rw(engine: &str, path: &str, cfg: Cfg) {
    setup(engine, path);
    let stop = Arc::new(AtomicBool::new(false));
    let nthreads = cfg.readers + cfg.writers;
    let barrier = Arc::new(Barrier::new(nthreads + 1));
    let shared_db: Option<Arc<Database>> = if engine == "ours-shared" {
        Some(Arc::new(open_ours(path)))
    } else {
        None
    };
    let mut hs: Vec<(bool, std::thread::JoinHandle<Out>)> = Vec::new();
    for i in 0..cfg.readers + cfg.writers {
        let is_writer = i >= cfg.readers;
        let stop = stop.clone();
        let barrier = barrier.clone();
        let path = path.to_string();
        let engine = engine.to_string();
        let sdb = shared_db.clone();
        let batch = cfg.batch;
        let nw = cfg.writers;
        let widx = i.saturating_sub(cfg.readers);
        hs.push((
            is_writer,
            std::thread::spawn(move || {
                // connection setup happens before the barrier
                match engine.as_str() {
                    "ours-shared" => {
                        let db = sdb.unwrap();
                        barrier.wait();
                        if is_writer {
                            writer_ours(&db, true, &stop, widx, batch, nw)
                        } else {
                            reader_ours(&db, &stop, i as u64 + 1)
                        }
                    }
                    "ours-conns" => {
                        let db = open_ours(&path);
                        barrier.wait();
                        if is_writer {
                            writer_ours(&db, false, &stop, widx, batch, nw)
                        } else {
                            reader_ours(&db, &stop, i as u64 + 1)
                        }
                    }
                    _ => {
                        barrier.wait();
                        if is_writer {
                            writer_sqlite(&path, &stop, widx, batch)
                        } else {
                            reader_sqlite(&path, &stop, i as u64 + 1)
                        }
                    }
                }
            }),
        ));
    }
    barrier.wait();
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_secs_f64(cfg.secs));
    let sz0 = |s: &str| {
        std::fs::metadata(format!("{path}{s}"))
            .map(|m| m.len())
            .unwrap_or(0)
    };
    println!("MIDRUN db={} wal={}", sz0(""), sz0("-wal"));
    stop.store(true, Ordering::Relaxed);
    let outs: Vec<(bool, Out)> = hs
        .into_iter()
        .map(|(w, h)| {
            (
                w,
                h.join().unwrap_or_else(|_| {
                    println!("THREAD_PANIC");
                    Out::default()
                }),
            )
        })
        .collect();
    let el = t0.elapsed().as_secs_f64();
    let (mut rops, mut wops, mut werr) = (0u64, 0u64, 0u64);
    let (mut rl, mut wl): (Vec<u32>, Vec<u32>) = (vec![], vec![]);
    for (w, mut o) in outs {
        if let Some(e) = &o.first_err {
            println!("FIRST_ERR {}", &e[..e.len().min(500)]);
        }
        if w {
            wops += o.ops;
            werr += o.errs;
            wl.append(&mut o.lat_ns);
        } else {
            rops += o.ops;
            rl.append(&mut o.lat_ns);
        }
    }
    println!(
        "RESULT engine={engine} R={} W={} batch={} reads/s={:.0} writes/s={:.0} write_errs={} \
         read_p50us={:.1} read_p99us={:.1} wtxn_p50us={:.1} wtxn_p99us={:.1} wtxn_max_us={:.0}",
        cfg.readers,
        cfg.writers,
        cfg.batch,
        rops as f64 / el,
        wops as f64 / el,
        werr,
        pct(&mut rl, 0.5),
        pct(&mut rl, 0.99),
        pct(&mut wl, 0.5),
        pct(&mut wl, 0.99),
        pct(&mut wl, 1.0)
    );
    drop(shared_db);
    if engine.starts_with("ours") {
        let db = Database::open(path).unwrap();
        let r = db.query("PRAGMA integrity_check", []).unwrap();
        println!(
            "INTEGRITY {}",
            r.iter()
                .map(|x| x[0].as_text())
                .collect::<Vec<_>>()
                .join(";")
                .chars()
                .take(200)
                .collect::<String>()
        );
    }
    let sz = |s: &str| {
        std::fs::metadata(format!("{path}{s}"))
            .map(|m| m.len())
            .unwrap_or(0)
    };
    println!("FILES db={} wal={}", sz(""), sz("-wal"));
}

fn run_oc(engine: &str, path: &str, threads: usize, cycles: usize) {
    setup(engine, path);
    let total = Arc::new(AtomicU64::new(0));
    let t0 = Instant::now();
    let mut hs = vec![];
    for tid in 0..threads {
        let path = path.to_string();
        let engine = engine.to_string();
        let total = total.clone();
        hs.push(std::thread::spawn(move || {
            let mut r = Rng(tid as u64 + 5);
            for _ in 0..cycles {
                let id = (r.next() % ROWS as u64) as i64 + 1;
                if engine == "sqlite" {
                    let c = open_sqlite(&path);
                    let v: i64 = c
                        .query_row("SELECT val FROM t WHERE id = ?1", [id], |r| r.get(0))
                        .unwrap();
                    assert!(v > 0);
                } else {
                    let db = open_ours(&path);
                    let rows = db
                        .query("SELECT val FROM t WHERE id = ?", [Value::Integer(id)])
                        .unwrap();
                    assert_eq!(rows.len(), 1);
                }
                total.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    let el = t0.elapsed().as_secs_f64();
    println!(
        "RESULT engine={engine} open_close threads={threads} cycles={} per_s={:.0} mean_us_per_cycle_per_thread={:.1}",
        total.load(Ordering::Relaxed),
        total.load(Ordering::Relaxed) as f64 / el,
        el * 1e6 / cycles as f64
    );
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let engine = a.get(1).cloned().unwrap_or("ours-shared".into());
    let scenario = a.get(2).cloned().unwrap_or("rw".into());
    let readers: usize = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(4);
    let writers: usize = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(1);
    let secs: f64 = a.get(5).and_then(|s| s.parse().ok()).unwrap_or(3.0);
    let batch: usize = a.get(6).and_then(|s| s.parse().ok()).unwrap_or(1);
    let dir = std::env::var("SCORE_DIR").unwrap_or_else(|_| "/tmp/claude-501/score_conc".into());
    std::fs::create_dir_all(&dir).unwrap();
    let path = format!("{dir}/{}_{}.db", engine, std::process::id());
    match scenario.as_str() {
        "rw" => run_rw(
            &engine,
            &path,
            Cfg {
                readers,
                writers,
                secs,
                batch,
            },
        ),
        "oc" => run_oc(&engine, &path, readers, (secs * 100.0) as usize),
        _ => panic!("scenario"),
    }
    for s in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{path}{s}"));
    }
}
