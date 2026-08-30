//! Dump the plan for the failing case-4 query.
use rustqlite::Database;
use rustqlite::Value;

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    for s in [
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, dept TEXT, salary REAL)",
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, total INTEGER)",
        "CREATE TABLE items (id INTEGER PRIMARY KEY, order_id INTEGER, qty INTEGER)",
        "INSERT INTO users VALUES (1, 'alice', 'eng', 100.0)",
        "INSERT INTO orders VALUES (1, 1, 30)",
        "INSERT INTO items VALUES (1, 1, 2)",
    ] {
        db.execute(s, []).unwrap();
    }

    // Probe: subquery alone with a NULL-ish outer ref — simulate what an
    // UNCORRELATED execution would produce (o.id unresolvable).
    let r = db.query("SELECT COUNT(*) FROM items i WHERE i.order_id = o.id", []);
    println!("subquery w/ unresolvable ref: {:?}", r.map(|v| v.len()));

    // Case 4: predicate evaluated per users row? per orders row? per join row?
    // Try intermediate shapes to find where it breaks:
    let shapes = [
        // join + where w/ correlated subquery (FAILS: 0 rows)
        "SELECT u.name, o.id FROM users u JOIN orders o ON o.user_id = u.id WHERE (SELECT COUNT(*) FROM items i WHERE i.order_id = o.id) >= 1",
        // join + where w/ correlated subquery referencing LEFT side
        "SELECT u.name, o.id FROM users u JOIN orders o ON o.user_id = u.id WHERE (SELECT COUNT(*) FROM items i WHERE i.order_id = u.id) >= 1",
        // no join: where w/ correlated subquery over single table (works?)
        "SELECT name FROM users u WHERE (SELECT COUNT(*) FROM items i WHERE i.order_id = u.id) >= 1",
        // join + where with plain column comparison (sanity)
        "SELECT u.name, o.id FROM users u JOIN orders o ON o.user_id = u.id WHERE o.total > 0",
        // join + where w/ UNcorrelated subquery
        "SELECT u.name, o.id FROM users u JOIN orders o ON o.user_id = u.id WHERE (SELECT COUNT(*) FROM items) >= 1",
    ];
    for sql in shapes {
        match db.query(sql, []) {
            Ok(rows) => println!("{:.70} => {} rows {:?}", sql, rows.len(), rows.iter().take(3).collect::<Vec<_>>()),
            Err(e) => println!("{:.70} => ERR {}", sql, e),
        }
    }
    let _ = Value::Null;
}
