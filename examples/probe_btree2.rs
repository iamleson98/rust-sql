//! Microbenchmark v2: isolate per-append cost within a single page.
use rustqlite::storage::btree::Btree;
use rustqlite::storage::pager::Pager;
use std::time::Instant;

fn main() {
    let pager = Pager::open("/tmp/probe_btree2.db", 1024).unwrap();
    // Fill ONE page with as many appends as fit (no split).
    {
        let root = pager.allocate_page().unwrap();
        {
            let p = pager.get_page(root).unwrap();
            p.lock().init_leaf_table();
        }
        pager.note_dirty(root);
        let mut bt = Btree::new(&pager, root, false);
        let payload = vec![0u8; 8];
        // Warm.
        let mut hint = None;
        for i in 1..=100i64 {
            hint = bt.insert_table_append_hinted(i, &payload, hint).unwrap();
        }
        let start = Instant::now();
        for i in 101..=600i64 {
            hint = bt.insert_table_append_hinted(i, &payload, hint).unwrap();
        }
        let d = start.elapsed();
        println!(
            "500 single-page appends: {:?} ({:.0} ns/append)",
            d,
            d.as_nanos() as f64 / 500.0
        );

        // get_page alone.
        let start = Instant::now();
        for _ in 0..10_000 {
            let _p = pager.get_page(root).unwrap();
        }
        let d = start.elapsed();
        println!(
            "10k get_page hits: {:?} ({:.0} ns/call)",
            d,
            d.as_nanos() as f64 / 10_000.0
        );

        // lock + unlock alone.
        let p = pager.get_page(root).unwrap();
        let start = Instant::now();
        for _ in 0..10_000 {
            let g = p.lock();
            let _n = g.n_cells();
            drop(g);
        }
        let d = start.elapsed();
        println!(
            "10k lock+n_cells: {:?} ({:.0} ns/call)",
            d,
            d.as_nanos() as f64 / 10_000.0
        );
    }
}
