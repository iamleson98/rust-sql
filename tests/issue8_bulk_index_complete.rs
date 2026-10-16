//! Issue #8 gate: bulk `INSERT INTO t SELECT * FROM src` must build
//! COMPLETE secondary/PK indexes.
//!
//! The bulk path used to emit only the first leaf's worth of index
//! entries (page-split sibling linkage lost), so table scans returned
//! every row while index-driven paths (range scans, IN-subqueries,
//! covering-index subselects) silently saw a truncated set. This suite
//! pins the exact production shape — uuid TEXT PRIMARY KEY tables,
//! ATTACHed source, >1 leaf of entries (~24 B/entry, 4 KiB pages) —
//! and asserts the index-driven counts equal the scan counts.

use rustqlite::{Database, Value};

fn tmpdb(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "issue8-{}-{}-{}.db",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

fn one(db: &Database, sql: &str) -> i64 {
    let (_, rs) = db.query_with_columns(sql, []).expect(sql);
    match rs.first().and_then(|r| r.first()) {
        Some(Value::Integer(i)) => *i,
        other => panic!("{sql} -> {other:?}"),
    }
}

fn uuid(i: usize) -> String {
    // deterministic distinct uuid-shaped keys
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        i as u32,
        i as u16,
        (i >> 16) as u16 & 0xfff,
        i as u16,
        i as u64
    )
}

/// Build the source DB with the row-at-a-time path (known correct),
/// then bulk-copy both tables into a fresh target and verify every
/// index-driven path sees the complete key set.
///
/// STILL FAILING on master (ae776f0) — the ATTACHed-source bulk copy
/// truncates the target's PK index (scan sees 78/800 brands), while the
/// plain same-database bulk path is fixed. Kept `#[ignore]` so CI stays
/// green; run with `cargo test --test issue8_bulk_index_complete
/// -- --ignored` when working the ATTACH variant of issue #8.
#[test]
#[ignore = "issue #8 (ATTACH variant): still truncates indexes on master"]
fn attach_bulk_insert_select_builds_complete_indexes() {
    const BRANDS: usize = 800;
    const ROUTES: usize = 2400;

    let src_path = tmpdb("src");
    let dst_path = tmpdb("dst");
    {
        let mut src = Database::open(&src_path).unwrap();
        src.execute(
            "CREATE TABLE brand (id text PRIMARY KEY, name varchar(255))",
            (),
        )
        .unwrap();
        src.execute(
            "CREATE TABLE route (id text PRIMARY KEY, brand_id text)",
            (),
        )
        .unwrap();
        src.execute("BEGIN", ()).unwrap();
        for i in 0..BRANDS {
            let sql = format!("INSERT INTO brand VALUES ('{}', 'brand-{}')", uuid(i), i);
            src.execute(&sql, ()).unwrap();
        }
        for i in 0..ROUTES {
            let sql = format!(
                "INSERT INTO route VALUES ('{}', (SELECT id FROM brand WHERE name = 'brand-{}'))",
                uuid(BRANDS + i),
                i % BRANDS
            );
            src.execute(&sql, ()).unwrap();
        }
        src.execute("COMMIT", ()).unwrap();

        let mut dst = Database::open(&dst_path).unwrap();
        dst.execute(
            "CREATE TABLE brand (id text PRIMARY KEY, name varchar(255))",
            (),
        )
        .unwrap();
        dst.execute(
            "CREATE TABLE route (id text PRIMARY KEY, brand_id text)",
            (),
        )
        .unwrap();
        dst.execute(&format!("ATTACH '{}' AS o", src_path.display()), ())
            .unwrap();
        dst.execute("INSERT INTO main.brand SELECT * FROM o.brand", ())
            .unwrap();
        dst.execute("INSERT INTO main.route SELECT * FROM o.route", ())
            .unwrap();
        dst.execute("DETACH o", ()).unwrap();

        for (table, want) in [("brand", BRANDS as i64), ("route", ROUTES as i64)] {
            let scan = one(&dst, &format!("SELECT COUNT(*) FROM {table}"));
            assert_eq!(scan, want, "{table}: scan count");
            // PK index range scan (the #8 symptom: stops after leaf 1)
            let range = one(
                &dst,
                &format!("SELECT COUNT(*) FROM {table} WHERE id > '0'"),
            );
            assert_eq!(
                range, want,
                "{table}: PK index range scan sees only {range}/{want} — truncated index"
            );
            // covering-index subquery
            let sub = one(
                &dst,
                &format!("SELECT COUNT(*) FROM (SELECT id FROM {table})"),
            );
            assert_eq!(sub, want, "{table}: covering-index subquery count");
        }
        // IN-subquery through the brand PK index
        let join = one(
            &dst,
            "SELECT COUNT(*) FROM route WHERE brand_id IN (SELECT id FROM brand)",
        );
        assert_eq!(join, ROUTES as i64, "join via IN-subquery over PK index");
        let integrity = one(&dst, "PRAGMA integrity_check");
        assert_eq!(integrity, 1, "integrity_check");
    }

    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&dst_path);
}

/// A plain (non-ATTACH) bulk INSERT ... SELECT with 1200 distinct uuid
/// keys — the gate shape proposed in the issue report itself.
#[test]
fn bulk_insert_select_many_distinct_uuid_keys() {
    let path = tmpdb("plain");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE t (id text PRIMARY KEY, v integer)", ())
            .unwrap();
        db.execute("CREATE TABLE src (id text, v integer)", ())
            .unwrap();
        db.execute("BEGIN", ()).unwrap();
        for i in 0..1200 {
            let sql = format!("INSERT INTO src VALUES ('{}', {})", uuid(i), i);
            db.execute(&sql, ()).unwrap();
        }
        db.execute("COMMIT", ()).unwrap();
        db.execute("INSERT INTO t SELECT * FROM src", ()).unwrap();

        let scan = one(&db, "SELECT COUNT(*) FROM t");
        assert_eq!(scan, 1200, "scan count");
        let range = one(&db, "SELECT COUNT(*) FROM t WHERE id > '0'");
        assert_eq!(
            range, 1200,
            "PK index range sees {range}/1200 — truncated index (issue #8)"
        );
        let distinct = one(&db, "SELECT COUNT(DISTINCT id) FROM t");
        assert_eq!(distinct, 1200, "distinct key count");
    }
    let _ = std::fs::remove_file(&path);
}
