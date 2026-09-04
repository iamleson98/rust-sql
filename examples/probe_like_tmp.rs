use rustqlite::{Database, Value};
use std::time::Instant;
fn bench(db: &Database, sql: &str, iters: usize) -> std::time::Duration {
    for _ in 0..3 {
        let _ = db.query(sql, []).unwrap();
    }
    // best of 5 rounds (like CI bench gate)
    let mut best = std::time::Duration::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        for _ in 0..iters {
            let _ = db.query(sql, []).unwrap();
        }
        best = best.min(t.elapsed() / iters as u32);
    }
    best
}
fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE z (id INTEGER PRIMARY KEY, data BLOB)", [])
        .unwrap();
    // S09 shape: 4 DISTINCT 64KB variants (no dedup possible)
    let blobs: Vec<Vec<u8>> = (0..4)
        .map(|k| {
            let mut v = vec![b'b'; 65536];
            v[0] = b'0' + k;
            v
        })
        .collect();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=100i64 {
        db.execute(
            "INSERT INTO z (id, data) VALUES (?, ?)",
            [
                Value::Integer(i),
                Value::Blob(blobs[(i % 4) as usize].clone()),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    println!(
        "rq blob select 100x64KB (best-of-5): {:?}",
        bench(&db, "SELECT data FROM z", 5)
    );
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute("CREATE TABLE z (id INTEGER PRIMARY KEY, data BLOB)", [])
        .unwrap();
    conn.execute("BEGIN", []).unwrap();
    for i in 1..=100i64 {
        conn.execute(
            "INSERT INTO z VALUES (?1, ?2)",
            rusqlite::params![i, blobs[(i % 4) as usize]],
        )
        .unwrap();
    }
    conn.execute("COMMIT", []).unwrap();
    let mut best = std::time::Duration::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        for _ in 0..5 {
            let mut s = conn.prepare("SELECT data FROM z").unwrap();
            let mut r = s.query([]).unwrap();
            while let Some(row) = r.next().unwrap() {
                let _: Vec<u8> = row.get(0).unwrap();
            }
        }
        best = best.min(t.elapsed() / 5);
    }
    println!("sq blob select 100x64KB (best-of-5): {:?}", best);
}
