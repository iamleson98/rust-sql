// Trigger feature tests.
use rustqlite::Database;

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE log (id INTEGER PRIMARY KEY, msg TEXT, v INTEGER)",
        [],
    )
    .unwrap();

    // 1. AFTER INSERT trigger
    db.execute("CREATE TRIGGER ai AFTER INSERT ON t BEGIN INSERT INTO log (msg, v) VALUES ('ins', NEW.val); END", []).unwrap();
    db.execute("INSERT INTO t (name, val) VALUES ('a', 1)", [])
        .unwrap();
    let r = db.query("SELECT msg, v FROM log", []).unwrap();
    println!(
        "1. after insert: {:?} (expect ins,1) {}",
        r,
        if format!("{:?}", r) == "[[Text(\"ins\"), Integer(1)]]" {
            "OK"
        } else {
            "FAIL"
        }
    );

    // 2. WHEN guard
    db.execute("CREATE TRIGGER ai_big AFTER INSERT ON t WHEN NEW.val > 100 BEGIN INSERT INTO log (msg, v) VALUES ('big', NEW.val); END", []).unwrap();
    db.execute("INSERT INTO t (name, val) VALUES ('b', 200)", [])
        .unwrap();
    let r = db
        .query("SELECT COUNT(*) FROM log WHERE msg = 'big'", [])
        .unwrap();
    println!(
        "2. when guard: {:?} (expect 1) {}",
        r,
        if r == vec![vec![rustqlite::Value::Integer(1)]] {
            "OK"
        } else {
            "FAIL"
        }
    );

    // 3. AFTER DELETE with OLD
    db.execute("CREATE TRIGGER ad AFTER DELETE ON t BEGIN INSERT INTO log (msg, v) VALUES ('del', OLD.val); END", []).unwrap();
    db.execute("DELETE FROM t WHERE val = 1", []).unwrap();
    let r = db
        .query("SELECT msg, v FROM log WHERE msg = 'del'", [])
        .unwrap();
    println!(
        "3. after delete: {:?} (expect del,1) {}",
        r,
        if format!("{:?}", r) == "[[Text(\"del\"), Integer(1)]]" {
            "OK"
        } else {
            "FAIL"
        }
    );

    // 4. AFTER UPDATE with OLD + NEW
    db.execute("CREATE TRIGGER au AFTER UPDATE ON t BEGIN INSERT INTO log (msg, v) VALUES ('upd:' || OLD.val || '->' || NEW.val, NEW.val); END", []).unwrap();
    db.execute("UPDATE t SET val = 250 WHERE val = 200", [])
        .unwrap();
    let r = db
        .query("SELECT msg FROM log WHERE msg LIKE 'upd%'", [])
        .unwrap();
    println!(
        "4. after update: {:?} (expect upd:200->250) {}",
        r,
        if format!("{:?}", r) == "[[Text(\"upd:200->250\")]]" {
            "OK"
        } else {
            "FAIL"
        }
    );

    // 5. UPDATE OF column filter
    db.execute("CREATE TRIGGER au_val AFTER UPDATE OF val ON t BEGIN INSERT INTO log (msg, v) VALUES ('ofval', NEW.val); END", []).unwrap();
    db.execute("UPDATE t SET name = 'renamed' WHERE val = 250", [])
        .unwrap();
    let r = db
        .query("SELECT COUNT(*) FROM log WHERE msg = 'ofval'", [])
        .unwrap();
    println!(
        "5. update-of (name change, no fire): {:?} (expect 0) {}",
        r,
        if r == vec![vec![rustqlite::Value::Integer(0)]] {
            "OK"
        } else {
            "FAIL"
        }
    );
    db.execute("UPDATE t SET val = 251 WHERE val = 250", [])
        .unwrap();
    let r = db
        .query("SELECT COUNT(*) FROM log WHERE msg = 'ofval'", [])
        .unwrap();
    println!(
        "   update-of (val change, fires): {:?} (expect 1) {}",
        r,
        if r == vec![vec![rustqlite::Value::Integer(1)]] {
            "OK"
        } else {
            "FAIL"
        }
    );

    // 6. BEFORE trigger (body runs before the change)
    db.execute("CREATE TRIGGER bi BEFORE INSERT ON t BEGIN INSERT INTO log (msg, v) VALUES ('before', NEW.val); END", []).unwrap();
    db.execute("INSERT INTO t (name, val) VALUES ('c', 3)", [])
        .unwrap();
    let r = db
        .query("SELECT COUNT(*) FROM log WHERE msg = 'before'", [])
        .unwrap();
    println!(
        "6. before insert: {:?} (expect 1) {}",
        r,
        if r == vec![vec![rustqlite::Value::Integer(1)]] {
            "OK"
        } else {
            "FAIL"
        }
    );

    // 7. Trigger body doing an UPDATE
    db.execute("CREATE TRIGGER ai_audit AFTER INSERT ON t BEGIN UPDATE t SET name = 'x' || NEW.val WHERE id = NEW.id; END", []).unwrap();
    db.execute("INSERT INTO t (name, val) VALUES ('z', 77)", [])
        .unwrap();
    let r = db.query("SELECT name FROM t WHERE val = 77", []).unwrap();
    println!(
        "7. body update: {:?} (expect x77) {}",
        r,
        if format!("{:?}", r) == "[[Text(\"x77\")]]" {
            "OK"
        } else {
            "FAIL"
        }
    );

    // 8. DROP TRIGGER
    db.execute("DROP TRIGGER ai_audit", []).unwrap();
    db.execute("INSERT INTO t (name, val) VALUES ('keep', 88)", [])
        .unwrap();
    let r = db.query("SELECT name FROM t WHERE val = 88", []).unwrap();
    println!(
        "8. drop trigger: {:?} (expect keep) {}",
        r,
        if format!("{:?}", r) == "[[Text(\"keep\")]]" {
            "OK"
        } else {
            "FAIL"
        }
    );

    // 9. Multi-row insert fires per row
    db.execute("CREATE TABLE log2 (id INTEGER PRIMARY KEY, n INTEGER)", [])
        .unwrap();
    db.execute("CREATE TRIGGER per_row AFTER INSERT ON log2 BEGIN INSERT INTO log (msg, v) VALUES ('r', NEW.n); END", []).unwrap();
    db.execute("INSERT INTO log2 (n) VALUES (1), (2), (3)", [])
        .unwrap();
    let r = db
        .query("SELECT COUNT(*) FROM log WHERE msg = 'r'", [])
        .unwrap();
    println!(
        "9. per-row (3 rows): {:?} (expect 3) {}",
        r,
        if r == vec![vec![rustqlite::Value::Integer(3)]] {
            "OK"
        } else {
            "FAIL"
        }
    );

    // 10. Trigger inside a transaction + rollback discards effects
    db.execute("BEGIN", []).unwrap();
    db.execute("INSERT INTO log2 (n) VALUES (99)", []).unwrap();
    db.execute("ROLLBACK", []).unwrap();
    let r = db
        .query("SELECT COUNT(*) FROM log WHERE v = 99", [])
        .unwrap();
    println!(
        "10. rollback discards trigger effects: {:?} (expect 0) {}",
        r,
        if r == vec![vec![rustqlite::Value::Integer(0)]] {
            "OK"
        } else {
            "FAIL"
        }
    );

    // 11. Performance sanity: DML on tables WITHOUT triggers unaffected
    let t0 = std::time::Instant::now();
    db.execute("BEGIN", []).unwrap();
    for i in 0..2000 {
        let s = format!("INSERT INTO t (name, val) VALUES ('p{}', {})", i, i);
        db.execute(&s, []).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    println!(
        "11. 2000 inserts with triggers present: {:?} (correctness > speed here)",
        t0.elapsed()
    );
}
