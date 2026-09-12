use rustqlite::{Database, Value};

fn show(db: &Database, sql: &str) {
    match db.query(sql, []) {
        Ok(rows) => {
            let s: Vec<String> = rows
                .iter()
                .map(|r| {
                    r.iter()
                        .map(|v| match v {
                            Value::Null => "NULL".into(),
                            Value::Integer(i) => format!("{}", i),
                            Value::Real(f) => format!("{:?}", f),
                            Value::Text(t) => format!("'{}'", t),
                            Value::Blob(b) => format!("blob({})", b.len()),
                        })
                        .collect::<Vec<_>>()
                        .join(" | ")
                })
                .collect();
            println!("{:60} => [{}]", sql, s.join(" ; "));
        }
        Err(e) => println!("{:60} => ERR {:?}", sql, e),
    }
}

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    for s in [
        "CREATE TABLE c(k, v)",
        "INSERT INTO c VALUES ('a',1),('a',5),('b',10)",
    ] {
        db.execute(s, []).unwrap();
    }

    println!("=== bare-column GROUP BY probes ===");
    show(&db, "SELECT k, v FROM c GROUP BY k");
    show(&db, "SELECT k, v FROM c GROUP BY k ORDER BY k");
    show(&db, "SELECT k, count(*), v FROM c GROUP BY k");
    show(&db, "SELECT k, v FROM c GROUP BY k HAVING k='b'");
    let con = rusqlite::Connection::open_in_memory().unwrap();
    con.execute_batch("CREATE TABLE c(k, v); INSERT INTO c VALUES ('a',1),('a',5),('b',10);")
        .unwrap();
    for sql in [
        "SELECT k, v FROM c GROUP BY k",
        "SELECT k, v FROM c GROUP BY k ORDER BY k",
        "SELECT k, count(*), v FROM c GROUP BY k",
        "SELECT k, v FROM c GROUP BY k HAVING v > 2 ORDER BY k",
        "SELECT k, v AS v2 FROM c GROUP BY k HAVING v2 > 2 ORDER BY k",
        "SELECT k, v AS v2 FROM c GROUP BY k HAVING v2 > 2 ORDER BY k",
    ] {
        let mut st = con.prepare(sql).unwrap();
        let n = st.column_count();
        let rows: Vec<String> = st
            .query_map([], |r| {
                let mut parts = Vec::new();
                for i in 0..n {
                    let v: rusqlite::types::Value = r.get(i)?;
                    parts.push(format!("{:?}", v));
                }
                Ok(parts.join(" | "))
            })
            .unwrap()
            .map(|x| x.unwrap())
            .collect();
        println!("{:60} => [{}]", sql, rows.join(" ; "));
    }

    for sql in [
        "SELECT k, min(v), v FROM c GROUP BY k",
        "SELECT k, max(v), v FROM c GROUP BY k",
        "SELECT k, min(v), max(v), v FROM c GROUP BY k",
        "SELECT count(*), v FROM c",
    ] {
        let mut st = con.prepare(sql).unwrap();
        let n = st.column_count();
        let rows: Vec<String> = st
            .query_map([], |r| {
                let mut parts = Vec::new();
                for i in 0..n {
                    let v: rusqlite::types::Value = r.get(i)?;
                    parts.push(format!("{:?}", v));
                }
                Ok(parts.join(" | "))
            })
            .unwrap()
            .map(|x| x.unwrap())
            .collect();
        println!("{:60} => [{}]", sql, rows.join(" ; "));
    }
    println!("=== HAVING probes ===");
    show(&db, "SELECT k, v FROM c GROUP BY k HAVING v > 2 ORDER BY k");
    show(
        &db,
        "SELECT k, v AS v2 FROM c GROUP BY k HAVING v2 > 2 ORDER BY k",
    );
    show(
        &db,
        "SELECT k, v AS v2 FROM c GROUP BY k HAVING v > 2 ORDER BY k",
    );
    show(
        &db,
        "SELECT k, max(v) FROM c GROUP BY k HAVING max(v) > 2 ORDER BY k",
    );

    println!("=== VALUES-in-FROM probes ===");
    show(&db, "SELECT * FROM (VALUES (1,'a'))");
    show(&db, "SELECT * FROM (VALUES (1,'a'),(2,'b'))");
    show(&db, "SELECT column1 FROM (VALUES (1),(2))");
    show(&db, "SELECT * FROM (SELECT 10 AS x, 20 AS y)");
    show(&db, "SELECT count(*) FROM (VALUES (1),(2),(3))");

    println!("=== FK probes ===");
    let mut db2 = Database::open_in_memory().unwrap();
    for s in [
        "PRAGMA foreign_keys=ON",
        "CREATE TABLE gp(id INTEGER PRIMARY KEY)",
        "CREATE TABLE p(id INTEGER PRIMARY KEY REFERENCES gp(id) ON UPDATE CASCADE)",
        "CREATE TABLE c(id INTEGER REFERENCES p(id) ON UPDATE CASCADE)",
        "INSERT INTO gp VALUES (1)",
        "INSERT INTO p VALUES (1)",
        "INSERT INTO c VALUES (1)",
    ] {
        db2.execute(s, []).unwrap();
    }
    show(&db2, "UPDATE gp SET id=100");
    show(&db2, "SELECT * FROM p");
    show(&db2, "SELECT * FROM c");

    println!("=== partial index update probe ===");
    let mut db3 = Database::open_in_memory().unwrap();
    for s in [
        "CREATE TABLE u(a, b)",
        "CREATE UNIQUE INDEX ui ON u(a) WHERE b > 0",
        "INSERT INTO u VALUES (1, 1)",
        "INSERT INTO u VALUES (2, -1)",
        "INSERT INTO u VALUES (2, -2)",
    ] {
        db3.execute(s, []).unwrap();
    }
    show(&db3, "UPDATE u SET b=5 WHERE b=-1");
    show(&db3, "UPDATE u SET b=7 WHERE b=-2");
    show(&db3, "SELECT * FROM u ORDER BY a, b");
    show(&db3, "SELECT * FROM ui ORDER BY 1, 2");
}
