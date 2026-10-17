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
/// ROOT CAUSE (fixed): cross-schema DML ran through the READER-shaped
/// federation path (`exec_foreign_select_with_cols`), whose DML
/// epilogue merged root overlays but skipped the maps-populated flag
/// and `sync_schema_roots_inner` — so a bulk copy that split the
/// target's root left the sqlite_schema row naming the ORIGINAL leaf.
/// Every fresh session then opened a one-leaf truncated table (the
/// split pages orphaned), and mid-session FastPath::CountStar fell
/// back to the stale catalog Arc through the stale flag. Both foreign
/// DML tails (this one and the injected-channel reverse direction) now
/// mirror the query-path DML epilogue completely.
#[test]
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
        // SQLite parity: a clean check reports the TEXT "ok" (one row
        // per problem otherwise).
        let (_, ic) = dst
            .query_with_columns("PRAGMA integrity_check", [])
            .unwrap();
        assert_eq!(ic, vec![vec![Value::Text("ok".into())]], "integrity_check");
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

/// Issue #8, second bug: a bulk INSERT..SELECT from an ATTACHed source
/// that ABORTS partway (a UNIQUE violation on the target at row N of M)
/// must leave NOTHING behind — no table rows, no index entries, no
/// schema-roots drift. The report's "ghost index entries for rows that
/// were never inserted" (13 table rows vs 154 index entries under a
/// concurrent source writer) was the read-side staleness of the same
/// reader-path epilogue bug — this pins the atomicity half: whatever
/// landed before the abort is taken back by the statement journal.
#[test]
fn attach_bulk_abort_midway_leaves_nothing() {
    const GOOD: usize = 300;
    const TOTAL: usize = 700;

    let src_path = tmpdb("atomic-src");
    let dst_path = tmpdb("atomic-dst");
    {
        let mut src = Database::open(&src_path).unwrap();
        src.execute("CREATE TABLE t (id text PRIMARY KEY, v integer)", ())
            .unwrap();
        src.execute("BEGIN", ()).unwrap();
        for i in 0..TOTAL {
            // Row GOOD collides with a row the target already holds —
            // the abort lands mid-statement, after ~GOOD-1 rows flowed.
            let key = if i == GOOD {
                "deadbeef-collide-4bad-8bad-deadbeefbad0".to_string()
            } else {
                uuid(i)
            };
            src.execute(&format!("INSERT INTO t VALUES ('{key}', {i})"), ())
                .unwrap();
        }
        src.execute("COMMIT", ()).unwrap();

        let mut dst = Database::open(&dst_path).unwrap();
        dst.execute("CREATE TABLE t (id text PRIMARY KEY, v integer)", ())
            .unwrap();
        // The colliding row pre-exists on the target.
        dst.execute(
            "INSERT INTO t VALUES ('deadbeef-collide-4bad-8bad-deadbeefbad0', -1)",
            (),
        )
        .unwrap();
        dst.execute(&format!("ATTACH '{}' AS o", src_path.display()), ())
            .unwrap();
        // Abort partway: the UNIQUE violation halts the statement.
        let r = dst.execute("INSERT INTO main.t SELECT * FROM o.t", ());
        assert!(r.is_err(), "the colliding row must abort the statement");
        dst.execute("DETACH o", ()).unwrap();

        // NOTHING from the aborted statement may remain.
        let n = one(&dst, "SELECT COUNT(*) FROM t");
        assert_eq!(n, 1, "only the pre-existing row remains (got {n})");
        let idx = one(&dst, "SELECT COUNT(*) FROM t WHERE id > '0'");
        assert_eq!(
            idx, 1,
            "index carries only the pre-existing row (got {idx})"
        );
        let (_, ic) = dst
            .query_with_columns("PRAGMA integrity_check", [])
            .unwrap();
        assert_eq!(ic, vec![vec![Value::Text("ok".into())]]);

        // And the target still WORKS: a clean re-copy lands everything
        // (the abort did not poison roots, maps, or the scratch).
        dst.execute(&format!("ATTACH '{}' AS o", src_path.display()), ())
            .unwrap();
        dst.execute("INSERT OR IGNORE INTO main.t SELECT * FROM o.t", ())
            .unwrap();
        dst.execute("DETACH o", ()).unwrap();
        let n = one(&dst, "SELECT COUNT(*) FROM t");
        assert_eq!(n, TOTAL as i64, "post-abort re-copy completes ({n})");
        let idx = one(&dst, "SELECT COUNT(*) FROM t WHERE id > '0'");
        assert_eq!(idx, TOTAL as i64, "post-abort index complete ({idx})");
        let (_, ic) = dst
            .query_with_columns("PRAGMA integrity_check", [])
            .unwrap();
        assert_eq!(ic, vec![vec![Value::Text("ok".into())]]);
    }
    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&dst_path);
}
