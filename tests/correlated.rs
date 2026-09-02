//! Correlated-subquery differential tests: every case runs against BOTH
//! rustqlite and SQLite (rusqlite) and must agree exactly.
use rustqlite::{Database, Value};

fn both(
    db: &mut Database,
    conn: &rusqlite::Connection,
    sql: &str,
) -> (Vec<Vec<String>>, Vec<Vec<String>>) {
    let ours = match db.query(sql, []) {
        Ok(rows) => rows
            .iter()
            .map(|r| r.iter().map(|v| format!("{:?}", v)).collect())
            .collect(),
        Err(e) => vec![vec![format!("ERR:{}", e)]],
    };
    let theirs = {
        let mut stmt = match conn.prepare(sql) {
            Ok(s) => s,
            Err(e) => return (ours, vec![vec![format!("ERR:{}", e)]]),
        };
        let ncols = stmt.column_count();
        let mut out = Vec::new();
        let mut rows = stmt.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            let mut r = Vec::new();
            for i in 0..ncols {
                let v: rusqlite::types::Value = row.get(i).unwrap_or(rusqlite::types::Value::Null);
                let s = match v {
                    rusqlite::types::Value::Null => "Null".to_string(),
                    rusqlite::types::Value::Integer(n) => format!("Integer({})", n),
                    rusqlite::types::Value::Real(f) => format!("Real({:?})", f),
                    rusqlite::types::Value::Text(t) => format!("Text({:?})", t),
                    rusqlite::types::Value::Blob(b) => format!("Blob({})", b.len()),
                };
                r.push(s);
            }
            out.push(r);
        }
        out
    };
    (ours, theirs)
}

fn check(db: &mut Database, conn: &rusqlite::Connection, cases: &[&str]) -> usize {
    let mut fails = 0;
    for sql in cases {
        let (ours, theirs) = both(db, conn, sql);
        if ours != theirs {
            fails += 1;
            println!("MISMATCH for: {sql}\n  ours:   {ours:?}\n  sqlite: {theirs:?}");
        }
    }
    fails
}

fn setup() -> (Database, rusqlite::Connection) {
    let mut db = Database::open_in_memory().unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let schema = [
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, dept TEXT, salary REAL)",
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, total INTEGER)",
        "CREATE TABLE items (id INTEGER PRIMARY KEY, order_id INTEGER, qty INTEGER)",
        "CREATE TABLE deleted_users (id INTEGER PRIMARY KEY, name TEXT)",
    ];
    for s in schema {
        db.execute(s, []).unwrap();
        conn.execute(s, []).unwrap();
    }
    // Identical data through plain SQL on both engines (no Value mixing).
    let data = [
        "INSERT INTO users (id, name, dept, salary) VALUES (1, 'alice', 'eng', 100.0)",
        "INSERT INTO users (id, name, dept, salary) VALUES (2, 'bob', 'eng', 90.0)",
        "INSERT INTO users (id, name, dept, salary) VALUES (3, 'carol', 'sales', 80.0)",
        "INSERT INTO users (id, name, dept, salary) VALUES (4, 'dan', 'sales', NULL)",
        "INSERT INTO users (id, name, dept, salary) VALUES (5, 'erin', NULL, 70.0)",
        // orders: user 1 has 3 orders (30, 10, 20), user 2 one, user 3 two, user 4 none, user 5 a NULL total
        "INSERT INTO orders (id, user_id, total) VALUES (1, 1, 30)",
        "INSERT INTO orders (id, user_id, total) VALUES (2, 1, 10)",
        "INSERT INTO orders (id, user_id, total) VALUES (3, 1, 20)",
        "INSERT INTO orders (id, user_id, total) VALUES (4, 2, 15)",
        "INSERT INTO orders (id, user_id, total) VALUES (5, 3, 25)",
        "INSERT INTO orders (id, user_id, total) VALUES (6, 3, 5)",
        "INSERT INTO orders (id, user_id, total) VALUES (7, 5, NULL)",
        "INSERT INTO items (id, order_id, qty) VALUES (1, 1, 2)",
        "INSERT INTO items (id, order_id, qty) VALUES (2, 1, 1)",
        "INSERT INTO items (id, order_id, qty) VALUES (3, 2, 5)",
        "INSERT INTO items (id, order_id, qty) VALUES (4, 4, 1)",
        "INSERT INTO items (id, order_id, qty) VALUES (5, 5, 3)",
        "INSERT INTO items (id, order_id, qty) VALUES (6, 5, 1)",
        "INSERT INTO items (id, order_id, qty) VALUES (7, 6, 4)",
    ];
    for s in data {
        db.execute(s, []).unwrap();
        conn.execute(s, []).unwrap();
    }
    (db, conn)
}

#[test]
fn correlated_scalar() {
    let (mut db, conn) = setup();
    let cases = [
        // Basic: total per user
        "SELECT name, (SELECT SUM(total) FROM orders o WHERE o.user_id = u.id) FROM users u ORDER BY u.id",
        // Correlated on a different column
        "SELECT name, (SELECT COUNT(*) FROM orders o WHERE o.user_id = u.id AND o.total > 12) FROM users u ORDER BY u.id",
        // Correlated with NULL outer value
        "SELECT name, (SELECT COUNT(*) FROM users u2 WHERE u2.dept = u.dept) FROM users u ORDER BY u.id",
        // Aggregate over correlated with NULLs in the aggregate
        "SELECT name, (SELECT MAX(total) FROM orders o WHERE o.user_id = u.id) FROM users u ORDER BY u.id",
        // Arithmetic around the subquery
        "SELECT name, (SELECT COALESCE(SUM(total), 0) FROM orders o WHERE o.user_id = u.id) * 2 FROM users u ORDER BY u.id",
        // Comparison in WHERE
        "SELECT name FROM users u WHERE (SELECT COUNT(*) FROM orders o WHERE o.user_id = u.id) >= 2 ORDER BY u.id",
        // Scalar subquery in SELECT with unqualified outer ref
        "SELECT name, (SELECT COUNT(*) FROM orders WHERE orders.user_id = users.id) FROM users ORDER BY id",
        // Nested: uncorrelated inside correlated
        "SELECT name, (SELECT COUNT(*) FROM orders o WHERE o.user_id = u.id AND o.total > (SELECT AVG(total) FROM orders)) FROM users u ORDER BY u.id",
    ];
    assert_eq!(
        check(&mut db, &conn, &cases),
        0,
        "correlated scalar mismatches"
    );
}

#[test]
fn correlated_exists() {
    let (mut db, conn) = setup();
    let cases = [
        "SELECT name FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id) ORDER BY u.id",
        "SELECT name FROM users u WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id AND o.total > 20) ORDER BY u.id",
        "SELECT name FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id AND o.total IS NULL) ORDER BY u.id",
        // EXISTS with join inside
        "SELECT name FROM users u WHERE EXISTS (SELECT 1 FROM orders o JOIN items i ON i.order_id = o.id WHERE o.user_id = u.id AND i.qty > 2) ORDER BY u.id",
        // NOT EXISTS anti-join pattern
        "SELECT name FROM users u WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id AND o.total IS NOT NULL) ORDER BY u.id",
    ];
    assert_eq!(
        check(&mut db, &conn, &cases),
        0,
        "correlated EXISTS mismatches"
    );
}

#[test]
fn correlated_in() {
    let (mut db, conn) = setup();
    let cases = [
        // Correlated IN: users whose id appears among orders with big totals
        "SELECT name FROM users u WHERE u.id IN (SELECT o.user_id FROM orders o WHERE o.total > 20) ORDER BY u.id",
        // Correlated NOT IN (with NULL semantics!)
        "SELECT name FROM users u WHERE u.id NOT IN (SELECT o.user_id FROM orders o WHERE o.total IS NOT NULL) ORDER BY u.id",
        // IN where the subquery references the outer row
        "SELECT name FROM users u WHERE 'eng' IN (SELECT dept FROM users u2 WHERE u2.id = u.id) ORDER BY u.id",
        // Uncorrelated IN with NULL in list
        "SELECT name FROM users WHERE salary IN (SELECT total FROM orders) ORDER BY id",
        "SELECT name FROM users WHERE salary NOT IN (SELECT total FROM orders) ORDER BY id",
    ];
    assert_eq!(check(&mut db, &conn, &cases), 0, "correlated IN mismatches");
}

#[test]
fn correlated_nested_and_multi_level() {
    let (mut db, conn) = setup();
    let cases = [
        // Two-level nesting: correlated at both levels
        "SELECT name,
                (SELECT COUNT(*) FROM orders o
                 WHERE o.user_id = u.id
                   AND EXISTS (SELECT 1 FROM items i WHERE i.order_id = o.id AND i.qty > 2))
         FROM users u ORDER BY u.id",
        // Correlated subquery inside a correlated subquery
        "SELECT name,
                (SELECT SUM(qty) FROM items i
                 WHERE i.order_id IN (SELECT o.id FROM orders o WHERE o.user_id = u.id))
         FROM users u ORDER BY u.id",
        // Correlated in HAVING-ish context via subquery over subquery
        "SELECT dept, COUNT(*) FROM users u GROUP BY dept HAVING COUNT(*) > (SELECT COUNT(*) FROM users WHERE dept = 'eng') ORDER BY dept",
        // Correlated in ORDER BY expression
        "SELECT name FROM users u ORDER BY (SELECT COUNT(*) FROM orders o WHERE o.user_id = u.id), u.id",
    ];
    assert_eq!(
        check(&mut db, &conn, &cases),
        0,
        "nested correlated mismatches"
    );
}

#[test]
fn correlated_with_join_context() {
    let (mut db, conn) = setup();
    let cases = [
        // Outer scope is a JOIN: refs to both outer aliases
        "SELECT u.name, o.id FROM users u JOIN orders o ON o.user_id = u.id
         WHERE (SELECT COUNT(*) FROM items i WHERE i.order_id = o.id) >= 2
         ORDER BY o.id",
        // Correlated ref to the second outer table by alias
        "SELECT u.name, (SELECT SUM(i.qty) FROM items i WHERE i.order_id = o.id) AS sq
         FROM users u JOIN orders o ON o.user_id = u.id ORDER BY o.id",
        // LEFT JOIN + correlated over the right side (NULL rows)
        "SELECT u.name, (SELECT COUNT(*) FROM items i WHERE i.order_id = o.id)
         FROM users u LEFT JOIN orders o ON o.user_id = u.id AND o.total > 100 ORDER BY u.id",
    ];
    assert_eq!(
        check(&mut db, &conn, &cases),
        0,
        "join-context correlated mismatches"
    );
}

#[test]
fn correlated_dml() {
    let (mut db, conn) = setup();
    // UPDATE with correlated subquery in SET — compare change counts, then state
    db.execute("UPDATE users SET salary = (SELECT MAX(total) FROM orders o WHERE o.user_id = users.id) WHERE id <= 2", []).unwrap();
    conn.execute("UPDATE users SET salary = (SELECT MAX(total) FROM orders o WHERE o.user_id = users.id) WHERE id <= 2", []).unwrap();
    // verify both agree post-update
    let v1 = both(&mut db, &conn, "SELECT id, salary FROM users ORDER BY id");
    assert_eq!(v1.0, v1.1, "post-UPDATE state");
    // DELETE with correlated EXISTS
    db.execute("DELETE FROM users WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = users.id AND o.total < 10)", []).unwrap();
    conn.execute("DELETE FROM users WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = users.id AND o.total < 10)", []).unwrap();
    let v2 = both(&mut db, &conn, "SELECT id, name FROM users ORDER BY id");
    assert_eq!(v2.0, v2.1, "post-DELETE state");
    // INSERT ... SELECT with a correlated scalar
    db.execute("INSERT INTO deleted_users (id, name) SELECT id, name FROM users u WHERE (SELECT COUNT(*) FROM orders o WHERE o.user_id = u.id) = 0", []).unwrap();
    conn.execute("INSERT INTO deleted_users (id, name) SELECT id, name FROM users u WHERE (SELECT COUNT(*) FROM orders o WHERE o.user_id = u.id) = 0", []).unwrap();
    let v3 = both(
        &mut db,
        &conn,
        "SELECT id, name FROM deleted_users ORDER BY id",
    );
    assert_eq!(v3.0, v3.1, "post-INSERT state");
}

#[test]
fn explain_query_plan_rows() {
    let (mut db, _conn) = setup();
    // EXPLAIN QUERY PLAN returns SQLite-schema rows and never executes.
    let rows = db
        .query("EXPLAIN QUERY PLAN SELECT name FROM users WHERE id = 1", [])
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0][3],
        Value::Text("SEARCH users USING INTEGER PRIMARY KEY (rowid=?)".into())
    );
    // Join: two SEARCH rows (needs the join-key index for INLJ).
    db.execute("CREATE INDEX idx_orders_user ON orders(user_id)", [])
        .unwrap();
    let rows = db.query(
        "EXPLAIN QUERY PLAN SELECT u.name FROM users u JOIN orders o ON o.user_id = u.id WHERE u.id = 1",
        [],
    ).unwrap();
    assert_eq!(rows.len(), 2);
    let details: Vec<String> = rows
        .iter()
        .map(|r| match &r[3] {
            Value::Text(t) => t.as_str().to_string(),
            _ => String::new(),
        })
        .collect();
    assert!(
        details
            .iter()
            .any(|d| d.starts_with("SEARCH u USING INTEGER PRIMARY KEY")),
        "{details:?}"
    );
    assert!(
        details
            .iter()
            .any(|d| d.starts_with("SEARCH o USING INDEX") || d == "SCAN o"),
        "{details:?}"
    );
    // ORDER BY emits the temp b-tree note.
    let rows = db
        .query(
            "EXPLAIN QUERY PLAN SELECT name FROM users ORDER BY name",
            [],
        )
        .unwrap();
    let details: Vec<String> = rows
        .iter()
        .map(|r| match &r[3] {
            Value::Text(t) => t.as_str().to_string(),
            _ => String::new(),
        })
        .collect();
    assert!(
        details.iter().any(|d| d == "USE TEMP B-TREE FOR ORDER BY"),
        "{details:?}"
    );
    // EXPLAIN must not mutate: the UPDATE inside is only planned.
    let before = db.query("SELECT COUNT(*) FROM users", []).unwrap();
    db.query("EXPLAIN QUERY PLAN UPDATE users SET salary = 1.0", [])
        .unwrap();
    let after = db.query("SELECT COUNT(*) FROM users", []).unwrap();
    assert_eq!(before, after);
}
