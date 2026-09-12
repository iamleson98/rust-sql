//! S17 SUM query memory attribution: serial vs parallel scan.

use rustqlite::{Database, Value};

fn mem() -> (f64, f64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let mut rss = 0.0;
    let mut hwm = 0.0;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            rss = rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<f64>()
                .unwrap()
                / 1024.0;
        }
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            hwm = rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<f64>()
                .unwrap()
                / 1024.0;
        }
    }
    (rss, hwm)
}

fn show(stage: &str) {
    let (rss, hwm) = mem();
    println!("{stage:44} rss={rss:6.1}MB hwm={hwm:6.1}MB");
}

fn build(path: &str) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let mut db = Database::open(path).unwrap();
    db.execute("PRAGMA journal_mode=WAL", []).unwrap();
    db.execute("PRAGMA synchronous=OFF", []).unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=1_000_000i64 {
        db.execute(
            "INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [
                Value::Text(format!("name{i}").into()),
                Value::Integer(i),
                Value::Real(i as f64 * 1.5),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
}

fn main() {
    let path = "/tmp/s17q.rq.db";
    build(path);
    show("after build (baseline)");

    // Serial scan: parallel disabled.
    {
        let mut db = Database::open(path).unwrap();
        db.execute("PRAGMA parallel_scan=0", []).unwrap();
        let out = db.query("SELECT SUM(val) FROM t", []).unwrap();
        println!("serial sum = {:?}", out[0][0]);
        show("after SUM (serial)");
        let out = db.query("SELECT SUM(val) FROM t", []).unwrap();
        println!("serial sum2 = {:?}", out[0][0]);
        show("after SUM again (serial)");
    }

    // Parallel scan (default).
    {
        let db = Database::open(path).unwrap();
        let out = db.query("SELECT SUM(val) FROM t", []).unwrap();
        println!("parallel sum = {:?}", out[0][0]);
        show("after SUM (parallel)");
        let _out = db.query("SELECT SUM(val) FROM t", []).unwrap();
        show("after SUM again (parallel)");
    }
    // Raw walk (no executor): how much does a bare b-tree walk retain?
    {
        let db = Database::open(path).unwrap();
        let mut n = 0i64;
        let mut stmt = db.prepare("SELECT val FROM t").unwrap();
        while stmt.step().unwrap() == rustqlite::StepResult::Row {
            n += 1;
        }
        drop(stmt);
        println!("walked {n} rows");
        show("after full rowid walk (step path)");
    }
}
