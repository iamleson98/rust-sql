//! Count actual entries in the index btree vs the table — confirm the
//! 8KB-page index build drops entries.
use rustqlite::types::Value;
use rustqlite::Database;

fn main() {
    for n in [1000usize, 2000] {
        let mut db = Database::open_in_memory().unwrap();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, cat TEXT, val INTEGER)",
            [],
        )
        .unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 1..=n as i64 {
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

        // Walk EVERY index entry by scanning the empty prefix (matches all).
        // Use a query shape that scans the whole index... easier: count each
        // cat plus a wildcard-ish probe via range: cat >= 'a' AND cat <= 'c'
        let total_idx: i64 = db
            .query("SELECT COUNT(*) FROM t WHERE cat >= 'a'", [])
            .unwrap()[0][0]
            .as_integer();
        let rows_t: i64 = db.query("SELECT COUNT(*) FROM t", []).unwrap()[0][0].as_integer();
        println!(
            "n={}: table rows={}  index-visible(cat>='a')={}  {}",
            n,
            rows_t,
            total_idx,
            if total_idx == rows_t {
                "OK".to_string()
            } else {
                format!("MISSING {}", rows_t - total_idx)
            }
        );

        // Which rowids are missing? Compare per-cat counts.
        for cat in ["a", "b", "c"] {
            let cnt = db
                .query(
                    "SELECT COUNT(*) FROM t WHERE cat = ?",
                    [Value::Text(cat.into())],
                )
                .unwrap()[0][0]
                .as_integer();
            println!("   cat={} -> {}", cat, cnt);
        }

        // Direct integrity check via the public API if available.
        // Also: re-open path — does the in-memory index tree persist state?
        // (in-memory: reopen not applicable; test a file-backed one too)
        let path = std::env::temp_dir().join(format!("probe_idx8k_{}.db", n));
        let _ = std::fs::remove_file(&path);
        let mut dbf = Database::open(&path).unwrap();
        dbf.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, cat TEXT, val INTEGER)",
            [],
        )
        .unwrap();
        dbf.execute("BEGIN", []).unwrap();
        for i in 1..=n as i64 {
            let cat = if i % 3 == 0 {
                "a"
            } else if i % 3 == 1 {
                "b"
            } else {
                "c"
            };
            dbf.execute(
                "INSERT INTO t (cat, val) VALUES (?, ?)",
                [Value::Text(cat.into()), Value::Integer(i)],
            )
            .unwrap();
        }
        dbf.execute("COMMIT", []).unwrap();
        dbf.execute("CREATE INDEX idx_cat ON t(cat)", []).unwrap();
        let total_idxf: i64 = dbf
            .query("SELECT COUNT(*) FROM t WHERE cat >= 'a'", [])
            .unwrap()[0][0]
            .as_integer();
        let rows_tf: i64 = dbf.query("SELECT COUNT(*) FROM t", []).unwrap()[0][0].as_integer();
        println!(
            "   file-backed: table={} index-visible={} {}",
            rows_tf,
            total_idxf,
            if total_idxf == rows_tf {
                "OK".to_string()
            } else {
                format!("MISSING {}", rows_tf - total_idxf)
            }
        );
        println!();
    }
}
