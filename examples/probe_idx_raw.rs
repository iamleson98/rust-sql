//! Minimal raw dump of the corrupted index root (page 6) at 8KB pages.
use rustqlite::storage::page::PageType;
use rustqlite::storage::pager::Pager;

fn main() {
    let path = "/tmp/probe_idx_b2.db";
    // Build the failing DB via the public API.
    let _ = std::fs::remove_file(path);
    {
        use rustqlite::types::Value;
        let mut db = rustqlite::Database::open(path).unwrap();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, cat TEXT, val INTEGER)",
            [],
        )
        .unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 1..=1000i64 {
            let cat = if i % 3 == 0 {
                "a"
            } else if i % 3 == 1 {
                "b"
            } else {
                "c"
            };
            db.execute(
                "INSERT INTO t (cat, val) VALUES (?, ?)",
                [Value::Text(cat.into()), Value::Integer(i)],
            )
            .unwrap();
        }
        db.execute("COMMIT", []).unwrap();
        db.execute("CREATE INDEX idx_cat ON t(cat)", []).unwrap();
    }

    let pager = Pager::open(path, 0).unwrap();
    let page_size = pager.page_size();
    eprintln!("total pages = {}", pager.n_pages());
    for pid in 0..pager.n_pages() {
        let page = pager.get_page(pid).unwrap();
        let b = page.lock();
        let pt = match b.page_type() {
            Ok(t) => t,
            Err(_) => {
                eprintln!("page {}: <bad type>", pid);
                continue;
            }
        };
        eprintln!(
            "page {}: {:?} n_cells={} right={}",
            pid,
            pt,
            b.n_cells(),
            b.right_most_pointer()
        );
        // For interior pages: dump each cell's left_child + key prefix.
        if matches!(pt, PageType::InteriorIndex | PageType::InteriorTable) {
            let n = b.n_cells();
            for i in 0..n {
                let ptr = b.cell_pointer(i) as usize;
                let c =
                    rustqlite::storage::btree::Cell::decode(&b.data[ptr..], pt, page_size).unwrap();
                let keyhex: String = c.index_key().iter().map(|x| format!("{:02x}", x)).collect();
                let keytxt = if c.index_key().len() > 6 {
                    String::from_utf8_lossy(&c.index_key()[6..]).to_string()
                } else {
                    "<table>".to_string()
                };
                eprintln!(
                    "  cell{}: left_child={} sep_key(hex)={} sep_key(txt)={:?} sep_rowid={}",
                    i,
                    c.left_child(),
                    keyhex,
                    keytxt,
                    c.key()
                );
            }
        }
        // For index leaves: first + last cell keys.
        if pt == PageType::LeafIndex && b.n_cells() > 0 {
            let n = b.n_cells();
            for (label, idx) in [("first", 0u16), ("last", n - 1)] {
                let ptr = b.cell_pointer(idx) as usize;
                let c =
                    rustqlite::storage::btree::Cell::decode(&b.data[ptr..], pt, page_size).unwrap();
                let keytxt = if c.index_key().len() > 6 {
                    String::from_utf8_lossy(&c.index_key()[6..]).to_string()
                } else {
                    "<empty>".to_string()
                };
                eprintln!("  {} cell: key={:?} rowid={}", label, keytxt, c.key());
            }
            // Count b's in this leaf.
            let mut cnt = 0;
            for i in 0..n {
                let ptr = b.cell_pointer(i) as usize;
                let c =
                    rustqlite::storage::btree::Cell::decode(&b.data[ptr..], pt, page_size).unwrap();
                if c.index_key().len() > 6 && c.index_key()[6] == b'b' {
                    cnt += 1;
                }
            }
            eprintln!("  b-count = {}", cnt);
        }
    }
}
