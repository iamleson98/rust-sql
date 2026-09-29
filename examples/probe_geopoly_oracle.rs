//! Dev probe round 2: geopoly error paths, plans, and formatting edges.

use rusqlite::Connection;

fn run(db: &Connection, _label: &str, sql: &str) {
    println!("--- {}", sql);
    match db.execute_batch(sql) {
        Ok(_) => {}
        Err(e) => println!("    ERR: {}", e),
    }
}

fn rows(db: &Connection, sql: &str) {
    println!("--- {}", sql);
    let mut stmt = match db.prepare(sql) {
        Ok(s) => s,
        Err(e) => {
            println!("    ERR: {}", e);
            return;
        }
    };
    let names: Vec<String> = stmt
        .column_names()
        .into_iter()
        .map(|s| s.to_string())
        .collect();
    println!("    COLS: {:?}", names);
    let mut rows = match stmt.query([]) {
        Ok(r) => r,
        Err(e) => {
            println!("    ERR: {}", e);
            return;
        }
    };
    while let Ok(Some(row)) = rows.next() {
        let mut cells = Vec::new();
        for i in 0..names.len() {
            let v = match row.get_ref(i) {
                Ok(rusqlite::types::ValueRef::Null) => "NULL".to_string(),
                Ok(rusqlite::types::ValueRef::Integer(x)) => format!("I:{}", x),
                Ok(rusqlite::types::ValueRef::Real(x)) => format!("R:{:?}", x),
                Ok(rusqlite::types::ValueRef::Text(t)) => {
                    format!("T:{}", String::from_utf8_lossy(t))
                }
                Ok(rusqlite::types::ValueRef::Blob(b)) => format!("B:{}", b.len()),
                Err(e) => format!("ERR:{}", e),
            };
            cells.push(v);
        }
        println!("    {}", cells.join(" | "));
    }
}

fn main() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE VIRTUAL TABLE g USING geopoly(a,b)")
        .unwrap();
    db.execute_batch("INSERT INTO g(_shape) VALUES ('[[0,0],[5,0],[5,5],[0,5],[0,0]]')")
        .unwrap();

    println!("===== error paths (execute_batch so errors surface)");
    run(&db, "invalid text", "INSERT INTO g(_shape) VALUES ('nope')");
    run(&db, "NULL shape", "INSERT INTO g(_shape) VALUES (NULL)");
    run(&db, "no shape", "INSERT INTO g(a) VALUES (1)");
    run(
        &db,
        "open poly",
        "INSERT INTO g(_shape) VALUES ('[[0,0],[5,0],[5,5],[0,5]]')",
    );
    run(&db, "number", "INSERT INTO g(_shape) VALUES (42)");
    run(
        &db,
        "valid",
        "INSERT INTO g(_shape) VALUES ('[[1,1],[2,1],[2,2],[1,2],[1,1]]')",
    );
    run(
        &db,
        "dup rowid",
        "INSERT INTO g(rowid, _shape) VALUES (1, '[[9,9],[9.5,9],[9.5,9.5],[9,9.5],[9,9]]')",
    );
    rows(&db, "SELECT rowid FROM g");
    run(
        &db,
        "update to invalid",
        "UPDATE g SET _shape='bad' WHERE rowid=1",
    );
    run(
        &db,
        "update ok",
        "UPDATE g SET _shape='[[0,0],[1,0],[1,1],[0,1],[0,0]]' WHERE rowid=1",
    );
    run(&db, "delete all", "DELETE FROM g");
    rows(&db, "SELECT count(*) FROM g");

    println!("===== plans");
    rows(
        &db,
        "EXPLAIN QUERY PLAN SELECT * FROM g WHERE geopoly_overlap(_shape, '[[0,0],[1,0],[1,1],[0,1],[0,0]]')",
    );
    rows(
        &db,
        "EXPLAIN QUERY PLAN SELECT * FROM g WHERE geopoly_within(_shape, '[[0,0],[1,0],[1,1],[0,1],[0,0]]')",
    );
    rows(&db, "EXPLAIN QUERY PLAN SELECT * FROM g WHERE rowid=5");
    rows(&db, "EXPLAIN QUERY PLAN SELECT * FROM g");
    rows(
        &db,
        "EXPLAIN QUERY PLAN SELECT * FROM g WHERE geopoly_overlap(a, '[[0,0],[1,0],[1,1],[0,1],[0,0]]')",
    );
    rows(
        &db,
        "EXPLAIN QUERY PLAN SELECT * FROM g WHERE geopoly_overlap('[[0,0],[1,0],[1,1],[0,1],[0,0]]', _shape)",
    );
    rows(
        &db,
        "EXPLAIN QUERY PLAN SELECT * FROM g WHERE _shape MATCH 'x'",
    );
    rows(
        &db,
        "EXPLAIN QUERY PLAN SELECT * FROM g WHERE geopoly_overlap(_shape, NULL)",
    );
    // invalid query polygon in WHERE: plan + result
    rows(
        &db,
        "SELECT rowid FROM g WHERE geopoly_overlap(_shape, 'not a polygon')",
    );
    rows(
        &db,
        "SELECT rowid FROM g WHERE geopoly_within(_shape, 'not a polygon')",
    );

    println!("===== float formatting edges");
    rows(
        &db,
        "SELECT geopoly_json('[[1e20,0],[1,0],[1,1],[1e20,0]]')",
    );
    rows(
        &db,
        "SELECT geopoly_json('[[1e-7,0],[1,0],[1,1],[1e-7,0]]')",
    );
    rows(
        &db,
        "SELECT geopoly_json('[[1e-5,0],[1,0],[1,1],[1e-5,0]]')",
    );
    rows(
        &db,
        "SELECT geopoly_json('[[1234567.0,0],[1,0],[1,1],[1234567.0,0]]')",
    );
    rows(
        &db,
        "SELECT geopoly_json('[[123456.7,0],[1,0],[1,1],[123456.7,0]]')",
    );
    rows(
        &db,
        "SELECT geopoly_json('[[0.09765625,0],[1,0],[1,1],[0.09765625,0]]')",
    );
    rows(
        &db,
        "SELECT geopoly_json('[[0.1,0.2],[1,0],[1,1],[0.1,0.2]]')",
    );
    rows(&db, "SELECT geopoly_svg('[[1e20,0],[1,0],[1,1],[1e20,0]]')");
    rows(
        &db,
        "SELECT geopoly_svg('[[0.5,0.25],[1,0],[1,1],[0.5,0.25]]')",
    );
    rows(&db, "SELECT geopoly_json(geopoly_regular(0,0,1e40,4))");
    rows(&db, "SELECT geopoly_area(geopoly_regular(0,0,1e40,4))");
    rows(
        &db,
        "SELECT geopoly_json('[[1e400,0],[1,0],[1,1],[1e400,0]]')",
    );
    rows(&db, "SELECT geopoly_json('-0.0' || '')");

    println!("===== aux columns with types + SELECT *");
    run(
        &db,
        "typed aux",
        "CREATE VIRTUAL TABLE t USING geopoly(name TEXT, n INTEGER)",
    );
    rows(&db, "PRAGMA table_info(t)");
    run(
        &db,
        "ins",
        "INSERT INTO t(_shape, name, n) VALUES ('[[0,0],[1,0],[1,1],[0,1],[0,0]]', 'x', '5')",
    );
    rows(
        &db,
        "SELECT rowid, name, n, typeof(n), typeof(_shape) FROM t",
    );
    rows(&db, "SELECT typeof(_shape), geopoly_json(_shape) FROM t");

    println!("===== rtree rowid conflict message (for rtree parity)");
    run(
        &db,
        "rt",
        "CREATE VIRTUAL TABLE rr USING rtree(id, x1, x2, y1, y2)",
    );
    run(&db, "ins1", "INSERT INTO rr VALUES (1, 0, 1, 0, 1)");
    run(&db, "dup", "INSERT INTO rr VALUES (1, 2, 3, 2, 3)");
    run(
        &db,
        "null coord",
        "INSERT INTO rr VALUES (2, NULL, 1, 0, 1)",
    );
    run(&db, "mismatch", "INSERT INTO rr VALUES (3, 5, 2, 0, 1)");
    rows(&db, "SELECT rowid FROM rr");

    println!("===== geopoly UPDATE rowid move + nochange");
    run(&db, "seed", "CREATE VIRTUAL TABLE m USING geopoly(lbl)");
    run(
        &db,
        "i1",
        "INSERT INTO m(_shape, lbl) VALUES ('[[0,0],[1,0],[1,1],[0,1],[0,0]]', 'a')",
    );
    run(
        &db,
        "i2",
        "INSERT INTO m(rowid, _shape, lbl) VALUES (5, '[[0,0],[2,0],[2,2],[0,2],[0,0]]', 'b')",
    );
    run(&db, "move", "UPDATE m SET rowid=7 WHERE rowid=5");
    rows(&db, "SELECT rowid, lbl FROM m");
    run(&db, "set-a-only", "UPDATE m SET lbl='c' WHERE rowid=7");
    rows(&db, "SELECT rowid, lbl, geopoly_json(_shape) FROM m");

    println!("===== persistence/reopen via file");
    drop(db);
    let db2 = Connection::open("/tmp/geopoly_probe.db").unwrap();
    db2.execute_batch("DROP TABLE IF EXISTS g2").unwrap();
    db2.execute_batch("CREATE VIRTUAL TABLE g2 USING geopoly(a)")
        .unwrap();
    db2.execute_batch(
        "INSERT INTO g2(_shape, a) VALUES ('[[0,0],[5,0],[5,5],[0,5],[0,0]]', 'hello')",
    )
    .unwrap();
    drop(db2);
    let db3 = Connection::open("/tmp/geopoly_probe.db").unwrap();
    rows(&db3, "SELECT rowid, a, geopoly_json(_shape) FROM g2");
    rows(
        &db3,
        "SELECT rowid FROM g2 WHERE geopoly_overlap(_shape, '[[4,4],[6,4],[6,6],[4,6],[4,4]]')",
    );
    let _ = std::fs::remove_file("/tmp/geopoly_probe.db");

    println!("==== done");
}
