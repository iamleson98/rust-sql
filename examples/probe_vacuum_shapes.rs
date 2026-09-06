//! VACUUM correctness across shapes: dense churn, fragmented (random
//! deletes), blob tables with overflow chains, multi-table + indexes.
//! Every shape: VACUUM, then verify rows, indexes, and file shrink; a
//! reopen round-trip catches on-disk inconsistency.
use rustqlite::{Database, Value};

fn count(db: &Database, sql: &str) -> i64 {
    match db.query(sql, []).unwrap().first().and_then(|r| r.first()) {
        Some(Value::Integer(n)) => *n,
        other => panic!("bad count for {sql}: {other:?}"),
    }
}

fn reopen_check(path: &str, checks: &[(&str, i64)]) {
    let db = Database::open(path).unwrap();
    for (sql, want) in checks {
        let got = count(&db, sql);
        assert_eq!(
            got, *want,
            "REOPEN check failed: {sql} = {got}, want {want}"
        );
    }
}

fn file_mb(path: &str) -> f64 {
    std::fs::metadata(path)
        .map(|m| m.len() as f64 / 1024.0 / 1024.0)
        .unwrap_or(0.0)
}

fn wal_file(path: &str) -> f64 {
    std::fs::metadata(format!("{path}-wal"))
        .map(|m| m.len() as f64 / 1024.0 / 1024.0)
        .unwrap_or(0.0)
}

fn fresh(path: &str) -> Database {
    for s in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{path}{s}"));
    }
    let mut db = Database::open(path).unwrap();
    db.execute("PRAGMA journal_mode=WAL", []).unwrap();
    db.execute("PRAGMA synchronous=OFF", []).unwrap();
    db
}

fn main() {
    let tmp = "/tmp/vac_shapes";

    // ---- Shape 1: dense churn (tail delete) — in-place path ----
    {
        let path = format!("{tmp}_dense.rq.db");
        let mut db = fresh(&path);
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
            [],
        )
        .unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 1..=100_000i64 {
            db.execute(
                "INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
                [
                    Value::Text(format!("name{i}").into()),
                    Value::Integer(i),
                    Value::Real(i as f64),
                ],
            )
            .unwrap();
        }
        db.execute("COMMIT", []).unwrap();
        db.execute("DELETE FROM t WHERE id > 10000", []).unwrap();
        let mb1 = file_mb(&path) + wal_file(&path);
        db.execute("VACUUM", []).unwrap();
        let checks = [
            ("SELECT COUNT(*) FROM t", 10_000),
            ("SELECT SUM(val) FROM t", 50_005_000),
            ("SELECT MIN(val), MAX(val) FROM t WHERE 1=1", 1), // shape only
        ];
        for (sql, want) in checks.iter().take(2) {
            assert_eq!(count(&db, sql), *want, "shape1 post-vacuum {sql}");
        }
        // Post-vacuum inserts still work (roots valid, allocation sane).
        db.execute(
            "INSERT INTO t (name, val, score) VALUES ('post', 999999, 1.0)",
            [],
        )
        .unwrap();
        assert_eq!(count(&db, "SELECT COUNT(*) FROM t"), 10_001);
        let mb2 = file_mb(&path) + wal_file(&path);
        println!(
            "shape1 dense churn: {mb1:.2}MB -> {mb2:.2}MB (+wal {:.2})",
            wal_file(&path)
        );
        assert!(mb2 < mb1 * 0.5, "shape1 must reclaim: {mb1} -> {mb2}");
        drop(db);
        reopen_check(
            &path,
            &[
                ("SELECT COUNT(*) FROM t", 10_001),
                ("SELECT SUM(val) FROM t", 50_005_000 + 999_999),
            ],
        );
    }

    // ---- Shape 2: fragmented (random deletes) — image path fallback ----
    {
        let path = format!("{tmp}_frag.rq.db");
        let mut db = fresh(&path);
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", [])
            .unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 1..=50_000i64 {
            db.execute("INSERT INTO t (x) VALUES (?)", [Value::Integer(i)])
                .unwrap();
        }
        db.execute("COMMIT", []).unwrap();
        // Genuinely fragmented: empty the [10k, 30k] range but strand a
        // 100-row island at [20k, 20.1k] — most mid leaves empty (holes),
        // the island's leaf stays live at a high page id (it must MOVE
        // down), and the freelist gains interior holes.
        db.execute(
            "DELETE FROM t WHERE id >= 10000 AND id < 30000 AND NOT (id >= 20000 AND id < 20100)",
            [],
        )
        .unwrap();
        let expect: i64 = 50_000 - 20_000 + 100; // 30100: range minus island
        let sum: i64 = (1..=50_000)
            .filter(|i| !(*i >= 10_000 && *i < 30_000 && !(*i >= 20_000 && *i < 20_100)))
            .sum();
        db.execute("VACUUM", []).unwrap();
        assert_eq!(count(&db, "SELECT COUNT(*) FROM t"), expect, "shape2 count");
        assert_eq!(count(&db, "SELECT SUM(x) FROM t"), sum, "shape2 sum");
        // Stranded island intact + boundary rows.
        for probe in [1i64, 9_999, 20_000, 20_099, 30_000, 50_000] {
            let got = count(&db, &format!("SELECT COUNT(*) FROM t WHERE id = {probe}"));
            assert_eq!(got, 1, "shape2 row {probe} must survive");
        }
        db.execute("INSERT INTO t (x) VALUES (-1)", []).unwrap();
        drop(db);
        reopen_check(
            &path,
            &[
                ("SELECT COUNT(*) FROM t", expect + 1),
                ("SELECT SUM(x) FROM t", sum - 1),
            ],
        );
        println!("shape2 fragmented: OK ({} rows)", expect + 1);
    }

    // ---- Shape 3: blob table with overflow chains + index ----
    {
        let path = format!("{tmp}_blob.rq.db");
        let mut db = fresh(&path);
        db.execute(
            "CREATE TABLE b (id INTEGER PRIMARY KEY, tag TEXT, blob BLOB)",
            [],
        )
        .unwrap();
        db.execute("CREATE INDEX i_tag ON b(tag)", []).unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 1..=300i64 {
            let blob = vec![i as u8; 20_000]; // overflow chains
            db.execute(
                "INSERT INTO b (tag, blob) VALUES (?, ?)",
                [Value::Text(format!("tag{i:03}").into()), Value::Blob(blob)],
            )
            .unwrap();
        }
        db.execute("COMMIT", []).unwrap();
        // Delete half (scattered) — overflow chains + index entries freed.
        db.execute("DELETE FROM b WHERE id % 2 = 0", []).unwrap();
        db.execute("VACUUM", []).unwrap();
        assert_eq!(count(&db, "SELECT COUNT(*) FROM b"), 150, "shape3 count");
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM b WHERE tag >= 'tag150'"),
            75,
            "shape3 index range"
        );
        // Blob integrity: length + first/last bytes of a survivor.
        let row = db
            .query("SELECT LENGTH(blob), blob FROM b WHERE id = 1", [])
            .unwrap();
        match row.first().map(|r| (r[0].clone(), r[1].clone())) {
            Some((Value::Integer(l), Value::Blob(b))) => {
                assert_eq!(l, 20_000, "shape3 blob length");
                assert_eq!(b[0], 1);
                assert_eq!(b[19_999], 1);
            }
            other => panic!("shape3 blob: {other:?}"),
        }
        drop(db);
        reopen_check(
            &path,
            &[
                ("SELECT COUNT(*) FROM b", 150),
                ("SELECT COUNT(*) FROM b WHERE tag >= 'tag150'", 75),
                ("SELECT SUM(LENGTH(blob)) FROM b", 150 * 20_000),
            ],
        );
        println!("shape3 blobs+overflow+index: OK");
    }

    // ---- Shape 4: multiple tables + views + triggers survive ----
    {
        let path = format!("{tmp}_multi.rq.db");
        let mut db = fresh(&path);
        for ddl in [
            "CREATE TABLE a (id INTEGER PRIMARY KEY, x INTEGER)",
            "CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER REFERENCES a(id))",
            "CREATE INDEX i_a ON b(a_id)",
            "CREATE VIEW v AS SELECT x FROM a WHERE x > 10",
            "CREATE TRIGGER tr AFTER INSERT ON a BEGIN INSERT INTO b (a_id) VALUES (NEW.id); END",
        ] {
            db.execute(ddl, []).unwrap();
        }
        db.execute("BEGIN", []).unwrap();
        for i in 1..=20_000i64 {
            db.execute("INSERT INTO a (x) VALUES (?)", [Value::Integer(i)])
                .unwrap();
        }
        db.execute("COMMIT", []).unwrap();
        db.execute("DELETE FROM b WHERE a_id > 5000", []).unwrap();
        db.execute("DELETE FROM a WHERE id > 5000", []).unwrap();
        db.execute("VACUUM", []).unwrap();
        assert_eq!(count(&db, "SELECT COUNT(*) FROM a"), 5000);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM b"), 5000);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM v"), 4990);
        // Trigger still fires post-VACUUM.
        db.execute("INSERT INTO a (x) VALUES (99999)", []).unwrap();
        assert_eq!(count(&db, "SELECT COUNT(*) FROM b"), 5001);
        drop(db);
        reopen_check(
            &path,
            &[
                ("SELECT COUNT(*) FROM a", 5001),
                ("SELECT COUNT(*) FROM b", 5001),
                ("SELECT COUNT(*) FROM v", 4991),
            ],
        );
        println!("shape4 multi-object (view/trigger/FK/index): OK");
    }

    // ---- Shape 5: VACUUM INTO on the in-place-eligible shape ----
    {
        let path = format!("{tmp}_into.rq.db");
        let mut db = fresh(&path);
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", [])
            .unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 1..=30_000i64 {
            db.execute("INSERT INTO t (x) VALUES (?)", [Value::Integer(i)])
                .unwrap();
        }
        db.execute("COMMIT", []).unwrap();
        db.execute("DELETE FROM t WHERE id > 3000", []).unwrap();
        let into = format!("{tmp}_into_out.rq.db");
        let _ = std::fs::remove_file(&into);
        db.execute(&format!("VACUUM INTO '{into}'"), []).unwrap();
        let out = Database::open(&into).unwrap();
        assert_eq!(count(&out, "SELECT COUNT(*) FROM t"), 3000);
        assert_eq!(count(&out, "SELECT SUM(x) FROM t"), (1..=3000).sum::<i64>());
        println!("shape5 VACUUM INTO: OK");
    }

    println!("ALL VACUUM SHAPE CHECKS PASSED");
}
