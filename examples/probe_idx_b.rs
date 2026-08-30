//! Dump the idx_cat index tree for the failing shape (8KB pages, n=1000)
//! and trace which leaves the equality scan for 'b' visits.
use rustqlite::types::Value;
use rustqlite::Database;
use rustqlite::storage::btree::Btree;
use rustqlite::storage::pager::Pager;
use rustqlite::storage::page::PageType;

fn main() {
    let path = "/tmp/probe_idx_b.db";
    let _ = std::fs::remove_file(path);
    let mut db = Database::open(path).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, cat TEXT, val INTEGER)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=1000i64 {
        let cat = if i % 3 == 0 { "a" } else if i % 3 == 1 { "b" } else { "c" };
        db.execute("INSERT INTO t (cat, val) VALUES (?, ?)",
            [Value::Text(cat.into()), Value::Integer(i)]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db.execute("CREATE INDEX idx_cat ON t(cat)", []).unwrap();
    drop(db);

    let pager = Pager::open(path, 0).unwrap();
    // Find idx_cat root via schema.
    let mut sbt = Btree::new(&pager, 0, false);
    let mut idx_root = 0u32;
    sbt.scan_table_borrowed(|_rowid, payload| {
        if let Ok(row) = rustqlite::storage::row_codec::decode_row(payload, 6, 0, None) {
            eprintln!("schema row: {:?}", row);
            for v in &row {
                if let Value::Text(n) = v {
                    if n.as_str() == "idx_cat" {
                        if let Some(Value::Integer(rp)) = row.iter().find(|v| matches!(v, Value::Integer(_))) {
                            idx_root = *rp as u32;
                        }
                    }
                }
            }
        }
        true
    }).unwrap();
    println!("idx_cat root = {}   total pages = {}", idx_root, pager.n_pages());

    // Walk the tree, printing each leaf's first/last cell keys.
    fn walk(pager: &Pager, page_id: u32, depth: usize, out: &mut Vec<(u32, String, String, usize)>) {
        let page = pager.get_page(page_id).unwrap();
        let borrowed = page.lock();
        let pt = borrowed.page_type().unwrap();
        let n = borrowed.n_cells() as usize;
        match pt {
            PageType::LeafIndex => {
                let first = borrowed.cell_pointer(0) as usize;
                let last = borrowed.cell_pointer((n - 1) as u16) as usize;
                let f = rustqlite::storage::btree::Cell::decode(&borrowed.data[first..], pt).unwrap();
                let l = rustqlite::storage::btree::Cell::decode(&borrowed.data[last..], pt).unwrap();
                let fk = String::from_utf8_lossy(f.index_key());
                let lk = String::from_utf8_lossy(l.index_key());
                out.push((page_id, fk.to_string(), lk.to_string(), n));
                eprintln!("{}leaf p{} n={} first={:?} last={:?}",
                    "  ".repeat(depth), page_id, n, &f.index_key()[..f.index_key().len().min(12)], &l.index_key()[..l.index_key().len().min(12)]);
            }
            PageType::InteriorIndex => {
                eprintln!("{}interior p{} n={} right={}", "  ".repeat(depth), page_id, n, borrowed.right_most_pointer());
                for i in 0..n {
                    let cell_ptr = borrowed.cell_pointer(i as u16) as usize;
                    let c = rustqlite::storage::btree::Cell::decode(&borrowed.data[cell_ptr..], pt).unwrap();
                    eprintln!("{}  cell{}: left_child={} key_bytes={:?} rowid={}",
                        "  ".repeat(depth), i, c.left_child(),
                        &c.index_key()[..c.index_key().len().min(10)], c.key());
                    if c.left_child() > 0 && c.left_child() != page_id {
                        let child = c.left_child();
                        drop(borrowed);
                        walk(pager, child, depth + 1, out);
                        let borrowed2 = pager.get_page(page_id).unwrap();
                        let _ = borrowed2.lock();
                    }
                }
                let right = borrowed.right_most_pointer();
                if right > 0 {
                    walk(pager, right, depth + 1, out);
                }
            }
            _ => eprintln!("{}?? p{} type {:?}", "  ".repeat(depth), page_id, pt),
        }
    }

    let mut leaves = Vec::new();
    walk(&pager, idx_root, 0, &mut leaves);

    // Count 'b'-prefixed keys per leaf.
    println!("\n--- 'b' entries per leaf ---");
    let mut total_b = 0usize;
    for (pid, _f, _l, _n) in &leaves {
        let page = pager.get_page(*pid).unwrap();
        let borrowed = page.lock();
        let n = borrowed.n_cells() as usize;
        let mut cnt = 0;
        for i in 0..n {
            let cell_ptr = borrowed.cell_pointer(i as u16) as usize;
            let c = rustqlite::storage::btree::Cell::decode(&borrowed.data[cell_ptr..], PageType::LeafIndex).unwrap();
            // cell key = order_key(cat) + ... — text key starts with 0x02 len prefix
            if c.index_key().len() > 6 && c.index_key()[5] == b'b' && c.index_key()[0] == 0x02 {
                cnt += 1;
            }
        }
        total_b += cnt;
        println!("leaf p{}: {} b-entries (n_cells={})", pid, cnt, n);
    }
    println!("TOTAL b entries in tree: {} (expect 334)", total_b);
}
