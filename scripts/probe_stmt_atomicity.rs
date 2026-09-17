// Minimal repro: multi-row INSERT + constraint violation → statement atomicity
fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t1 (a TEXT, e BLOB, c INTEGER, PRIMARY KEY (e))",
        [],
    )
    .unwrap();
    let r = db.execute(
        "INSERT INTO t1 (a, e, c) VALUES ('alpha', x'A293', 50), (NULL, x'', 19), ('naive', x'', 7)",
        [],
    );
    println!("insert result: {:?}", r.map_err(|e| e.to_string()));
    let rows = db.query("SELECT count(*) FROM t1", []).unwrap();
    println!(
        "count after failed insert: {:?}",
        rows.first().unwrap().first()
    );

    let mut db2 = rustqlite::Database::open_in_memory().unwrap();
    db2.execute("CREATE TABLE u (id INTEGER PRIMARY KEY, v TEXT)", []).unwrap();
    db2.execute("CREATE UNIQUE INDEX uq ON u(v)", []).unwrap();
    let r2 = db2.execute("INSERT INTO u (v) VALUES ('x'), ('y'), ('x'), ('z')", []);
    println!("insert2 result: {:?}", r2.map_err(|e| e.to_string()));
    let rows2 = db2.query("SELECT count(*) FROM u", []).unwrap();
    println!(
        "count2 after failed insert: {:?}",
        rows2.first().unwrap().first()
    );

    let mut db3 = rustqlite::Database::open_in_memory().unwrap();
    db3.execute("CREATE TABLE n (a INTEGER, b TEXT NOT NULL)", []).unwrap();
    let r3 = db3.execute("INSERT INTO n VALUES (1, 'a'), (2, NULL), (3, 'c')", []);
    println!("insert3 result: {:?}", r3.map_err(|e| e.to_string()));
    let rows3 = db3.query("SELECT count(*) FROM n", []).unwrap();
    println!(
        "count3 after failed insert: {:?}",
        rows3.first().unwrap().first()
    );

    let mut db4 = rustqlite::Database::open_in_memory().unwrap();
    db4.execute("CREATE TABLE k (a INTEGER CHECK (a < 100))", []).unwrap();
    let r4 = db4.execute("INSERT INTO k VALUES (1), (2), (150), (4)", []);
    println!("insert4 result: {:?}", r4.map_err(|e| e.to_string()));
    let rows4 = db4.query("SELECT count(*) FROM k", []).unwrap();
    println!(
        "count4 after failed insert: {:?}",
        rows4.first().unwrap().first()
    );

    // Variant 5: multi-row insert, INTEGER PRIMARY KEY alias duplicate
    let mut db5 = rustqlite::Database::open_in_memory().unwrap();
    db5.execute("CREATE TABLE p (id INTEGER PRIMARY KEY, v TEXT)", []).unwrap();
    let r5 = db5.execute("INSERT INTO p VALUES (1, 'a'), (2, 'b'), (1, 'dup'), (4, 'd')", []);
    println!("insert5 result: {:?}", r5.map_err(|e| e.to_string()));
    let rows5 = db5.query("SELECT count(*) FROM p", []).unwrap();
    println!(
        "count5 after failed insert: {:?}",
        rows5.first().unwrap().first()
    );
}
