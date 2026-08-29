use rustqlite::storage::btree::Btree;
use rustqlite::storage::pager::Pager;
use rustqlite::types::Value;
use std::time::Instant;

fn main() {
    // Build a db via SQL (so shapes match the bench), then time raw Btree ops.
    let path = "/tmp/probe_idx_time.db";
    let _ = std::fs::remove_file(path);
    let mut db = rustqlite::Database::open(path).unwrap();
    db.set_deferred_flush(true);
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10_000i64 {
        db.execute("INSERT INTO t (name, val, score) VALUES (?, ?, ?)", [
            Value::Text(format!("name{}", i)),
            Value::Integer(i * 2),
            Value::Real(i as f64 * 1.5),
        ]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
    drop(db);

    // Raw pager access.
    let pager = Pager::open(path, 2048).unwrap();
    // Find roots via schema.
    let mut table_root = 0u32; let mut idx_root = 0u32;
    {
        let mut sbt = Btree::new(&pager, 0, false);
        sbt.scan_table_borrowed(|_r, payload| {
            if let Ok(row) = rustqlite::storage::row_codec::decode_row(payload, 5, 0, None) {
                match (&row[1], &row[3]) {
                    (Value::Text(n), Value::Integer(rp)) if n == "t" => table_root = *rp as u32,
                    (Value::Text(n), Value::Integer(rp)) if n == "idx_val" => idx_root = *rp as u32,
                    _ => {}
                }
            }
            true
        }).unwrap();
    }
    println!("table_root={} idx_root={}", table_root, idx_root);

    // Time raw index lookups.
    let n = 5000;
    let start = Instant::now();
    let mut hits = 0usize;
    for i in 0..n {
        let key = Value::Integer(((i % 1000) as i64 + 1) * 2).encode_order_key();
        let mut ibt = Btree::new(&pager, idx_root, true);
        let rids = ibt.lookup_index(&key).unwrap();
        hits += rids.len();
    }
    let d1 = start.elapsed();
    println!("raw index lookup:  {:?}/op (hits={})", d1.as_nanos() / (n as u128), hits);

    // Time raw index lookup + table fetch.
    let start = Instant::now();
    let mut rows = 0usize;
    for i in 0..n {
        let key = Value::Integer(((i % 1000) as i64 + 1) * 2).encode_order_key();
        let mut ibt = Btree::new(&pager, idx_root, true);
        let rids = ibt.lookup_index(&key).unwrap();
        for rid in rids {
            let mut tbt = Btree::new(&pager, table_root, false);
            if let rustqlite::storage::btree::LookupResult::Found(_p) = tbt.lookup_table(rid).unwrap() {
                rows += 1;
            }
        }
    }
    let d2 = start.elapsed();
    println!("index+table fetch: {:?}/op (rows={})", d2.as_nanos() / (n as u128), rows);

    // Raw table lookup for comparison.
    let start = Instant::now();
    let mut found_t = 0usize;
    for i in 0..n {
        let mut tbt = Btree::new(&pager, table_root, false);
        if let rustqlite::storage::btree::LookupResult::Found(_p) = tbt.lookup_table(1 + (i % 10000)).unwrap() {
            found_t += 1;
        }
    }
    let dt = start.elapsed();
    println!("raw table lookup: {:?}/op (found={})", dt.as_nanos() / (n as u128), found_t);

    // Full SQL path for comparison.
    let mut db2 = rustqlite::Database::open(path).unwrap();
    let sql = "SELECT id, name, score FROM t WHERE val = ?";
    let _ = db2.query(sql, [Value::Integer(2)]).unwrap();
    let start = Instant::now();
    for i in 0..n {
        let _ = db2.query(sql, [Value::Integer(((i % 1000) as i64 + 1) * 2)]).unwrap();
    }
    let d3 = start.elapsed();
    println!("SQL query path:    {:?}/op", d3.as_nanos() / (n as u128));
}
