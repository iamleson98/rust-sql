//! sqlite_dbdata — the forensic raw-page reader (one row per
//! page/cell/field, no b-tree linkage followed). Engine shape:
//! 0-based pages, per-value-tag row codec, order-key index cells,
//! freed pages ZEROED on free (deleted-row recovery is deliberately
//! impossible; unallocated regions and orphaned overflow chains remain
//! readable).

use rustqlite::{Database, Value};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn blob_of(v: &Value) -> Vec<u8> {
    match v {
        Value::Blob(b) => b.clone(),
        _ => panic!("expected blob, got {v:?}"),
    }
}

#[test]
fn dbdata_basic_field_decode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("d.db");
    let mut db = Database::open(&path).unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT, c BLOB, d REAL, e)",
        [],
    )
    .unwrap();
    // Row 1: every codec shape. Row 2: a second rowid (i16-class).
    db.execute(
        "INSERT INTO t VALUES (1, 100000, 'hi', x'8f8e', 2.5, NULL)",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO t VALUES (200, -3, '', NULL, 0.25, 9)", [])
        .unwrap();

    let rows = db
        .query(
            "SELECT pgno, cell, field, value, hexval, descr FROM sqlite_dbdata('main')",
            [],
        )
        .unwrap();

    // The table's leaf page: find it via the rowid key rows (field=-1).
    let rowid_rows: Vec<&Vec<Value>> = rows
        .iter()
        .filter(|r| r[0].as_integer() != 0 && r[1].as_integer() >= 0 && r[2].as_integer() == -1)
        .collect();
    let key1 = rowid_rows
        .iter()
        .find(|r| r[5].as_text().contains("rowid=1"))
        .expect("rowid=1 key row present");
    assert_eq!(hex(&[0x01]), key1[4].as_text(), "rowid 1 varint bytes");
    let key200 = rowid_rows
        .iter()
        .find(|r| r[5].as_text().contains("rowid=200"))
        .expect("rowid=200 key row present");
    // 200 as plain unsigned LEB128: 0x81 0x48.
    assert_eq!(hex(&[0x81, 0x48]), key200[4].as_text());

    // Row 1's fields (cell = key1's cell): alias marker, i32 int, text,
    // blob, real, null.
    let (pg1, cell1) = (key1[0].as_integer(), key1[1].as_integer());
    let fields1: Vec<&Vec<Value>> = rows
        .iter()
        .filter(|r| {
            r[0].as_integer() == pg1 && r[1].as_integer() == cell1 && r[2].as_integer() >= 0
        })
        .collect();
    // 6 columns: id (rowid marker), a, b, c, d, e.
    assert_eq!(
        fields1.len(),
        6,
        "six fields: {:?}",
        fields1.iter().map(|r| r[5].as_text()).collect::<Vec<_>>()
    );
    let by_field = |f: i64| {
        fields1
            .iter()
            .find(|r| r[2].as_integer() == f)
            .unwrap_or_else(|| panic!("field {f} missing"))
    };
    // field 0: the rowid-alias marker (tag 0x09).
    assert_eq!(blob_of(&by_field(0)[3]), vec![0x09]);
    assert!(by_field(0)[5].as_text().contains("rowid-marker"));
    // field 1: 100000 as tag-0x04 i32 LE.
    assert_eq!(
        blob_of(&by_field(1)[3]),
        [vec![0x04], (100000i32).to_le_bytes().to_vec()].concat()
    );
    assert!(by_field(1)[5].as_text().contains("type=int"));
    // field 2: 'hi' — tag 0x07 + len 2 + bytes.
    assert_eq!(blob_of(&by_field(2)[3]), vec![0x07, 0x02, b'h', b'i']);
    assert!(by_field(2)[5].as_text().contains("type=text"));
    // field 3: blob 8f8e — tag 0x08 + len 2 + bytes.
    assert_eq!(blob_of(&by_field(3)[3]), vec![0x08, 0x02, 0x8f, 0x8e]);
    // field 4: 2.5 — wide f64 (tag 0x06 + 8 LE bytes).
    assert_eq!(blob_of(&by_field(4)[3])[0], 0x06);
    assert!(by_field(4)[5].as_text().contains("type=real"));
    // field 5: NULL (tag 0x00).
    assert_eq!(blob_of(&by_field(5)[3]), vec![0x00]);
    assert!(by_field(5)[5].as_text().contains("type=null"));

    // hexval mirrors value for every row.
    for r in &rows {
        assert_eq!(hex(&blob_of(&r[3])), r[4].as_text(), "hexval of {:?}", r[5]);
    }
}

#[test]
fn dbdata_schema_page_and_interiors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("d.db");
    let mut db = Database::open(&path).unwrap();
    db.execute("CREATE TABLE big (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    {
        let mut sql = String::from("INSERT INTO big (id, v) VALUES ");
        for i in 1..=400i64 {
            if i > 1 {
                sql.push(',');
            }
            sql.push_str(&format!("({}, 'value{:04}')", i, i));
        }
        db.execute(&sql, []).unwrap();
    }

    let rows = db
        .query(
            "SELECT pgno, cell, field, value, descr FROM sqlite_dbdata('main') WHERE cell = -1",
            [],
        )
        .unwrap();
    // Page-header rows: at least the schema page (pgno=0), the table's
    // interior(s) and leaves; the interior header reports right!=0.
    let hdrs: Vec<&Vec<Value>> = rows
        .iter()
        .filter(|r| r[4].as_text().starts_with("pgno=") && r[4].as_text().contains("type="))
        .collect();
    assert!(hdrs.len() >= 3, "page headers present: {hdrs:?}");
    assert!(
        hdrs.iter()
            .any(|r| r[4].as_text().contains("type=interior-table")),
        "an interior page header exists"
    );
    // Interior cells: (child, sep) rows with field=-1 and cell>=0.
    let interiors = db
        .query(
            "SELECT descr FROM sqlite_dbdata('main') WHERE descr LIKE 'cell=%child=%'",
            [],
        )
        .unwrap();
    assert!(!interiors.is_empty(), "interior cell rows present");

    // The schema page (pgno=0) decodes sqlite_master rows: some text
    // field holds 'big' with tag 0x07.
    let schema_texts = db
        .query(
            "SELECT value FROM sqlite_dbdata('main') WHERE pgno = 0 AND field >= 0",
            [],
        )
        .unwrap();
    let big_bytes = [vec![0x07, 0x03], b"big".to_vec()].concat();
    assert!(
        schema_texts.iter().any(|r| blob_of(&r[0]) == big_bytes),
        "the schema row's 'big' name decodes on page 0"
    );
}

#[test]
fn dbdata_overflow_chain() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("d.db");
    let mut db = Database::open(&path).unwrap();
    db.execute("CREATE TABLE b (id INTEGER PRIMARY KEY, w BLOB)", [])
        .unwrap();
    db.execute(
        "INSERT INTO b VALUES (1, ?)",
        [Value::Blob(vec![0xAB; 100_000])],
    )
    .unwrap();

    let ov = db
        .query(
            "SELECT pgno, value, descr FROM sqlite_dbdata('main') WHERE descr LIKE '%type=overflow%'",
            [],
        )
        .unwrap();
    assert!(
        ov.len() >= 24,
        "100KB blob spills into ~25 overflow pages: {ov_len}",
        ov_len = ov.len()
    );
    let psz = 4096usize;
    assert_eq!(
        blob_of(&ov[0][1]).len(),
        psz - 16,
        "overflow chunk = page minus 16B header"
    );
    for r in &ov {
        assert!(
            blob_of(&r[1]).iter().all(|&b| b == 0xAB),
            "every chunk byte is the blob's 0xAB"
        );
    }
    // The chain terminates: exactly one row with next=0 at the tail.
    assert_eq!(
        ov.iter()
            .filter(|r| r[2].as_text().contains("next=0"))
            .count(),
        1,
        "one chain tail"
    );
}

#[test]
fn dbdata_freelist_zeroed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("d.db");
    let mut db = Database::open(&path).unwrap();
    db.execute("CREATE TABLE dropme (v TEXT)", []).unwrap();
    {
        let mut sql = String::from("INSERT INTO dropme VALUES ");
        for i in 0..400i64 {
            if i > 0 {
                sql.push(',');
            }
            sql.push_str(&format!("('row number {} padding padding padding')", i));
        }
        db.execute(&sql, []).unwrap();
    }
    // keep allocates AFTER dropme so dropme's page is not the file tail
    // (the tail would be truncated away, never reaching the freelist).
    db.execute("CREATE TABLE keep (v)", []).unwrap();
    db.execute("INSERT INTO keep VALUES ('k')", []).unwrap();
    db.execute("DROP TABLE dropme", []).unwrap();

    let rows = db
        .query(
            "SELECT pgno, descr FROM sqlite_dbdata('main') WHERE descr LIKE '%freelist%' OR descr LIKE '%zeroed%'",
            [],
        )
        .unwrap();
    assert!(
        rows.iter()
            .any(|r| r[1].as_text().contains("freelist-trunk")),
        "a trunk row is reported: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|r| r[1].as_text().contains("zeroed (freed)")),
        "freed pages decode as zeroed (the engine scribbles on free)"
    );
}

#[test]
fn dbdata_args_and_shadowing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("d.db");
    let mut db = Database::open(&path).unwrap();
    db.execute("CREATE TABLE one (v)", []).unwrap();
    // 'main' and NULL forms work (the schema argument is the entry —
    // SQLite's own shape; there is no no-arg eponymous form).
    let a = db
        .query("SELECT count(*) FROM sqlite_dbdata('main')", [])
        .unwrap()[0][0]
        .as_integer();
    let b = db
        .query("SELECT count(*) FROM sqlite_dbdata(NULL)", [])
        .unwrap()[0][0]
        .as_integer();
    assert!(a > 0);
    assert_eq!(a, b);
    // Unknown schema errors like SQLite.
    assert!(db
        .query("SELECT * FROM sqlite_dbdata('nonsense')", [])
        .is_err());
    // sqlite_* names are reserved (SQLite parity): the shadowing
    // scenario is structurally impossible — creation errors.
    assert!(
        db.execute("CREATE TABLE sqlite_dbdata (x)", []).is_err(),
        "sqlite_ names are reserved for internal use"
    );
}
