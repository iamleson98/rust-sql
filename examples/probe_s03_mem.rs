//! S03 memory forensics: where does GROUP BY 100k-bucket memory go?

use rustqlite::{Database, Value};

fn rss_mb() -> f64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: f64 = rest.trim().trim_end_matches("kB").trim().parse().unwrap();
            return kb / 1024.0;
        }
    }
    0.0
}

fn hwm_mb() -> f64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kb: f64 = rest.trim().trim_end_matches("kB").trim().parse().unwrap();
            return kb / 1024.0;
        }
    }
    0.0
}

fn main() {
    let parallel = std::env::args().nth(1).map(|s| s != "off").unwrap_or(true);
    let rows: i64 = 1_000_000;

    let mut db = Database::open_in_memory().unwrap();
    if !parallel {
        db.execute("PRAGMA parallel_scan=0", []).unwrap();
    }
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    println!(
        "after open+create: rss={:.1}MB hwm={:.1}MB",
        rss_mb(),
        hwm_mb()
    );

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
    println!(
        "after build 1M rows: rss={:.1}MB hwm={:.1}MB",
        rss_mb(),
        hwm_mb()
    );

    let t = std::time::Instant::now();
    let sql = "SELECT val/10, COUNT(*) FROM t GROUP BY val/10";
    let mut stmt = db.prepare(sql).unwrap();
    let mut n_groups = 0usize;
    let mut acc = 0i64;
    while let Ok(rustqlite::StepResult::Row) = stmt.step() {
        n_groups += 1;
        if let Some(r) = stmt.row() {
            if let Some(Value::Integer(c)) = r.get(1) {
                acc = acc.wrapping_add(*c);
            }
        }
    }
    drop(stmt);
    println!(
        "after GROUP BY (groups={n_groups}, acc={acc}): time={:.0}ms rss={:.1}MB hwm={:.1}MB",
        t.elapsed().as_secs_f64() * 1000.0,
        rss_mb(),
        hwm_mb()
    );

    // Again — does state accumulate across statements?
    let t = std::time::Instant::now();
    let mut stmt = db.prepare(sql).unwrap();
    let mut n2 = 0usize;
    while let Ok(rustqlite::StepResult::Row) = stmt.step() {
        n2 += 1;
    }
    drop(stmt);
    println!(
        "after 2nd GROUP BY (groups={n2}): time={:.0}ms rss={:.1}MB hwm={:.1}MB",
        t.elapsed().as_secs_f64() * 1000.0,
        rss_mb(),
        hwm_mb()
    );
}
