//! Microbenchmark: raw index-tree appends (hinted) vs table appends.
use rustqlite::storage::btree::Btree;
use rustqlite::storage::pager::Pager;

fn main() {
    let pager = Pager::open("/tmp/probe_btree_append.db", 1024).unwrap();
    // Table tree appends.
    {
        let root = pager.allocate_page().unwrap();
        {
            let p = pager.get_page(root).unwrap();
            p.lock().init_leaf_table();
        }
        pager.note_dirty(root);
        let mut bt = Btree::new(&pager, root, false);
        let payload = vec![0u8; 24];
        let start = std::time::Instant::now();
        let mut hint = None;
        for i in 1..=10_000i64 {
            hint = bt.insert_table_append_hinted(i, &payload, hint).unwrap();
        }
        let d = start.elapsed();
        println!("table appends 10k: {:?} ({:.0} ns/row)", d, d.as_nanos() as f64 / 10_000.0);
    }
    // Index tree appends.
    {
        let root = pager.allocate_page().unwrap();
        {
            let p = pager.get_page(root).unwrap();
            p.lock().init_leaf_index();
        }
        pager.note_dirty(root);
        let mut bt = Btree::new(&pager, root, true);
        let start = std::time::Instant::now();
        let mut hint = None;
        for i in 1..=10_000i64 {
            // order-key encoded INTEGER: 0x01 + 8-byte BE order key
            let mut key = Vec::with_capacity(9);
            rustqlite::types::Value::Integer(i).encode_order_key_into(&mut key);
            hint = bt.insert_index_append_hinted(&key, i, hint).unwrap();
        }
        let d = start.elapsed();
        println!("index appends 10k: {:?} ({:.0} ns/row)", d, d.as_nanos() as f64 / 10_000.0);
        // Sanity: look up a few keys.
        let mut key = Vec::new();
        rustqlite::types::Value::Integer(5_000).encode_order_key_into(&mut key);
        let r = bt.lookup_index(&key).unwrap();
        println!("lookup 5000 -> {:?} (expect [5000])", r);
        let mut cnt = 0usize;
        bt.scan_index(|_rid, _k| { cnt += 1; true }).unwrap();
        println!("index entry count: {} (expect 10000)", cnt);
    }
}
