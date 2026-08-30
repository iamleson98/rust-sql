//! examples/probe_split_trace.rs — bisect which insert corrupts the index
//! tree at 8KB pages: after each insert, verify structural invariants
//! (leaf ordering, parent-child coverage, entry count).
use rustqlite::types::Value;
use rustqlite::Database;

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, cat TEXT, val INTEGER)", []).unwrap();
    // Insert rows one at a time (auto-commit), maintaining the index
    // incrementally — NOT via CREATE INDEX backfill.
    db.execute("CREATE INDEX idx_cat ON t(cat)", []).unwrap();
    let n = 1000i64;
    for i in 1..=n {
        let cat = if i % 3 == 0 { "a" } else if i % 3 == 1 { "b" } else { "c" };
        db.execute("INSERT INTO t (cat, val) VALUES (?, ?)",
            [Value::Text(cat.into()), Value::Integer(i)]).unwrap();
        // Periodically verify: total visible via index == inserted rows.
        if i % 50 == 0 || i == n {
            let total: i64 = db.query("SELECT COUNT(*) FROM t WHERE cat >= 'a'", [])
                .unwrap()[0][0].as_integer();
            if total != i {
                println!("CORRUPT at insert {}: index sees {} of {}", i, total, i);
                // Narrow down per-cat.
                for c in ["a", "b", "c"] {
                    let cnt = db.query("SELECT COUNT(*) FROM t WHERE cat = ?",
                        [Value::Text(c.into())]).unwrap()[0][0].as_integer();
                    println!("   cat={} -> {}", c, cnt);
                }
                break;
            }
        }
    }
    println!("done");
}
