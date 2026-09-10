//! Print the ENGINE's preupdate event stream for the same battery as
//! examples/preupdate_oracle.rs (diff them to find divergences).
fn fmt(v: &rustqlite::Value) -> String {
    match v {
        rustqlite::Value::Null => "NULL".into(),
        rustqlite::Value::Integer(i) => format!("i:{i}"),
        rustqlite::Value::Real(f) => format!("f:{f}"),
        rustqlite::Value::Text(t) => format!("t:{t}"),
        rustqlite::Value::Blob(b) => format!("b:{:?}", b),
    }
}

const SCRIPT: &[&str] = &[
    "CREATE TABLE t (a INTEGER, b TEXT, c REAL)",
    "INSERT INTO t VALUES (1, 'x', 1.5)",
    "INSERT INTO t VALUES (2, NULL, NULL), (3, 'z', 3.75)",
    "UPDATE t SET b = 'y' WHERE a = 1",
    "UPDATE t SET a = a + 10 WHERE a >= 2",
    "DELETE FROM t WHERE a = 12",
    "CREATE TABLE ipk (id INTEGER PRIMARY KEY, v TEXT)",
    "INSERT INTO ipk (id, v) VALUES (7, 'seven')",
    "INSERT INTO ipk (v) VALUES ('auto')",
    "UPDATE ipk SET id = 99 WHERE id = 7",
    "DELETE FROM ipk WHERE id = 99",
    "CREATE TABLE wr (k TEXT PRIMARY KEY, v INT) WITHOUT ROWID",
    "INSERT INTO wr VALUES ('a', 1), ('b', 2)",
    "UPDATE wr SET v = 2 WHERE k = 'a'",
    "DELETE FROM wr WHERE k = 'a'",
    "CREATE TABLE u (id INTEGER PRIMARY KEY, v TEXT)",
    "INSERT INTO u VALUES (1, 'one')",
    "INSERT INTO u VALUES (1, 'uno') ON CONFLICT (id) DO UPDATE SET v = 'one!'",
    "INSERT INTO u VALUES (1, 'x') ON CONFLICT DO NOTHING",
    "INSERT OR REPLACE INTO u VALUES (1, 'replaced')",
    "CREATE TABLE log (msg TEXT, n INTEGER); CREATE TABLE log2 (msg TEXT); \
     CREATE TRIGGER trg_ins AFTER INSERT ON u BEGIN INSERT INTO log VALUES ('ins', new.id); END; \
     CREATE TRIGGER trg_log AFTER INSERT ON log BEGIN INSERT INTO log2 VALUES ('nested'); END;",
    "INSERT INTO u VALUES (2, 'two')",
    "CREATE TABLE parent (id INTEGER PRIMARY KEY); \
     CREATE TABLE child (id INTEGER PRIMARY KEY, pid INT REFERENCES parent(id) ON DELETE CASCADE); \
     CREATE TABLE child2 (id INTEGER PRIMARY KEY, pid INT REFERENCES parent(id) ON DELETE SET NULL); \
     CREATE TABLE c_a (id INTEGER PRIMARY KEY, pid INT REFERENCES parent(id) ON DELETE SET NULL);",
    "INSERT INTO parent VALUES (10), (11)",
    "INSERT INTO child VALUES (1, 10), (2, 11)",
    "INSERT INTO child2 VALUES (1, 10)",
    "INSERT INTO c_a VALUES (1, 10)",
    "DELETE FROM parent WHERE id = 10",
    "UPDATE u SET v = 'nope' WHERE id = 999",
    "DELETE FROM u WHERE id = 999",
    "CREATE TABLE tr (a TEXT DEFAULT 'd', b INT DEFAULT NULL, c TEXT DEFAULT NULL)",
    "INSERT INTO tr DEFAULT VALUES",
    "UPDATE tr SET c = 'set' WHERE rowid = 1",
    "DELETE FROM tr WHERE rowid = 1",
    "CREATE TABLE ddl1 (a INT)",
    "CREATE INDEX ddl1_ix ON ddl1(a)",
    "ALTER TABLE ddl1 RENAME TO ddl1r",
    "DROP TABLE ddl1r",
    "CREATE TABLE tree (id INTEGER PRIMARY KEY, parent INT REFERENCES tree(id) ON DELETE CASCADE); \
     INSERT INTO tree VALUES (1, NULL), (2, 1), (3, 2);",
    "DELETE FROM tree WHERE id = 1",
];

fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("PRAGMA foreign_keys=ON", ()).unwrap();
    let events: &'static std::sync::Mutex<Vec<String>> =
        Box::leak(Box::new(std::sync::Mutex::new(Vec::new())));
    let ev = events;
    db.set_preupdate_hook(Some(Box::new(
        move |e: &rustqlite::preupdate::PreupdateEvent| {
            let op = match e.op {
                rustqlite::preupdate::PreupdateOp::Insert => "INSERT",
                rustqlite::preupdate::PreupdateOp::Delete => "DELETE",
                rustqlite::preupdate::PreupdateOp::Update => "UPDATE",
            };
            let count = e
                .old
                .as_ref()
                .or(e.new.as_ref())
                .map(|v| v.len())
                .unwrap_or(0);
            let mut line = format!(
                "{op}({}) {}.{} rowid={} count={} depth={}",
                e.op.code(),
                e.db,
                e.table,
                e.rowid,
                count,
                e.depth
            );
            if let Some(old) = &e.old {
                let vals: Vec<String> = old.iter().map(fmt).collect();
                line.push_str(&format!(" old=[{}]", vals.join(",")));
            }
            if let Some(new) = &e.new {
                let vals: Vec<String> = new.iter().map(fmt).collect();
                line.push_str(&format!(" new=[{}]", vals.join(",")));
            }
            ev.lock().unwrap().push(line);
        },
    )));
    for sql in SCRIPT {
        println!("-- {sql}");
        db.execute(sql, ()).unwrap();
    }
    for line in events.lock().unwrap().iter() {
        println!("{line}");
    }
}
