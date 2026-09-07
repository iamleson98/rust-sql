//! S11 forensics: where do 3MB go on a 5000-literal IN list?
use rustqlite::{Database, Value};

fn rss() -> f64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap();
    for l in s.lines() {
        if let Some(r) = l.strip_prefix("VmRSS:") {
            return r
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

fn lcg(s: &mut u64) -> u64 {
    *s = s
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *s >> 33
}

fn main() {
    let rows = 100_000i64;
    let mut seed = 0xABCDEF01u64;
    let ids: Vec<i64> = (0..5000)
        .map(|i| {
            if i % 5 == 0 {
                1_000_000 + (lcg(&mut seed) % 100_000) as i64
            } else {
                (lcg(&mut seed) % rows as u64) as i64 + 1
            }
        })
        .collect();
    let list = ids
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT COUNT(*) FROM t WHERE val IN ({list})");

    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
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
    println!("after build: rss={:.1}", rss());
    println!("sql text len: {} bytes", sql.len());

    let t = std::time::Instant::now();
    let out = db.query(&sql, []).unwrap();
    println!(
        "count={:?} time={:.1}ms rss={:.1}",
        out[0][0].clone(),
        t.elapsed().as_secs_f64() * 1000.0,
        rss()
    );
    // again
    let t = std::time::Instant::now();
    let out = db.query(&sql, []).unwrap();
    println!(
        "2nd: count={:?} time={:.1}ms rss={:.1}",
        out[0][0].clone(),
        t.elapsed().as_secs_f64() * 1000.0,
        rss()
    );
}
