use rustqlite::storage::btree::Btree;
use rustqlite::storage::pager::Pager;
use tempfile::NamedTempFile;

fn main() {
    let tmp = NamedTempFile::new().unwrap();
    let pager = Pager::open(tmp.path(), 2048).unwrap();
    // Build a table with 10k rows.
    let mut tbt = Btree::create(&pager, false).unwrap();
    let payload = [7u8; 24];
    for i in 1..=10_000i64 {
        tbt.insert_table(i, &payload).unwrap();
    }
    println!("table pages: {}", pager.n_pages());

    // Now CREATE INDEX: scan table while inserting into index tree.
    let idx_root = pager.allocate_page().unwrap();
    {
        let page = pager.get_page(idx_root).unwrap();
        page.lock().init_leaf_index();
    }
    let mut ibt = Btree::new(&pager, idx_root, true);
    let mut scanned = 0usize;
    let mut inserted = 0usize;
    let mut err: Option<String> = None;
    let mut scan_bt = Btree::new(&pager, tbt.root, false);
    scan_bt.scan_table_borrowed(|rowid, _payload| {
        scanned += 1;
        let key = [(rowid * 2) as u8; 9];
        match ibt.insert_index(&key, rowid) {
            Ok(()) => inserted += 1,
            Err(e) => {
                err = Some(format!("{:?}", e));
                return false;
            }
        }
        true
    }).unwrap();
    println!("scanned: {}, inserted: {}, err: {:?}", scanned, inserted, err);
    println!("index pages: {}, index root: {}", pager.n_pages(), ibt.root);
    let mut n = 0usize;
    ibt.scan_index(|_r, _k| { n += 1; true }).unwrap();
    println!("index entries: {} (expect 10000)", n);
}
