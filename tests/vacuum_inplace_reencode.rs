//! In-place VACUUM re-encode regressions (the schema cell patch).
//!
//! The in-place planner's schema-site resolution historically looked the
//! RAW SQLite-convention (1-based) rootpage up in the INTERNAL (0-based)
//! live-id map — any object whose root+1 page had died (every suffix
//! mass-delete: the truncate DELETE frees the tail past the root)
//! spuriously declined the whole in-place path, and the byte-patch
//! variant could not shrink a 2-byte rootpage back into the 1-byte
//! range. The re-encode path (SchemaCellPatch + rebuild_schema_leaf)
//! fixes both: the row is re-encoded whole and the schema leaf rebuilt.
//!
//! These shapes pin the CONTRACT the fast path must keep: a compact
//! file, every row intact, the rootpage column re-anchored, a clean
//! REOPEN (fresh pager reads the rebuilt bytes from disk), and further
//! DML through the re-anchored roots.

use rustqlite::{Database, Value};

fn tmpdb(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "vac-inplace-{}-{}-{}.db",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ))
}

fn rows_of(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql, []).unwrap()
}

/// The flagship churn shape (torture S14 at CI scale): one table + one
/// index, bulk-loaded to ~800 pages (the table root is promoted to a
/// 2-byte page id mid-file), then a suffix DELETE frees 90% (the root's
/// +1 page dies with the tail). VACUUM must compact in place — the
/// rootpage re-encode shrinks the column from a 2-byte value to a
/// 1-byte one — and the result must reopen and keep working.
#[test]
fn in_place_vacuum_suffix_mass_delete_reencode_reopens() {
    let path = tmpdb("s14");
    let rows: i64 = 100_000;
    let keep = rows / 10;
    {
        let mut db = Database::open(&path).unwrap();
        // The in-place install is the WAL-mode fast path (rq_file in the
        // torture matrix sets the same pragma pair).
        db.execute("PRAGMA journal_mode=WAL", []).unwrap();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
            [],
        )
        .unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 1..=rows {
            db.execute(
                "INSERT INTO t (name, val, score) VALUES (?1, ?2, ?3)",
                [
                    Value::Text(format!("name{i}").into()),
                    Value::Integer(i),
                    Value::Real(i as f64 * 1.5),
                ],
            )
            .unwrap();
        }
        db.execute("COMMIT", []).unwrap();

        // The suffix mass delete: rows keep+1.. and their index entries.
        db.execute("DELETE FROM t WHERE id > ?1", [Value::Integer(keep)])
            .unwrap();
        let cnt: i64 = rows_of(&db, "SELECT COUNT(*) FROM t")[0][0].as_integer();
        assert_eq!(cnt, keep, "count after delete");

        db.execute("VACUUM", []).unwrap();

        // Same-connection sanity right after the swap: the catalog was
        // reloaded from the re-encoded schema row, so every query path
        // runs through the re-anchored roots.
        let cnt: i64 = rows_of(&db, "SELECT COUNT(*) FROM t")[0][0].as_integer();
        assert_eq!(cnt, keep, "count after vacuum (same connection)");
        let agg = rows_of(&db, "SELECT SUM(val), MIN(id), MAX(id), COUNT(*) FROM t")[0].clone();
        let expect_sum: i64 = (1..=keep).sum();
        assert_eq!(agg[0].as_integer(), expect_sum, "SUM(val) after vacuum");
        assert_eq!(agg[1].as_integer(), 1);
        assert_eq!(agg[2].as_integer(), keep);
        assert_eq!(agg[3].as_integer(), keep);
        // A keyed probe through the (moved) table root.
        let via_key = db
            .query("SELECT id FROM t WHERE id = ?1", [Value::Integer(keep)])
            .unwrap();
        assert_eq!(via_key.len(), 1, "key probe after vacuum");
        assert_eq!(via_key[0][0].as_integer(), keep);
    }
    // REOPEN: a fresh pager reads the rebuilt schema leaf from disk —
    // the re-encoded rootpage must anchor the moved trees.
    {
        let mut db = Database::open(&path).unwrap();
        let cnt: i64 = rows_of(&db, "SELECT COUNT(*) FROM t")[0][0].as_integer();
        assert_eq!(cnt, keep, "count after reopen");
        let first = rows_of(&db, "SELECT name, score FROM t WHERE id = 1")[0].clone();
        assert_eq!(first[0].as_text(), "name1");
        assert_eq!(first[1].as_real(), 1.5);
        let via_key = db
            .query("SELECT name FROM t WHERE id = ?1", [Value::Integer(7_777)])
            .unwrap();
        assert_eq!(via_key.len(), 1, "key probe after reopen");
        assert_eq!(via_key[0][0].as_text(), "name7777");
        // The rootpage column itself: compacted into the 1-byte range
        // (the file shrank from ~800 pages to well under 128 — both the
        // table and the index root must sit inside it).
        let rp = rows_of(
            &db,
            "SELECT name, rootpage FROM sqlite_master WHERE type = 'table'",
        );
        assert_eq!(rp.len(), 1, "the table row");
        for r in &rp {
            let root = r[1].as_integer();
            assert!(
                root > 0 && root < 128,
                "rootpage {root} must be re-anchored into the compact range"
            );
        }
        // Further DML through the re-anchored roots: insert, update,
        // delete, and one more vacuum round (identity this time).
        db.execute(
            "INSERT INTO t (name, val, score) VALUES ('post', 100001, 0.5)",
            [],
        )
        .unwrap();
        db.execute("UPDATE t SET score = 9.9 WHERE id = 3", [])
            .unwrap();
        db.execute("DELETE FROM t WHERE id = 4", []).unwrap();
        db.execute("VACUUM", []).unwrap();
        let cnt: i64 = rows_of(&db, "SELECT COUNT(*) FROM t")[0][0].as_integer();
        assert_eq!(
            cnt, keep,
            "count after second vacuum ({} kept + 1 - 1)",
            keep
        );
        let row3 = rows_of(&db, "SELECT score FROM t WHERE id = 3")[0][0].clone();
        assert_eq!(row3.as_real(), 9.9, "update survived");
        let gone = rows_of(&db, "SELECT COUNT(*) FROM t WHERE id = 4")[0][0].as_integer();
        assert_eq!(gone, 0, "delete survived");
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

/// The multi-leaf schema shape: enough objects that the schema tree
/// spans pages, then churn that frees pages BELOW the object roots so
/// every rootpage column (across BOTH schema leaves) shrinks. The
/// rebuild grouping must patch each affected leaf exactly once, and the
/// schema tree's own interior child references move with it.
#[test]
fn in_place_vacuum_multi_leaf_schema_reencode() {
    let path = tmpdb("multileaf");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("PRAGMA journal_mode=WAL", []).unwrap();
        // 60 objects, EVERY one populated: the schema tree outgrows
        // page 0 into a second leaf (each schema row is ~100 bytes) and
        // no table carries an empty wired leaf (which in-place cannot
        // prune — the strict object gate declines on it by design).
        for i in 0..60 {
            db.execute(
                &format!("CREATE TABLE t{i:02} (k INTEGER PRIMARY KEY, v TEXT)"),
                [],
            )
            .unwrap();
            db.execute("BEGIN", []).unwrap();
            for k in 1..=300i64 {
                db.execute(
                    &format!("INSERT INTO t{i:02} (k, v) VALUES (?1, ?2)"),
                    [Value::Integer(k), Value::Text(format!("v{k}").into())],
                )
                .unwrap();
            }
            db.execute("COMMIT", []).unwrap();
        }
        // Drop a block of EARLY tables: their (dense, fully-allocated)
        // trees go to the freelist, punching holes that move every
        // later table's root down — and with them every later schema
        // row's rootpage column, across BOTH schema leaves.
        for i in 10..45 {
            db.execute(&format!("DROP TABLE t{i:02}"), []).unwrap();
        }
        let live: i64 = rows_of(&db, "SELECT COUNT(*) FROM sqlite_master")[0][0].as_integer();
        assert_eq!(live, 60 - 35, "25 tables remain");

        db.execute("VACUUM", []).unwrap();

        // Every surviving table works through its re-anchored root,
        // and t59's data is fully intact.
        let cnt: i64 = rows_of(&db, "SELECT COUNT(*) FROM t59")[0][0].as_integer();
        assert_eq!(cnt, 300, "t59 rows after vacuum");
        let probe = rows_of(&db, "SELECT v FROM t59 WHERE k = 271")[0][0].clone();
        assert_eq!(probe.as_text(), "v271");
        // Spot-check survivors on BOTH sides of the dropped block:
        // every one kept its rows through the root move.
        for name in ["t00", "t09", "t45", "t58"] {
            let n: i64 = rows_of(&db, &format!("SELECT COUNT(*) FROM {name}"))[0][0].as_integer();
            assert_eq!(n, 300, "{name} rows intact after vacuum");
            let p = rows_of(&db, &format!("SELECT v FROM {name} WHERE k = 100"))[0][0].clone();
            assert_eq!(p.as_text(), "v100", "{name} row readable");
        }
    }
    // Reopen and re-verify: the schema tree's SECOND leaf moved too —
    // its rows' rootpage columns were rebuilt at the new slot.
    {
        let mut db = Database::open(&path).unwrap();
        let cnt: i64 = rows_of(&db, "SELECT COUNT(*) FROM t59")[0][0].as_integer();
        assert_eq!(cnt, 300, "t59 rows after reopen");
        let live: i64 = rows_of(&db, "SELECT COUNT(*) FROM sqlite_master")[0][0].as_integer();
        assert_eq!(live, 25, "schema rows after reopen");
        let probe = rows_of(&db, "SELECT v FROM t59 WHERE k = 271")[0][0].clone();
        assert_eq!(probe.as_text(), "v271");
        // DML through a moved root after reopen.
        db.execute("INSERT INTO t00 (k, v) VALUES (301, 'x')", [])
            .unwrap();
        let back = rows_of(&db, "SELECT v FROM t00 WHERE k = 301")[0][0].clone();
        assert_eq!(back.as_text(), "x");
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}
