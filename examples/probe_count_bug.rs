//! Reproduce the IndexCount correctness bug: duplicate runs spanning
//! multiple index leaves under-count with 8KB pages.
use rustqlite::types::Value;
use rustqlite::Database;

fn main() {
    for n in [1000usize, 2000, 5000] {
        let mut db = Database::open_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, cat TEXT, val INTEGER)", []).unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 1..=n as i64 {
            let cat = if i % 3 == 0 { "a" } else if i % 3 == 1 { "b" } else { "c" };
            db.execute("INSERT INTO t (cat, val) VALUES (?, ?)",
                [Value::Text(cat.into()), Value::Integer(i)]).unwrap();
        }
        db.execute("COMMIT", []).unwrap();
        db.execute("CREATE INDEX idx_cat ON t(cat)", []).unwrap();

        // Expected counts.
        let mut exp = std::collections::BTreeMap::new();
        for i in 1..=n as i64 {
            let cat = if i % 3 == 0 { "a" } else if i % 3 == 1 { "b" } else { "c" };
            *exp.entry(cat.to_string()).or_insert(0i64) += 1;
        }

        for cat in ["a", "b", "c"] {
            // Fast path: literal
            let fast = db.query("SELECT COUNT(*) FROM t WHERE cat = ?",
                [Value::Text(cat.into())]).unwrap();
            let fast_val = fast[0][0].as_integer();
            // General path: force non-fast shape via extra predicate.
            let general = db.query(
                "SELECT COUNT(*) FROM t WHERE cat = ? AND val > 0",
                [Value::Text(cat.into())]).unwrap();
            let gen_val = general[0][0].as_integer();
            let expect = exp[cat];
            let status = if fast_val == expect && gen_val == expect { "OK" } else { "BUG" };
            println!("n={:<5} cat={} expect={:<5} fast={:<5} general={:<5} {}",
                n, cat, expect, fast_val, gen_val, status);
        }
        println!();
    }
}
