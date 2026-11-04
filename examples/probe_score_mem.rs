//! Scoreboard probe: memory / CPU / disk for ONE engine per process.
//! Wrap with `/usr/bin/time -l` to get max RSS and user+sys CPU.
//!
//! usage: probe_score_mem <ours|sqlite> <workload> <db-path> [rows]
//! workloads:
//!   idle        open empty file DB, CREATE TABLE, one SELECT (RSS floor)
//!   build       insert <rows> in ONE txn (WAL+NORMAL), then close
//!   build_idx   same, with 2 secondary indexes (val, name) created first
//!   build_auto  <rows> autocommit single-row inserts (WAL+NORMAL), then close
//!   update_all  open existing db; UPDATE t SET val=val+1 (one txn)
//!   delete_half open existing db; DELETE half the rows; reports sizes
//!   scan        open existing db; count/sum/avg, GROUP BY, 20k point reads
//!   sort        open existing db; SELECT * ORDER BY name (full sort), stream rows
//!   join        open existing db; self-join aggregate on pk
//!   readers8    open existing db in 8 threads (conn each), each runs a full scan
//! Prints: RESULT wall_s=.. and FILES db=.. wal=.. (before/after close).
use rustqlite::{Database, StepResult, Value};
use std::time::Instant;

fn sz(p: &str, s: &str) -> u64 {
    std::fs::metadata(format!("{p}{s}"))
        .map(|m| m.len())
        .unwrap_or(0)
}

fn clean(p: &str) {
    for s in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{p}{s}"));
    }
}

fn open_ours(p: &str) -> Database {
    let mut db = Database::open(p).unwrap();
    db.execute("PRAGMA journal_mode=WAL", []).unwrap();
    db.execute("PRAGMA synchronous=NORMAL", []).unwrap();
    db
}

fn open_sq(p: &str) -> rusqlite::Connection {
    let c = rusqlite::Connection::open(p).unwrap();
    c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
        .unwrap();
    c
}

fn ours_q(db: &Database, sql: &str) -> usize {
    // stream via prepared statement (no full materialization on our side)
    let mut st = db.prepare(sql).unwrap();
    let mut n = 0;
    while let StepResult::Row = st.step().unwrap() {
        let _ = st.column_value(0);
        n += 1;
    }
    n
}

fn sq_q(c: &rusqlite::Connection, sql: &str) -> usize {
    let mut st = c.prepare(sql).unwrap();
    let mut rows = st.query([]).unwrap();
    let mut n = 0;
    while let Some(r) = rows.next().unwrap() {
        let _ = r.get_ref(0).unwrap();
        n += 1;
    }
    n
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let eng = a[1].as_str();
    let wl = a[2].as_str();
    let path = a[3].as_str();
    let rows: i64 = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(1_000_000);
    let ours = eng == "ours";
    let t0 = Instant::now();
    let schema = "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)";
    let mut mid_sizes = (0u64, 0u64);
    let mut checksum = 0i64;

    match wl {
        "idle" => {
            clean(path);
            if ours {
                let mut db = open_ours(path);
                db.execute(schema, []).unwrap();
                checksum = ours_q(&db, "SELECT count(*) FROM t") as i64;
            } else {
                let c = open_sq(path);
                c.execute_batch(schema).unwrap();
                checksum = sq_q(&c, "SELECT count(*) FROM t") as i64;
            }
        }
        "build" | "build_idx" | "build_auto" => {
            clean(path);
            let auto = wl == "build_auto";
            if ours {
                let mut db = open_ours(path);
                db.execute(schema, []).unwrap();
                if wl == "build_idx" {
                    db.execute("CREATE INDEX t_val ON t(val)", []).unwrap();
                    db.execute("CREATE INDEX t_name ON t(name)", []).unwrap();
                }
                if !auto {
                    db.execute("BEGIN", []).unwrap();
                }
                for i in 1..=rows {
                    db.execute(
                        "INSERT INTO t VALUES (?,?,?,?)",
                        [
                            Value::Integer(i),
                            Value::Text(format!("name{}", (i * 7919) % rows).into()),
                            Value::Integer((i * 31) % 100_003),
                            Value::Real(i as f64 * 1.5),
                        ],
                    )
                    .unwrap();
                }
                if !auto {
                    db.execute("COMMIT", []).unwrap();
                }
                mid_sizes = (sz(path, ""), sz(path, "-wal"));
            } else {
                let c = open_sq(path);
                c.execute_batch(schema).unwrap();
                if wl == "build_idx" {
                    c.execute_batch(
                        "CREATE INDEX t_val ON t(val); CREATE INDEX t_name ON t(name);",
                    )
                    .unwrap();
                }
                if !auto {
                    c.execute_batch("BEGIN").unwrap();
                }
                {
                    let mut s = c
                        .prepare_cached("INSERT INTO t VALUES (?1,?2,?3,?4)")
                        .unwrap();
                    for i in 1..=rows {
                        s.execute(rusqlite::params![
                            i,
                            format!("name{}", (i * 7919) % rows),
                            (i * 31) % 100_003,
                            i as f64 * 1.5
                        ])
                        .unwrap();
                    }
                }
                if !auto {
                    c.execute_batch("COMMIT").unwrap();
                }
                mid_sizes = (sz(path, ""), sz(path, "-wal"));
            }
        }
        "update_all" => {
            if ours {
                let mut db = open_ours(path);
                db.execute("UPDATE t SET val = val + 1", []).unwrap();
                mid_sizes = (sz(path, ""), sz(path, "-wal"));
            } else {
                let c = open_sq(path);
                c.execute_batch("UPDATE t SET val = val + 1").unwrap();
                mid_sizes = (sz(path, ""), sz(path, "-wal"));
            }
        }
        "delete_half" => {
            if ours {
                let mut db = open_ours(path);
                db.execute("DELETE FROM t WHERE id % 2 = 0", []).unwrap();
                mid_sizes = (sz(path, ""), sz(path, "-wal"));
            } else {
                let c = open_sq(path);
                c.execute_batch("DELETE FROM t WHERE id % 2 = 0").unwrap();
                mid_sizes = (sz(path, ""), sz(path, "-wal"));
            }
        }
        "scan" => {
            let qs = [
                "SELECT count(*), sum(val), avg(score) FROM t",
                "SELECT val % 100, count(*), sum(score) FROM t GROUP BY val % 100",
                "SELECT id, name FROM t WHERE val < 2000",
            ];
            if ours {
                let db = open_ours(path);
                for q in qs {
                    checksum += ours_q(&db, q) as i64;
                }
                let mut p = db.prepare("SELECT name FROM t WHERE id = ?").unwrap();
                for i in 0..20_000i64 {
                    p.bind(1, Value::Integer((i * 48271) % rows + 1)).unwrap();
                    while let StepResult::Row = p.step().unwrap() {
                        checksum += 1;
                    }
                    p.reset();
                }
            } else {
                let c = open_sq(path);
                for q in qs {
                    checksum += sq_q(&c, q) as i64;
                }
                let mut p = c.prepare("SELECT name FROM t WHERE id = ?1").unwrap();
                for i in 0..20_000i64 {
                    let mut r = p.query([(i * 48271) % rows + 1]).unwrap();
                    while r.next().unwrap().is_some() {
                        checksum += 1;
                    }
                }
            }
        }
        "sort" => {
            let q = "SELECT id, name FROM t ORDER BY name";
            if ours {
                let db = open_ours(path);
                checksum = ours_q(&db, q) as i64;
            } else {
                let c = open_sq(path);
                checksum = sq_q(&c, q) as i64;
            }
        }
        "join" => {
            let q = "SELECT count(*), sum(a.val + b.val) FROM t a JOIN t b ON a.id = b.id";
            if ours {
                let db = open_ours(path);
                checksum = ours_q(&db, q) as i64;
            } else {
                let c = open_sq(path);
                checksum = sq_q(&c, q) as i64;
            }
        }
        "memdb" => {
            // in-memory database with <rows> rows
            if ours {
                let mut db = Database::open_in_memory().unwrap();
                db.execute(schema, []).unwrap();
                db.execute("BEGIN", []).unwrap();
                for i in 1..=rows {
                    db.execute(
                        "INSERT INTO t VALUES (?,?,?,?)",
                        [
                            Value::Integer(i),
                            Value::Text(format!("name{}", (i * 7919) % rows).into()),
                            Value::Integer((i * 31) % 100_003),
                            Value::Real(i as f64 * 1.5),
                        ],
                    )
                    .unwrap();
                }
                db.execute("COMMIT", []).unwrap();
                checksum = ours_q(&db, "SELECT count(*), sum(val) FROM t") as i64;
            } else {
                let c = rusqlite::Connection::open_in_memory().unwrap();
                c.execute_batch(schema).unwrap();
                c.execute_batch("BEGIN").unwrap();
                {
                    let mut s = c
                        .prepare_cached("INSERT INTO t VALUES (?1,?2,?3,?4)")
                        .unwrap();
                    for i in 1..=rows {
                        s.execute(rusqlite::params![
                            i,
                            format!("name{}", (i * 7919) % rows),
                            (i * 31) % 100_003,
                            i as f64 * 1.5
                        ])
                        .unwrap();
                    }
                }
                c.execute_batch("COMMIT").unwrap();
                checksum = sq_q(&c, "SELECT count(*), sum(val) FROM t") as i64;
            }
        }
        "conns64" => {
            // 64 simultaneous open connections to the same existing file, each ran a point query
            if ours {
                let dbs: Vec<Database> = (0..64).map(|_| open_ours(path)).collect();
                for (k, db) in dbs.iter().enumerate() {
                    checksum +=
                        ours_q(db, &format!("SELECT name FROM t WHERE id = {}", k + 1)) as i64;
                }
            } else {
                let cs: Vec<rusqlite::Connection> = (0..64).map(|_| open_sq(path)).collect();
                for (k, c) in cs.iter().enumerate() {
                    checksum += sq_q(c, &format!("SELECT name FROM t WHERE id = {}", k + 1)) as i64;
                }
            }
        }
        "readers8" => {
            let hs: Vec<_> = (0..8)
                .map(|_| {
                    let p = path.to_string();
                    std::thread::spawn(move || {
                        if ours {
                            let db = open_ours(&p);
                            ours_q(
                                &db,
                                "SELECT count(*), sum(val), avg(score) FROM t WHERE name > 'name5'",
                            )
                        } else {
                            let c = open_sq(&p);
                            sq_q(
                                &c,
                                "SELECT count(*), sum(val), avg(score) FROM t WHERE name > 'name5'",
                            )
                        }
                    })
                })
                .collect();
            for h in hs {
                checksum += h.join().unwrap() as i64;
            }
        }
        _ => panic!("workload"),
    }
    let wall = t0.elapsed().as_secs_f64();
    println!("RESULT eng={eng} wl={wl} rows={rows} wall_s={wall:.3} chk={checksum}");
    println!(
        "FILES before_close db={} wal={} | after_close db={} wal={}",
        mid_sizes.0,
        mid_sizes.1,
        sz(path, ""),
        sz(path, "-wal")
    );
}
