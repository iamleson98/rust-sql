//! S17 memory forensics: file build peak vs query peak.

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

fn main() {
    let rows: i64 = 1_000_000;
    let path = "/tmp/s17probe.rq.db";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{path}-wal"));

    show("start");
    {
        let mut db = Database::open(path).unwrap();
        db.execute("PRAGMA journal_mode=WAL", []).unwrap();
        db.execute("PRAGMA synchronous=OFF", []).unwrap();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
            [],
        )
        .unwrap();
        show("after open+create");
        db.execute("BEGIN", []).unwrap();
        for i in 1..=rows {
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
        show("after 1M-row insert txn (file)");
    }
    show("after drop Database");

    let t = std::time::Instant::now();
    let db = Database::open(path).unwrap();
    show("after reopen");
    let out = db.query("SELECT SUM(val) FROM t", []).unwrap();
    let v = &out[0][0];
    println!("sum = {v:?}");
    show("after SUM query");
    let db2 = Database::open(path).unwrap();
    let out2 = db2.query("SELECT COUNT(*) FROM t", []).unwrap();
    println!("count = {:?}", out2[0][0]);
    show("after 2nd open+COUNT");
    println!("query time: {:.1}ms", t.elapsed().as_secs_f64() * 1000.0);
}
