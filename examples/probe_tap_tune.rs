//! Tune the allocator settle tap: which allocation pattern actually
//! absorbs the post-storm wake?
use rustqlite::{Database, Value};
use std::time::Instant;

fn fresh_db() -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.set_deferred_flush(true);
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", []).unwrap();
    let sql = "INSERT INTO t (name, val, score) VALUES (?, ?, ?)";
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10000i64 {
        db.execute(sql, [
            Value::Text(format!("name{}", i).into()),
            Value::Integer(i * 2),
            Value::Real(i as f64 * 1.5),
        ]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db
}

fn main() {
    // (a) no tap
    {
        let mut db = fresh_db();
        let t0 = Instant::now();
        let _ = db.query("SELECT name, val, score FROM t WHERE id = ?", [Value::Integer(1)]).unwrap();
        println!("no tap                    {:>7.1} µs", t0.elapsed().as_secs_f64() * 1e6);
    }
    // (b) small-class tap (the one that worked in probe_alloc_wake)
    {
        let mut db = fresh_db();
        let t0 = Instant::now();
        let mut sink: Vec<Vec<u8>> = Vec::with_capacity(64);
        for i in 0..64u32 {
            sink.push(vec![0u8; ((i * 13) % 128 + 8) as usize]);
        }
        drop(sink);
        let t1 = Instant::now();
        let _ = db.query("SELECT name, val, score FROM t WHERE id = ?", [Value::Integer(1)]).unwrap();
        println!("small tap (64, 8-136B)    {:>7.1} µs (tap {:.1} µs)",
            t1.elapsed().as_secs_f64() * 1e6, t0.elapsed().as_secs_f64() * 1e6);
    }
    // (c) wide tap: 256 allocs, sizes 8..1024
    {
        let mut db = fresh_db();
        let t0 = Instant::now();
        let mut sink: Vec<Vec<u8>> = Vec::with_capacity(256);
        for i in 0..256u32 {
            sink.push(vec![0u8; ((i * 29) % 1016 + 8) as usize]);
        }
        drop(sink);
        let t1 = Instant::now();
        let _ = db.query("SELECT name, val, score FROM t WHERE id = ?", [Value::Integer(1)]).unwrap();
        println!("wide tap (256, 8-1024B)   {:>7.1} µs (tap {:.1} µs)",
            t1.elapsed().as_secs_f64() * 1e6, t0.elapsed().as_secs_f64() * 1e6);
    }
    // (d) touch tap: 256 allocs + WRITE the bytes (page faults)
    {
        let mut db = fresh_db();
        let t0 = Instant::now();
        let mut sink: Vec<Vec<u8>> = Vec::with_capacity(256);
        for i in 0..256u32 {
            let mut v = vec![0u8; ((i * 29) % 1016 + 8) as usize];
            let last = v.len() - 1;
            v[0] = 1;
            v[last] = 2;
            sink.push(v);
        }
        drop(sink);
        let t1 = Instant::now();
        let _ = db.query("SELECT name, val, score FROM t WHERE id = ?", [Value::Integer(1)]).unwrap();
        println!("touch tap (256)           {:>7.1} µs (tap {:.1} µs)",
            t1.elapsed().as_secs_f64() * 1e6, t0.elapsed().as_secs_f64() * 1e6);
    }
    // (e) parse-warmup only (one allocation-light execute? just call the
    // statement cache with the query — actually run a dummy COUNT first)
    {
        let mut db = fresh_db();
        let t0 = Instant::now();
        let _ = db.query("SELECT COUNT(*) FROM t", []).unwrap();
        let t1 = Instant::now();
        let _ = db.query("SELECT name, val, score FROM t WHERE id = ?", [Value::Integer(1)]).unwrap();
        println!("COUNT first               {:>7.1} µs (count {:.1} µs)",
            t1.elapsed().as_secs_f64() * 1e6, t0.elapsed().as_secs_f64() * 1e6);
    }
}
