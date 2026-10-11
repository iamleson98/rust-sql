//! Focused probe: the marathon M2/M9 aggregate shape —
//! `SELECT count(*), sum(k), min(k), max(k) FROM events WHERE id <= N`
//! over a lean (id INTEGER PRIMARY KEY, k INTEGER) table — engine vs
//! bundled SQLite, at a scale this box can hold.
use rustqlite::{Database, Value};
use std::time::Instant;

fn gen_k(i: u64) -> i64 {
    // the marathon's distribution: k in [0, 1_000_000)
    (i % 1_000_000) as i64
}

fn main() {
    let rows: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(2_000_000);
    let epath = "/tmp/aggprobe_rq.db";
    let spath = "/tmp/aggprobe_sq.db";
    for p in [epath, spath] {
        let _ = std::fs::remove_file(p);
        let _ = std::fs::remove_file(format!("{p}-wal"));
        let _ = std::fs::remove_file(format!("{p}-shm"));
    }

    let agg_sql = format!("SELECT count(*), sum(k), min(k), max(k) FROM events WHERE id <= {rows}");

    // ---- engine build ----
    {
        let t = Instant::now();
        let mut db = Database::open(epath).unwrap();
        db.execute(
            "CREATE TABLE events (id INTEGER PRIMARY KEY, k INTEGER)",
            [],
        )
        .unwrap();
        let batch = 5000u64;
        let mut s = 0;
        while s < rows {
            let n = batch.min(rows - s);
            let mut sql = String::with_capacity(64 * n as usize + 40);
            sql.push_str("INSERT INTO events (id, k) VALUES ");
            for j in 0..n {
                if j > 0 {
                    sql.push(',');
                }
                sql.push_str(&format!("({}, {})", s + j + 1, gen_k(s + j + 1)));
            }
            db.execute(&sql, []).unwrap();
            s += n;
        }
        println!(
            "engine build {} rows in {:.2}s",
            rows,
            t.elapsed().as_secs_f64()
        );
    }
    // ---- sqlite build (same statements) ----
    {
        use rusqlite::Connection;
        let t = Instant::now();
        let rc = Connection::open(spath).unwrap();
        rc.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=OFF; PRAGMA cache_size=-262144;",
        )
        .unwrap();
        rc.execute(
            "CREATE TABLE events (id INTEGER PRIMARY KEY, k INTEGER)",
            [],
        )
        .unwrap();
        let batch = 5000u64;
        let mut s = 0;
        while s < rows {
            let n = batch.min(rows - s);
            let mut sql = String::with_capacity(64 * n as usize + 40);
            sql.push_str("INSERT INTO events (id, k) VALUES ");
            for j in 0..n {
                if j > 0 {
                    sql.push(',');
                }
                sql.push_str(&format!("({}, {})", s + j + 1, gen_k(s + j + 1)));
            }
            rc.execute(&sql, []).unwrap();
            s += n;
        }
        println!(
            "sqlite build {} rows in {:.2}s",
            rows,
            t.elapsed().as_secs_f64()
        );
    }

    // ---- engine aggregate (cold, then warm) ----
    let mut exp = None;
    {
        let db = Database::open(epath).unwrap();
        for pass in 0..3 {
            let t = Instant::now();
            let got = db.query(&agg_sql, []).unwrap();
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            let r = &got[0];
            println!(
                "engine  agg pass{} {:8.1} ms  -> count={} sum={:?} min={:?} max={:?}",
                pass,
                ms,
                r[0].as_integer(),
                r[1].as_integer(),
                r[2].as_integer(),
                r[3].as_integer(),
            );
            if exp.is_none() {
                exp = Some((
                    r[0].as_integer(),
                    r[1].as_integer(),
                    r[2].as_integer(),
                    r[3].as_integer(),
                ));
            }
        }
        // no-WHERE variant: takes the FUSED machine (range-free)
        let bare = "SELECT count(*), sum(k), min(k), max(k) FROM events";
        for pass in 0..3 {
            let t = Instant::now();
            let got = db.query(bare, []).unwrap();
            println!(
                "engine  agg(bare=fused) pass{} {:8.1} ms count={}",
                pass,
                t.elapsed().as_secs_f64() * 1000.0,
                got[0][0].as_integer()
            );
        }
        // range-bounded sub-shape too (mid range)
        let mid = format!(
            "SELECT count(*), sum(k), min(k), max(k) FROM events WHERE id <= {}",
            rows / 2
        );
        for pass in 0..2 {
            let t = Instant::now();
            let got = db.query(&mid, []).unwrap();
            println!(
                "engine  agg(half) pass{} {:8.1} ms count={}",
                pass,
                t.elapsed().as_secs_f64() * 1000.0,
                got[0][0].as_integer()
            );
        }
    }
    // ---- sqlite aggregate ----
    {
        use rusqlite::Connection;
        let rc = Connection::open(spath).unwrap();
        let mut stmt = rc.prepare(&agg_sql).unwrap();
        for pass in 0..3 {
            let t = Instant::now();
            let mut rows_out = stmt.query([]).unwrap();
            let r = rows_out.next().unwrap().unwrap();
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            let v: (i64, Option<i64>, Option<i64>, Option<i64>) = (
                r.get(0).unwrap(),
                r.get(1).unwrap(),
                r.get(2).unwrap(),
                r.get(3).unwrap(),
            );
            println!(
                "sqlite  agg pass{} {:8.1} ms  -> count={} sum={:?} min={:?} max={:?}",
                pass, ms, v.0, v.1, v.2, v.3
            );
            if pass == 0 {
                let e = exp.unwrap();
                let flat = (v.0, v.1.unwrap_or(0), v.2.unwrap_or(0), v.3.unwrap_or(0));
                assert_eq!((e.0, e.1, e.2, e.3), flat, "answer equality");
            }
        }
        let mid = format!(
            "SELECT count(*), sum(k), min(k), max(k) FROM events WHERE id <= {}",
            rows / 2
        );
        let mut stmt = rc.prepare(&mid).unwrap();
        for pass in 0..2 {
            let t = Instant::now();
            let mut rows_out = stmt.query([]).unwrap();
            let r = rows_out.next().unwrap().unwrap();
            println!(
                "sqlite  agg(half) pass{} {:8.1} ms count={}",
                pass,
                t.elapsed().as_secs_f64() * 1000.0,
                r.get::<_, i64>(0).unwrap()
            );
        }
    }
    let _ = Value::Null;
}
