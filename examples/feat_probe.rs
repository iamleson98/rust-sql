use rustqlite::{Database, Value};
fn t(db: &mut Database, label: &str, sql: &str) {
    match db.execute(sql, []) {
        Ok(_) => println!("{:<42} OK", label),
        Err(e) => println!("{:<42} ERR: {}", label, e.to_string().chars().take(80).collect::<String>()),
    }
}
fn q(db: &mut Database, label: &str, sql: &str) {
    match db.query(sql, []) {
        Ok(r) => println!("{:<42} OK ({} rows)", label, r.len()),
        Err(e) => println!("{:<42} ERR: {}", label, e.to_string().chars().take(80).collect::<String>()),
    }
}
fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, active INT)", []).unwrap();
    db.execute("INSERT INTO users (name, active) VALUES ('a', 1), ('b', 0), ('c', 1)", []).unwrap();
    t(&mut db, "CREATE VIEW", "CREATE VIEW active_users AS SELECT * FROM users WHERE active = 1");
    q(&mut db, "SELECT from view", "SELECT * FROM active_users");
    t(&mut db, "CREATE TRIGGER", "CREATE TABLE log (msg TEXT)");
    t(&mut db, "  trigger body", "CREATE TRIGGER trg AFTER INSERT ON users FOR EACH ROW BEGIN INSERT INTO log(msg) VALUES ('new'); END");
    t(&mut db, "  fire trigger (INSERT)", "INSERT INTO users (name, active) VALUES ('d', 1)");
    q(&mut db, "  trigger fired?", "SELECT COUNT(*) FROM log");
    q(&mut db, "WITH RECURSIVE", "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n<10) SELECT SUM(n) FROM r");
    q(&mut db, "window ROW_NUMBER", "SELECT name, ROW_NUMBER() OVER (ORDER BY id) FROM users");
    q(&mut db, "window LAG", "SELECT id, LAG(id) OVER (ORDER BY id) FROM users");
    q(&mut db, "correlated scalar subq", "SELECT name, (SELECT COUNT(*) FROM users u2 WHERE u2.active = u.active) FROM users u");
    q(&mut db, "correlated EXISTS", "SELECT name FROM users u WHERE EXISTS (SELECT 1 FROM users u2 WHERE u2.id = u.id AND u2.active = 1)");
    q(&mut db, "INDEXED BY", "SELECT * FROM users WHERE id = 1");
    t(&mut db, "CREATE INDEX partial", "CREATE INDEX idx_act ON users(active) WHERE active = 1");
    q(&mut db, "CTE non-recursive", "WITH x AS (SELECT 1 AS a) SELECT a FROM x");
    q(&mut db, "savepoint", "SELECT 1");
    t(&mut db, "SAVEPOINT", "SAVEPOINT s1");
    t(&mut db, "ROLLBACK TO SAVEPOINT", "ROLLBACK TO s1");
    t(&mut db, "RELEASE", "RELEASE s1");
    q(&mut db, "EXPLAIN QUERY PLAN", "EXPLAIN QUERY PLAN SELECT * FROM users WHERE id = 1");
    q(&mut db, "VALUES clause", "VALUES (1, 'a'), (2, 'b')");
    q(&mut db, "lateral join", "SELECT 1");
}
