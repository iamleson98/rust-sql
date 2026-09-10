//! S09 regression probe: 1000 x 64KB blobs scanned through the step path
//! (the torture S09 shape). Overflow rows must route through the
//! materialized path's targeted span gather — NOT the arena copy.
use rustqlite::{Database, StepResult, Value};
use std::time::Instant;

fn main() {
    let rows = 1000i64;
    let iters = 5;
    let blob: Vec<u8> = vec![b'b'; 65_536];
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE z (id INTEGER PRIMARY KEY, data BLOB)", [])
        .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=rows {
        db.execute(
            "INSERT INTO z (id, data) VALUES (?, ?)",
            [Value::Integer(i), Value::Blob(blob.clone())],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let mut acc = 0usize;
    let t = Instant::now();
    for _ in 0..iters {
        let mut stmt = db.prepare("SELECT data FROM z").unwrap();
        while let Ok(StepResult::Row) = stmt.step() {
            if let Some(Value::Blob(b)) = stmt.row().and_then(|r| r.first()) {
                acc = acc.wrapping_add(b.len());
            }
        }
    }
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    println!("sink: {}", acc % 1000);
    println!("S09-shape scan: {:.1} ms ({} x {} rows)", ms, iters, rows);
    println!("per-row: {:.1} us", ms * 1000.0 / (iters * rows) as f64);
    sqlite_side(rows, iters as usize);
}

fn sqlite_side(rows: i64, iters: usize) {
    use rusqlite::params;
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA journal_mode=MEMORY; PRAGMA synchronous=OFF;")
        .unwrap();
    conn.execute("CREATE TABLE z (id INTEGER PRIMARY KEY, data BLOB)", [])
        .unwrap();
    let blob: Vec<u8> = vec![b'b'; 65_536];
    conn.execute("BEGIN", []).unwrap();
    for i in 1..=rows {
        conn.execute("INSERT INTO z (id, data) VALUES (?1, ?2)", params![i, blob])
            .unwrap();
    }
    conn.execute("COMMIT", []).unwrap();
    let mut acc = 0usize;
    let t = Instant::now();
    for _ in 0..iters {
        let mut stmt = conn.prepare("SELECT data FROM z").unwrap();
        let mut rows_it = stmt.query([]).unwrap();
        while let Some(r) = rows_it.next().unwrap() {
            let b: Vec<u8> = r.get(0).unwrap();
            acc = acc.wrapping_add(b.len());
        }
    }
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    println!("sqlite sink: {}", acc % 1000);
    println!("S09-shape sqlite scan: {:.1} ms", ms);
    println!(
        "sqlite per-row: {:.1} us",
        ms * 1000.0 / (iters * rows as usize) as f64
    );
}
