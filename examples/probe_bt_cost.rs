//! Isolate B-tree lookup costs from API plumbing, with the real
//! benchmark shape (10k rows, index on val, rotating probe pattern).
use rustqlite::storage::btree::Btree;
use rustqlite::types::Value;
use rustqlite::Database;
use std::time::Instant;

fn ns(d: std::time::Duration) -> f64 {
    d.as_secs() as f64 * 1e9 + d.as_nanos() as f64
}

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    let ins = "INSERT INTO t (name, val, score) VALUES (?, ?, ?)";
    for i in 1..=10000i64 {
        db.execute(ins, [
            Value::Text(format!("name{}", i).into()),
            Value::Integer(i * 2),
            Value::Real(i as f64 * 1.5),
        ]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();

    // Find roots from the schema table (page 0).
    let pager = db.pager();
    let mut sbt = Btree::new(pager, 0, false);
    let mut t_root = 0u32;
    let mut idx_root = 0u32;
    sbt.scan_table_borrowed(|_rid, payload| {
        if let Ok(row) = rustqlite::storage::row_codec::decode_row(payload, 6, 0, None) {
            let name = row.get(1);
            let root = row.get(3).map(|v| v.as_integer());
            if let (Some(Value::Text(n)), Some(r)) = (name, root) {
                if n.as_str() == "t" { t_root = r as u32; }
                if n.as_str() == "idx_val" { idx_root = r as u32; }
            }
        }
        true
    }).unwrap();
    println!("t root={t_root} idx_val root={idx_root} pages={}", pager.n_pages());

    // Warm: run each op pattern once so hint caches exist.
    let n = 200_000i64;

    // (1) index lookups only — rotate vals 2..20000 like the benchmark.
    {
        let mut ibt = Btree::new(pager, idx_root, true);
        let mut out: Vec<i64> = Vec::with_capacity(8);
        let mut key = Vec::with_capacity(16);
        // warm
        for i in 1..=1000 {
            key.clear();
            Value::Integer(i * 2).encode_order_key_into(&mut key);
            ibt.lookup_index_into(&key, &mut out).unwrap();
        }
        let t = Instant::now();
        for i in 0..n {
            let v = ((i % 10000) + 1) * 2;
            key.clear();
            Value::Integer(v).encode_order_key_into(&mut key);
            ibt.lookup_index_into(&key, &mut out).unwrap();
        }
        println!("index lookup rotate-10k:   {:>7.1} ns/op", ns(t.elapsed()) / n as f64);
        // same-leaf pattern: vals 2..2600 (~4 leaves), higher hint-hit rate
        let t = Instant::now();
        for i in 0..n {
            let v = ((i % 1300) + 1) * 2;
            key.clear();
            Value::Integer(v).encode_order_key_into(&mut key);
            ibt.lookup_index_into(&key, &mut out).unwrap();
        }
        println!("index lookup rotate-1.3k:  {:>7.1} ns/op", ns(t.elapsed()) / n as f64);
        // single val (100% hint hit, same binary search position)
        let t = Instant::now();
        for _i in 0..n {
            key.clear();
            Value::Integer(6666).encode_order_key_into(&mut key);
            ibt.lookup_index_into(&key, &mut out).unwrap();
        }
        println!("index lookup same-key:     {:>7.1} ns/op", ns(t.elapsed()) / n as f64);
    }

    // (2) table lookups only — rotate rowids 1..10000.
    {
        let mut tbt = Btree::new(pager, t_root, false);
        let _cnt = 0usize;
        let _ = tbt.lookup_table_with(1, |_p| Ok::<_, rustqlite::Error>(0));
        let t = Instant::now();
        for i in 0..n {
            let rid = (i % 10000) + 1;
            let _ = tbt.lookup_table_with(rid, |_p| Ok::<_, rustqlite::Error>(0)).unwrap();
        }
        println!("table lookup alone:        {:>7.1} ns/op", ns(t.elapsed()) / n as f64);
    }

    // (3) key encoding alone.
    {
        let mut key = Vec::with_capacity(16);
        let t = Instant::now();
        for i in 0..n {
            key.clear();
            Value::Integer(((i % 10000) + 1) * 2).encode_order_key_into(&mut key);
        }
        println!("key encode alone:          {:>7.1} ns/op", ns(t.elapsed()) / n as f64);
    }

    // (4) full query for reference.
    {
        let sql = "SELECT id, name, score FROM t WHERE val = ?";
        for i in 1..=1000 {
            let _ = db.query(sql, [Value::Integer(i * 2)]).unwrap();
        }
        let t = Instant::now();
        for i in 0..n {
            let target = ((i % 10000) + 1) * 2;
            let _ = db.query(sql, [Value::Integer(target)]).unwrap();
        }
        println!("full query:                {:>7.1} ns/op", ns(t.elapsed()) / n as f64);
    }

}


