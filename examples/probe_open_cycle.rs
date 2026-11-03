//! Probe: cost of a two-connection open / read / update / close cycle on
//! a file database, against SQLite doing the same work.
//!
//! cargo run --release --example probe_open_cycle -- [ROWS] [CYCLES] [wal|delete]
use rustqlite::types::Value;
use rustqlite::Database;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rows: i64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1000);
    let cycles: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(200);
    let mode = args.get(3).cloned().unwrap_or_else(|| "wal".into());
    let dir = std::env::temp_dir().join(format!("probe_open_cycle_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ours = dir.join("ours.db");
    let theirs = dir.join("theirs.db");

    {
        let mut db = Database::open(&ours).unwrap();
        db.execute(&format!("PRAGMA journal_mode={mode}"), [])
            .unwrap();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER, s TEXT)",
            [],
        )
        .unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 1..=rows {
            db.execute(
                "INSERT INTO t VALUES (?, ?, ?)",
                [
                    Value::Integer(i),
                    Value::Integer(i),
                    Value::Text(format!("s{i}").into()),
                ],
            )
            .unwrap();
        }
        db.execute("COMMIT", []).unwrap();
    }
    {
        let c = rusqlite::Connection::open(&theirs).unwrap();
        c.query_row(&format!("PRAGMA journal_mode={mode}"), [], |_| Ok(()))
            .unwrap();
        c.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER, s TEXT); BEGIN")
            .unwrap();
        for i in 1..=rows {
            c.execute(
                "INSERT INTO t VALUES (?, ?, ?)",
                rusqlite::params![i, i, format!("s{i}")],
            )
            .unwrap();
        }
        c.execute_batch("COMMIT").unwrap();
    }

    let phase = std::env::var("PHASES").is_ok();
    let mut t_open = 0f64;
    let mut t_read = 0f64;
    let mut t_upd = 0f64;
    let mut t_close = 0f64;
    let start = Instant::now();
    for k in 0..cycles {
        let t0 = Instant::now();
        let a = Database::open(&ours).unwrap();
        let mut b = Database::open(&ours).unwrap();
        let t1 = Instant::now();
        let r = a
            .query(
                "SELECT v FROM t WHERE id = ?",
                [Value::Integer(1 + (k as i64 % rows))],
            )
            .unwrap();
        assert_eq!(r.len(), 1);
        let t2 = Instant::now();
        b.execute(
            "UPDATE t SET v = v + 1 WHERE id = ?",
            [Value::Integer(1 + (k as i64 % rows))],
        )
        .unwrap();
        let t3 = Instant::now();
        drop(a);
        drop(b);
        let t4 = Instant::now();
        t_open += (t1 - t0).as_secs_f64();
        t_read += (t2 - t1).as_secs_f64();
        t_upd += (t3 - t2).as_secs_f64();
        t_close += (t4 - t3).as_secs_f64();
    }
    let ours_t = start.elapsed().as_secs_f64() / cycles as f64;

    let start = Instant::now();
    for k in 0..cycles {
        let a = rusqlite::Connection::open(&theirs).unwrap();
        let b = rusqlite::Connection::open(&theirs).unwrap();
        let v: i64 = a
            .query_row(
                "SELECT v FROM t WHERE id = ?",
                [1 + (k as i64 % rows)],
                |r| r.get(0),
            )
            .unwrap();
        let _ = v;
        b.execute(
            "UPDATE t SET v = v + 1 WHERE id = ?",
            [1 + (k as i64 % rows)],
        )
        .unwrap();
        drop(a);
        drop(b);
    }
    let theirs_t = start.elapsed().as_secs_f64() / cycles as f64;

    println!(
        "mode={mode} rows={rows} cycles={cycles}: ours {:.3} ms  sqlite {:.3} ms  ratio(sqlite/ours) {:.2}x",
        ours_t * 1e3,
        theirs_t * 1e3,
        theirs_t / ours_t
    );
    if phase {
        let n = cycles as f64 * 1e-3;
        println!(
            "  ours phases (ms/cycle): open2 {:.3} read {:.3} update {:.3} close2 {:.3}",
            t_open / n,
            t_read / n,
            t_upd / n,
            t_close / n
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
