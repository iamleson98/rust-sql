//! dbstat virtual-table tests — the eponymous `FROM dbstat` /
//! `FROM dbstat('t')` page inventory (SQLite's DBSTAT_VTAB):
//!
//! - Both the bare eponymous form and the table-function form resolve
//!   (planner + namecheck paths).
//! - Per-page rows: pagetype leaf/internal/overflow, ncell, payload,
//!   unused, mx_payload, pgoffset/pgsize arithmetic, the sqlite_master
//!   b-tree included.
//! - The one-object filter and AGGREGATE mode.
//! - Overflow chains get one row per chain page; interior pages appear
//!   once a tree splits.

use rustqlite::Database;

fn q0(db: &Database, sql: &str) -> Vec<Vec<rustqlite::Value>> {
    db.query(sql, []).unwrap()
}

fn qi(db: &Database, sql: &str) -> i64 {
    let rows = db.query(sql, []).unwrap();
    rows.first()
        .and_then(|r| r.first())
        .map(|v| v.as_integer())
        .unwrap_or(0)
}

#[test]
fn dbstat_bare_and_filtered_forms() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    for i in 0..50 {
        db.execute(&format!("INSERT INTO t VALUES ({i}, 'v{i}')"), [])
            .unwrap();
    }
    db.execute("CREATE INDEX ti ON t(v)", []).unwrap();

    // Bare eponymous form: rows exist for t, its index, and
    // sqlite_master.
    let names: Vec<String> = q0(&db, "SELECT DISTINCT name FROM dbstat")
        .iter()
        .map(|r| r[0].as_text().to_string())
        .collect();
    assert!(
        names.iter().any(|n| n == "t"),
        "table btree present: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "ti"),
        "index btree present: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "sqlite_master"),
        "schema btree present: {names:?}"
    );

    // Filtered form: only t's pages.
    let filtered: Vec<String> = q0(&db, "SELECT DISTINCT name FROM dbstat('t')")
        .iter()
        .map(|r| r[0].as_text().to_string())
        .collect();
    assert_eq!(filtered, vec!["t".to_string()]);

    // Unknown filter: empty, not an error (SQLite shape).
    assert_eq!(qi(&db, "SELECT count(*) FROM dbstat('nope')"), 0);
}

#[test]
fn dbstat_per_page_shape() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    for i in 1..=20 {
        db.execute(&format!("INSERT INTO t VALUES ({i}, 'v{i}')"), [])
            .unwrap();
    }

    // 20 small rows fit one leaf page.
    let rows = q0(
        &db,
        "SELECT path, pageno, pagetype, ncell, pgoffset, pgsize FROM dbstat('t')",
    );
    assert_eq!(rows.len(), 1, "one leaf page: {rows:?}");
    let r = &rows[0];
    assert_eq!(r[0].as_text(), "/");
    assert_eq!(r[2].as_text(), "leaf");
    assert_eq!(r[3].as_integer(), 20, "ncell = row count");
    let pageno = r[1].as_integer();
    let pgsize = r[5].as_integer();
    assert_eq!(pgsize, 4096);
    assert_eq!(r[4].as_integer(), pageno * pgsize);
    // payload + unused + overhead fills the page — both are positive.
    let pu = qi(&db, "SELECT payload + unused FROM dbstat('t')");
    assert!(pu > 0 && pu < 4096, "payload+unused within the page: {pu}");

    // Split the tree: 2000 rows force interior pages.
    for i in 100..2100 {
        db.execute(&format!("INSERT INTO t VALUES ({i}, 'v{i}')"), [])
            .unwrap();
    }
    let kinds: Vec<String> = q0(&db, "SELECT DISTINCT pagetype FROM dbstat('t')")
        .iter()
        .map(|r| r[0].as_text().to_string())
        .collect();
    assert!(
        kinds.contains(&"internal".to_string()) && kinds.contains(&"leaf".to_string()),
        "split tree has internal + leaf pages: {kinds:?}"
    );
    // The root's path is '/'; every other path starts with '/'.
    let paths: Vec<String> = q0(&db, "SELECT path FROM dbstat('t')")
        .iter()
        .map(|r| r[0].as_text().to_string())
        .collect();
    assert!(paths.iter().all(|p| p.starts_with('/')));
    assert!(paths.iter().any(|p| p == "/"), "root present: {paths:?}");
}

#[test]
fn dbstat_overflow_chain_rows() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, b BLOB)", [])
        .unwrap();
    // One 64 KB blob: local prefix + ceil spill across 4096-byte pages
    // (16-byte overflow header per page, cap = 4080).
    let hex: String = (0..65536u32)
        .map(|i| format!("{:02x}", (i % 251) as u8))
        .collect();
    db.execute(&format!("INSERT INTO t VALUES (1, x'{hex}')"), [])
        .unwrap();

    let rows = q0(
        &db,
        "SELECT pagetype, count(*), sum(payload) FROM dbstat('t') GROUP BY pagetype",
    );
    let mut overflow_pages = 0i64;
    let mut overflow_payload = 0i64;
    for r in &rows {
        if r[0].as_text() == "overflow" {
            overflow_pages = r[1].as_integer();
            overflow_payload = r[2].as_integer();
        }
    }
    assert!(overflow_pages > 0, "overflow rows present: {rows:?}");
    // The cell payload is the full RECORD (~5 B header + the 64 KB
    // blob): mx_payload is the record's total size, the leaf stores
    // the local prefix, and the chain carries exactly the rest.
    let local = qi(
        &db,
        "SELECT payload FROM dbstat('t') WHERE pagetype = 'leaf'",
    );
    let mx = qi(
        &db,
        "SELECT mx_payload FROM dbstat('t') WHERE pagetype = 'leaf'",
    );
    assert!(mx >= 65536, "mx_payload covers the whole blob record: {mx}");
    assert_eq!(overflow_payload, mx - local);
    // Chain math: ceil((total - local) / 4080) pages (cap = 4096-16).
    let expect_pages = ((mx - local) + 4079) / 4080;
    assert_eq!(overflow_pages, expect_pages);
}

#[test]
fn dbstat_aggregate_mode() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    for i in 1..=500 {
        db.execute(&format!("INSERT INTO t VALUES ({i}, 'value-{i}')"), [])
            .unwrap();
    }

    // Aggregate: exactly one row for t, ncell = total cells, payload =
    // the per-page sum.
    let agg = q0(
        &db,
        "SELECT ncell, payload, unused, mx_payload, pgsize FROM dbstat('t', 1)",
    );
    assert_eq!(agg.len(), 1);
    let ncell = agg[0][0].as_integer();
    let payload = agg[0][1].as_integer();
    // ncell counts EVERY page's cells (interior separators included,
    // like SQLite's dbstat) — the 500 rows live on the leaves.
    let leaf_cells = qi(
        &db,
        "SELECT sum(ncell) FROM dbstat('t') WHERE pagetype = 'leaf'",
    );
    assert_eq!(leaf_cells, 500);
    assert_eq!(
        ncell,
        qi(&db, "SELECT sum(ncell) FROM dbstat('t')"),
        "aggregate ncell = the per-page sum"
    );
    let per_page_sum = qi(&db, "SELECT sum(payload) FROM dbstat('t')");
    assert_eq!(payload, per_page_sum);
    let page_count = qi(&db, "SELECT count(*) FROM dbstat('t')");
    let unused_sum = qi(&db, "SELECT sum(unused) FROM dbstat('t')");
    assert_eq!(agg[0][2].as_integer(), unused_sum);
    assert!(page_count >= 1);
    assert_eq!(agg[0][4].as_integer(), 4096);

    // Aggregate across everything: one row per btree (t +
    // sqlite_master).
    let n = qi(&db, "SELECT count(*) FROM dbstat(NULL, 1)");
    let objects = qi(
        &db,
        "SELECT count(*) FROM (SELECT DISTINCT name FROM dbstat)",
    );
    assert_eq!(n, objects, "aggregate row per btree");
}

#[test]
fn dbstat_shadows_and_column_contract() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", [])
        .unwrap();
    // Column contract: exactly SQLite's 10 columns (via SELECT *).
    let row = q0(&db, "SELECT * FROM dbstat('t')");
    assert_eq!(row.len(), 1);
    // 10 columns per row.
    assert_eq!(row[0].len(), 10);
    // A real table named dbstat shadows the eponymous form.
    db.execute("CREATE TABLE dbstat (x INTEGER)", []).unwrap();
    let n = qi(&db, "SELECT count(*) FROM dbstat");
    assert_eq!(n, 0, "the real table shadows the vtab");
    // The explicit function form still reaches the vtab.
    let n2 = qi(&db, "SELECT count(*) FROM dbstat('t')");
    assert!(n2 >= 1);
}
