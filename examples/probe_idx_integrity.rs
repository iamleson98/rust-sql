use rustqlite::storage::btree::Btree;
use rustqlite::storage::pager::Pager;
use rustqlite::types::Value;
use tempfile::NamedTempFile;

fn main() {
    let tmp = NamedTempFile::new().unwrap();
    let pager = Pager::open(tmp.path(), 256).unwrap();
    let mut ibt = Btree::create(&pager, true).unwrap();
    // Insert 10k sequential index entries (mimics backfill).
    for i in 1..=10_000i64 {
        let key = Value::Integer(i * 2).encode_order_key();
        ibt.insert_index(&key, i).unwrap();
    }
    println!("index pages: {}", pager.n_pages());

    // Full scan: count entries.
    let mut n = 0usize;
    ibt.scan_index(|_rowid, _key| { n += 1; true }).unwrap();
    println!("full scan entries: {} (expect 10000)", n);

    // Point lookups for every key.
    let mut missing = 0usize;
    for i in 1..=10_000i64 {
        let key = Value::Integer(i * 2).encode_order_key();
        let found = ibt.lookup_index(&key).unwrap();
        if found.is_empty() {
            missing += 1;
            if missing <= 5 {
                println!("  missing key val={}", i * 2);
            }
        }
    }
    println!("missing from lookup_index: {} (expect 0)", missing);

    // Also check mid-split inserts (random order).
    let tmp2 = NamedTempFile::new().unwrap();
    let pager2 = Pager::open(tmp2.path(), 256).unwrap();
    let mut ibt2 = Btree::create(&pager2, true).unwrap();
    let mut vals: Vec<i64> = (1..=10_000).map(|i| i * 2).collect();
    // pseudo-shuffle
    let mut seed = 42u64;
    for i in (1..vals.len()).rev() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let j = (seed % (i as u64 + 1)) as usize;
        vals.swap(i, j);
    }
    for (n_, v) in vals.iter().enumerate() {
        let key = Value::Integer(*v).encode_order_key();
        ibt2.insert_index(&key, (n_ + 1) as i64).unwrap();
    }
    let mut n2 = 0usize;
    ibt2.scan_index(|_rowid, _key| { n2 += 1; true }).unwrap();
    println!("random-order full scan: {} (expect 10000)", n2);
    let mut missing2 = 0usize;
    for v in (1..=10_000).map(|i| i * 2) {
        let key = Value::Integer(v).encode_order_key();
        if ibt2.lookup_index(&key).unwrap().is_empty() {
            missing2 += 1;
        }
    }
    println!("random-order missing: {} (expect 0)", missing2);
}
