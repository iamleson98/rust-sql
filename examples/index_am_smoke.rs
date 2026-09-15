//! SMOKE TEST ONLY (development scratch — not part of the final suite):
//! quick end-to-end validation of the GIN inverted index and GiST
//! spatial index paths before the real test files are written.

use rustqlite::{Database, Value};

fn db() -> Database {
    Database::open_in_memory().unwrap()
}

fn rows_of(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    let rows = db.query(sql, []).unwrap();
    rows.iter().map(|r| r.to_vec()).collect()
}

fn main() {
    // ---------------- GIN inverted index ----------------
    let mut d = db();
    d.execute("CREATE TABLE docs(id INTEGER PRIMARY KEY, body TEXT)", [])
        .unwrap();
    d.execute(
        "INSERT INTO docs(id, body) VALUES (1,'the quick brown fox'), (2,'lazy dogs sleep'), (3,'quick foxes jump high'), (4,'brown bears run')",
        [],
    )
    .unwrap();

    // Expression-form gin index (to_tsvector over a column).
    d.execute(
        "CREATE INDEX docs_fts ON docs USING gin(to_tsvector('english', body))",
        [],
    )
    .unwrap();

    let r = rows_of(
        &d,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'quick') ORDER BY id",
    );
    println!("gin quick: {:?}", r);
    assert_eq!(r.len(), 2, "quick matches docs 1 and 3");

    let r = rows_of(
        &d,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'quick & fox') ORDER BY id",
    );
    println!("gin quick&fox: {:?}", r);
    assert_eq!(r.len(), 2, "foxes stems to fox -> docs 1 and 3");

    let r = rows_of(
        &d,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'quick | sleep') ORDER BY id",
    );
    assert_eq!(r.len(), 3);

    // Prefix
    let r = rows_of(
        &d,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'fox:*') ORDER BY id",
    );
    assert_eq!(r.len(), 2, "fox:* matches fox and foxes");

    // NOT: a & !fox
    let r = rows_of(
        &d,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'quick & !fox') ORDER BY id",
    );
    println!("gin quick&!fox: {:?}", r);
    assert_eq!(r.len(), 0, "both quick rows contain fox/foxes");

    // Unsatisfiable-positive: !fox alone -> full scan fallback
    let r = rows_of(
        &d,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', '!fox') ORDER BY id",
    );
    assert_eq!(r.len(), 2, "!fox -> docs 2 and 4");

    // UPDATE maintenance: change a body, re-query.
    d.execute("UPDATE docs SET body = 'sleepy cats' WHERE id = 1", [])
        .unwrap();
    let r = rows_of(
        &d,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'quick') ORDER BY id",
    );
    assert_eq!(r.len(), 1, "after update only doc 3 has quick");

    // DELETE maintenance.
    d.execute("DELETE FROM docs WHERE id = 3", []).unwrap();
    let r = rows_of(
        &d,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'quick')",
    );
    assert!(r.is_empty(), "quick gone after delete");

    // Column-form gin index over a stored/generated tsvector.
    let mut d2 = db();
    d2.execute(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, body TEXT, tsv TEXT GENERATED ALWAYS AS (to_tsvector('english', body)))",
        [],
    )
    .unwrap();
    d2.execute(
        "INSERT INTO t(id, body) VALUES (1,'hello world'), (2,'goodbye world'), (3,'hello again')",
        [],
    )
    .unwrap();
    d2.execute("CREATE INDEX t_fts ON t USING gin(tsv)", [])
        .unwrap();
    let r = rows_of(
        &d2,
        "SELECT id FROM t WHERE tsv @@ to_tsquery('english', 'hello') ORDER BY id",
    );
    assert_eq!(r.len(), 2);

    // Parameterized query.
    let out = d2
        .query(
            "SELECT id FROM t WHERE tsv @@ ? ORDER BY id",
            [Value::Text("'hello'".into())],
        )
        .unwrap();
    assert_eq!(out.len(), 2);
    println!("gin param OK");

    // Persistence round-trip: create, close, reopen, query.
    let path = std::env::temp_dir().join("rustqlite_gin_smoke.db");
    let _ = std::fs::remove_file(&path);
    {
        let mut dp = Database::open(&path).unwrap();
        dp.execute("CREATE TABLE docs(id INTEGER PRIMARY KEY, body TEXT)", [])
            .unwrap();
        dp.execute(
            "INSERT INTO docs VALUES (1,'alpha beta'), (2,'gamma delta')",
            [],
        )
        .unwrap();
        dp.execute(
            "CREATE INDEX di ON docs USING gin(to_tsvector('english', body))",
            [],
        )
        .unwrap();
    }
    {
        let dp = Database::open(&path).unwrap();
        let r = rows_of(&dp, "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'alpha')");
        assert_eq!(r.len(), 1, "gin survives reopen");
        // EXPLAIN shows the inverted scan.
        let plan = rows_of(&dp, "EXPLAIN SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'alpha')");
        let joined = format!("{:?}", plan);
        assert!(
            joined.contains("INVERTED"),
            "EXPLAIN shows inverted scan: {}",
            joined
        );
    }
    let _ = std::fs::remove_file(&path);

    // ---------------- GiST spatial index ----------------
    let mut d3 = db();
    d3.execute(
        "CREATE TABLE places(id INTEGER PRIMARY KEY, name TEXT, geom TEXT)",
        [],
    )
    .unwrap();
    let mut stmt_data: Vec<String> = Vec::new();
    let coords = [
        (0.0, 0.0, "origin"),
        (0.10, 0.0, "east"),
        (0.0, 0.10, "north"),
        (5.0, 5.0, "far"),
        (-0.05, -0.05, "southwest"),
    ];
    for (i, (x, y, _)) in coords.iter().enumerate() {
        stmt_data.push(format!("({},'{}','POINT({} {})')", i + 1, "p", x, y));
    }
    d3.execute(
        &format!("INSERT INTO places VALUES {}", stmt_data.join(",")),
        [],
    )
    .unwrap();
    d3.execute("CREATE INDEX places_gix ON places USING gist(geom)", [])
        .unwrap();

    // KNN: nearest 3 to (0,0).
    let r = rows_of(
        &d3,
        "SELECT id FROM places ORDER BY geom <-> ST_Point(0, 0) LIMIT 3",
    );
    println!("knn: {:?}", r);
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Integer(1), "origin is nearest");
    // 2nd/3rd: east dist 0.1, southwest dist ~0.0707 -> southwest then east.
    assert_eq!(r[1][0], Value::Integer(5), "southwest second");
    assert_eq!(r[2][0], Value::Integer(2), "east third");

    // KNN with WHERE residual.
    let r = rows_of(
        &d3,
        "SELECT id FROM places WHERE id <> 1 ORDER BY geom <-> ST_Point(0, 0) LIMIT 2",
    );
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Integer(5));
    assert_eq!(r[1][0], Value::Integer(2));

    // ST_DWithin range scan.
    let r = rows_of(
        &d3,
        "SELECT id FROM places WHERE ST_DWithin(geom, ST_Point(0, 0), 0.2) ORDER BY id",
    );
    println!("dwithin: {:?}", r);
    assert_eq!(r.len(), 4, "origin, east, north, southwest within 0.2");

    // `<-> < r` comparison form.
    let r = rows_of(
        &d3,
        "SELECT id FROM places WHERE geom <-> ST_Point(0, 0) < 0.2 ORDER BY id",
    );
    assert_eq!(r.len(), 4);

    // Non-point geometry: a polygon near origin — its bbox cells cover it.
    let mut d4 = db();
    d4.execute("CREATE TABLE zones(id INTEGER PRIMARY KEY, geom TEXT)", [])
        .unwrap();
    d4.execute(
        "INSERT INTO zones VALUES (1,'POLYGON((0.001 0.001, 0.009 0.001, 0.009 0.009, 0.001 0.009, 0.001 0.001))'), (2,'POLYGON((9 9, 9.1 9, 9.1 9.1, 9 9.1, 9 9))')",
        [],
    )
    .unwrap();
    d4.execute(
        "CREATE INDEX zones_gix ON zones USING gist(geom, 0.001)",
        [],
    )
    .unwrap();
    let r = rows_of(
        &d4,
        "SELECT id FROM zones ORDER BY geom <-> ST_Point(0, 0) LIMIT 1",
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Integer(1), "small polygon nearest");

    // EXPLAIN visibility for KNN.
    let plan = rows_of(
        &d3,
        "EXPLAIN SELECT id FROM places ORDER BY geom <-> ST_Point(0,0) LIMIT 3",
    );
    let joined = format!("{:?}", plan);
    assert!(joined.contains("KNN"), "EXPLAIN shows KNN: {}", joined);

    // Persistence round-trip for gist.
    let path = std::env::temp_dir().join("rustqlite_gist_smoke.db");
    let _ = std::fs::remove_file(&path);
    {
        let mut dp = Database::open(&path).unwrap();
        dp.execute("CREATE TABLE p(id INTEGER PRIMARY KEY, geom TEXT)", [])
            .unwrap();
        dp.execute(
            "INSERT INTO p VALUES (1,'POINT(0 0)'), (2,'POINT(3 4)')",
            [],
        )
        .unwrap();
        dp.execute("CREATE INDEX p_gix ON p USING gist(geom)", [])
            .unwrap();
    }
    {
        let dp = Database::open(&path).unwrap();
        let r = rows_of(
            &dp,
            "SELECT id FROM p ORDER BY geom <-> ST_Point(2, 2) LIMIT 1",
        );
        assert_eq!(r.len(), 1);
        // (3,4) is 2.24 from (2,2); (0,0) is 2.83 — id 2 is nearer.
        assert_eq!(r[0][0], Value::Integer(2), "gist survives reopen");
    }
    let _ = std::fs::remove_file(&path);

    println!("ALL SMOKE TESTS PASSED");
}
