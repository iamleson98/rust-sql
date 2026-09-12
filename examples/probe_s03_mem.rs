//! S03 GROUP BY memory attribution: in-memory DB (matches torture S03),
//! RSS checkpoints around build + 3x GROUP BY, plus parallel off variant.

use rustqlite::{Database, StepResult, Value};

fn rss() -> f64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<f64>()
                .unwrap()
                / 1024.0;
        }
    }
    0.0
}

fn main() {
    let serial = std::env::args().nth(1) == Some("serial".into());
    let mut db = Database::open_in_memory().unwrap();
    if serial {
        db.execute("PRAGMA parallel_scan=0", []).unwrap();
    }
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
    println!("after 1M build (serial={serial}) rss={:.1}MB", rss());
    for i in 0..3 {
        let mut stmt = db
            .prepare("SELECT val/10, COUNT(*) FROM t GROUP BY val/10")
            .unwrap();
        let mut n = 0i64;
        while stmt.step().unwrap() == StepResult::Row {
            n += 1;
        }
        drop(stmt);
        println!("after GROUP BY #{i} groups={n} rss={:.1}MB", rss());
    }
}
