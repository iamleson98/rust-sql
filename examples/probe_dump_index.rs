use rustqlite::storage::btree::{Btree, Cell};
use rustqlite::storage::pager::Pager;

fn main() {
    // probe_idx_sql3.db left behind by the previous probe.
    let pager = Pager::open("/tmp/probe_idx_sql3.db", 2048).unwrap();
    let page_size = pager.page_size();
    // Read schema table (page 0) to find idx_val's root page.
    let mut sbt = Btree::new(&pager, 0, false);
    let mut idx_root = 0u32;
    sbt.scan_table_borrowed(|_rowid, payload| {
        let s = String::from_utf8_lossy(payload);
        if s.contains("idx_val") && s.contains("CREATE INDEX") {
            // Schema row: kind, name, tbl, rootpage, sql — find rootpage by
            // scanning the decoded values.
            if let Ok(row) = rustqlite::storage::row_codec::decode_row(payload, 5, 0, None) {
                if let rustqlite::types::Value::Text(n) = &row[1] {
                    if n == "idx_val" {
                        if let rustqlite::types::Value::Integer(rp) = row[3] {
                            idx_root = rp as u32;
                        }
                    }
                }
            }
        }
        true
    }).unwrap();
    println!("idx_val root page: {}", idx_root);
    println!("total pages: {}", pager.n_pages());

    // Dump the root page structure.
    let page = pager.get_page(idx_root).unwrap();
    let borrowed = page.lock();
    let pt = borrowed.page_type().unwrap();
    let n = borrowed.n_cells();
    println!("root type: {:?}, n_cells: {}, right_most: {}", pt, n, borrowed.right_most_pointer());
    for i in 0..n {
        let cell_ptr = borrowed.cell_pointer(i) as usize;
        let c = Cell::decode(&borrowed.data[cell_ptr..], pt, page_size).unwrap();
        match c {
            Cell::IndexInterior { left_child, key, rowid } => {
                println!("  cell[{}]: left_child={}, sep_rowid={}, sep_key={:02x?}", i, left_child, rowid, &key[..key.len().min(4)]);
            }
            Cell::IndexLeaf { key, rowid } => {
                println!("  leaf cell[{}]: rowid={}, key={:02x?}...", i, rowid, &key[..key.len().min(4)]);
            }
            _ => println!("  cell[{}]: other", i),
        }
    }
    drop(borrowed);

    // Now count entries reachable from this root + check a specific lookup.
    let mut ibt = Btree::new(&pager, idx_root, true);
    let mut total = 0usize;
    ibt.scan_index(|_r, _k| { total += 1; true }).unwrap();
    println!("entries reachable from root: {}", total);
    let key = rustqlite::types::Value::Integer(1236).encode_order_key();
    let found = ibt.lookup_index(&key).unwrap();
    println!("lookup val=1236: {} rowids {:?}", found.len(), &found[..found.len().min(3)]);
}
