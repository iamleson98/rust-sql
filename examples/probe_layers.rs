//! Layer-cost decomposition for the indexed point lookup: raw Btree calls
//! on the same file the Database produced (drop + reopen the Pager).
use rustqlite::storage::btree::Btree;
use rustqlite::storage::pager::Pager;
use rustqlite::types::Value;
use rustqlite::Database;
use std::time::Instant;

fn ns(d: std::time::Duration, n: u64) -> f64 {
    (d.as_secs() as f64 * 1e9 + d.as_nanos() as f64) / n as f64
}

fn main() {
    let path = "/tmp/probe_layers.db";
    let _ = std::fs::remove_file(path);
    let mut db = Database::open(path).unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10000i64 {
        db.execute(
            "INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [
                Value::Text(format!("user{}", i).into()),
                Value::Integer(i),
                Value::Real(i as f64 * 1.5),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
    db.flush().unwrap();
    drop(db);

    let pager = Pager::open(std::path::Path::new(path), 2000).unwrap();
    // roots: schema on page 0; find table & index roots by probing: the
    // table is created first (root 1), the index after inserts (later page).
    // Load roots by scanning schema: use Btree scan of page 0? Simpler:
    // brute-force identify by trying candidate roots.
    // Instead: reopen via Database once to read catalog... it's private.
    // Fallback: read schema rows via a table Btree on page 0.
    let mut sbt = Btree::new(&pager, 0, false);
    let mut roots: Vec<(String, u32)> = Vec::new();
    fn collect_roots(_rowid: i64, payload: &[u8], out: &mut Vec<(String, u32)>) -> bool {
        if let Ok(row) = rustqlite::storage::row_codec::decode_row(payload, 5, 0, None) {
            let name = row[1].as_text().to_string();
            let root = row[3].as_integer() as u32;
            out.push((name, root));
        }
        true
    }
    let mut sink = roots;
    let mut cb = |rid: i64, pl: &[u8]| collect_roots(rid, pl, &mut sink);
    sbt.scan_table_borrowed(&mut cb).unwrap();
    roots = sink;
    let troot = roots
        .iter()
        .find(|(n, _)| n == "t")
        .map(|(_, r)| *r)
        .unwrap();
    let iroot = roots
        .iter()
        .find(|(n, _)| n == "idx_val")
        .map(|(_, r)| *r)
        .unwrap();
    println!("table root={troot}, index root={iroot}, all={roots:?}");

    // Warm everything
    let mut key_buf = Vec::new();
    let mut ibt = Btree::new(&pager, iroot, true);
    let mut tbt = Btree::new(&pager, troot, false);
    for i in 0..200 {
        key_buf.clear();
        Value::Integer((i % 1000) * 2 + 2).encode_order_key_into(&mut key_buf);
        let _ = ibt.lookup_index(&key_buf).unwrap();
    }
    for i in 0..200 {
        let _ = tbt.lookup_table(((i % 1000) + 1) as i64).unwrap();
    }

    let n: u64 = 5000;
    let _ = n;

    // Layer 1: index seek only (encode + lookup_index)
    let start = Instant::now();
    for i in 0u64..n {
        key_buf.clear();
        Value::Integer((((i % 1000) + 1) * 2) as i64).encode_order_key_into(&mut key_buf);
        let _ = ibt.lookup_index(&key_buf).unwrap();
    }
    println!(
        "index seek (encode+lookup_index):  {:>7.1} ns",
        ns(start.elapsed(), n)
    );

    // Layer 1b: key encode only
    let start = Instant::now();
    for i in 0u64..n {
        key_buf.clear();
        Value::Integer((((i % 1000) + 1) * 2) as i64).encode_order_key_into(&mut key_buf);
    }
    println!(
        "key encode alone:                 {:>7.1} ns",
        ns(start.elapsed(), n)
    );

    // Layer 2: table fetch only
    let start = Instant::now();
    for i in 0u64..n {
        let _ = tbt.lookup_table(((i % 1000) + 1) as i64).unwrap();
    }
    println!(
        "table fetch (lookup_table):       {:>7.1} ns",
        ns(start.elapsed(), n)
    );

    // Layer 3: decode only (constant payload)
    let payload = match tbt.lookup_table(500).unwrap() {
        rustqlite::storage::btree::LookupResult::Found(p) => p,
        _ => panic!(),
    };
    let start = Instant::now();
    for _ in 0u64..n {
        let v: Vec<Value> =
            rustqlite::storage::row_codec::decode_row(&payload, 4, 500, Some(0)).unwrap();
        std::hint::black_box(&v);
    }
    println!(
        "decode_row alone (4 cols):        {:>7.1} ns",
        ns(start.elapsed(), n)
    );

    // Combined: what the fast path does minus query() overhead
    let start = Instant::now();
    for i in 0u64..n {
        key_buf.clear();
        Value::Integer((((i % 1000) + 1) * 2) as i64).encode_order_key_into(&mut key_buf);
        let rids = ibt.lookup_index(&key_buf).unwrap();
        if let Some(rid) = rids.first() {
            if let rustqlite::storage::btree::LookupResult::Found(p) =
                tbt.lookup_table(*rid).unwrap()
            {
                let v: Vec<Value> =
                    rustqlite::storage::row_codec::decode_row(&p, 4, 500, Some(0)).unwrap();
                std::hint::black_box(&v);
            }
        }
    }
    println!(
        "combined btree+decode:            {:>7.1} ns",
        ns(start.elapsed(), n)
    );
}
