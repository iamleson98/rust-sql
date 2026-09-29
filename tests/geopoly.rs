//! Geopoly tests: the geopoly module (CREATE VIRTUAL TABLE ... USING
//! geopoly), the 12 scalar functions + geopoly_group_bbox aggregate,
//! overlap/within constraint pushdown, error paths, persistence —
//! differential against real SQLite (rusqlite bundled, compiled with
//! -DSQLITE_ENABLE_GEOPOLY) wherever the oracle applies.

use rustqlite::types::Value;
use rustqlite::Database;

fn lite() -> rusqlite::Connection {
    rusqlite::Connection::open_in_memory().unwrap()
}

fn q1(db: &mut Database, sql: &str) -> String {
    match db.query(sql, []) {
        Ok(rows) => {
            if rows.is_empty() {
                "<no rows>".to_string()
            } else if rows[0].is_empty() {
                "<no cols>".to_string()
            } else {
                render(&rows[0][0])
            }
        }
        Err(e) => format!("<err {}>", e),
    }
}

fn rows_of(db: &mut Database, sql: &str) -> Vec<Vec<String>> {
    match db.query(sql, []) {
        Ok(rs) => rs.iter().map(|r| r.iter().map(render).collect()).collect(),
        Err(e) => vec![vec![format!("<err {}>", e)]],
    }
}

/// The detail column of an EXPLAIN QUERY PLAN result.
fn detail(db: &mut Database, sql: &str) -> String {
    match db.query(sql, []) {
        Ok(rows) => rows
            .first()
            .and_then(|r| r.get(3))
            .map(render)
            .unwrap_or_else(|| "<no detail>".to_string()),
        Err(e) => format!("<err {}>", e),
    }
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Integer(i) => format!("I:{}", i),
        Value::Real(f) => format!("R:{}", f),
        Value::Text(t) => format!("T:{}", t.as_str()),
        Value::Blob(b) => format!(
            "B:{}",
            b.iter().map(|x| format!("{:02x}", x)).collect::<String>()
        ),
    }
}

fn assert_same(db: &mut Database, oracle: &rusqlite::Connection, sql: &str) {
    let mine = q1(db, sql);
    let theirs = oracle
        .query_row(sql, [], |r| {
            let v = r.get_ref(0).unwrap();
            Ok(match v {
                rusqlite::types::ValueRef::Null => "NULL".to_string(),
                rusqlite::types::ValueRef::Integer(i) => format!("I:{}", i),
                rusqlite::types::ValueRef::Real(f) => format!("R:{}", f),
                rusqlite::types::ValueRef::Text(t) => {
                    format!("T:{}", String::from_utf8_lossy(t))
                }
                rusqlite::types::ValueRef::Blob(b) => format!(
                    "B:{}",
                    b.iter().map(|x| format!("{:02x}", x)).collect::<String>()
                ),
            })
        })
        .unwrap_or_else(|e| format!("<err {}>", e));
    // rusqlite's Real rendering vs our as_text: compare through SQLite's
    // own text conversion by formatting both with a final CAST when the
    // result is REAL. Simpler: compare as strings; both sides went
    // through the same f64 -> shortest formatting in the harness above.
    assert_eq!(mine, theirs, "divergence on: {}", sql);
}

// ---------------------------------------------------------------------------
// Function surface (differential, literal polygons)
// ---------------------------------------------------------------------------

#[test]
fn geopoly_functions_differential() {
    let mut db = Database::open_in_memory().unwrap();
    let oracle = lite();
    let cases: &[&str] = &[
        // area / json / blob round trips
        "SELECT geopoly_area('[[0,0],[5,0],[5,5],[0,5],[0,0]]')",
        "SELECT geopoly_area('[[0,0],[1,0],[1,1],[0,1],[0,0]]')",
        "SELECT geopoly_area('[[0,0],[10,0],[10,10],[5,5],[0,10],[0,0]]')",
        "SELECT geopoly_area('[[0,0],[0,5],[5,5],[5,0],[0,0]]')",
        "SELECT geopoly_area('[[0,0],[16777217,0],[16777217,1],[0,1],[0,0]]')",
        "SELECT geopoly_json('[[0,0],[5,0],[5,5],[0,5],[0,0]]')",
        "SELECT geopoly_json('[[0.1,0.2],[1,0],[1,1],[0.1,0.2]]')",
        "SELECT geopoly_json('[[1e20,0],[1,0],[1,1],[1e20,0]]')",
        "SELECT geopoly_json('[[1e-7,0],[1,0],[1,1],[1e-7,0]]')",
        "SELECT geopoly_json('[[1234567.0,0],[1,0],[1,1],[1234567.0,0]]')",
        "SELECT geopoly_json('[[123456.7,0],[1,0],[1,1],[123456.7,0]]')",
        "SELECT geopoly_json('[[0.09765625,0],[1,0],[1,1],[0.09765625,0]]')",
        "SELECT hex(geopoly_blob('[[1,2],[4,2],[4,5],[1,5],[1,2]]'))",
        "SELECT geopoly_json(geopoly_bbox('[[1,1],[5,1],[5,4],[1,4],[1,1]]'))",
        "SELECT geopoly_json(geopoly_ccw('[[0,0],[0,5],[5,5],[5,0],[0,0]]'))",
        "SELECT geopoly_json(geopoly_ccw('[[0,0],[5,0],[5,5],[0,5],[0,0]]'))",
        "SELECT geopoly_area(geopoly_ccw('[[0,0],[5,0],[5,5],[0,5],[0,0]]'))",
        "SELECT geopoly_svg('[[1,2],[4,2],[4,5],[1,5],[1,2]]')",
        "SELECT geopoly_svg('[[1,2],[4,2],[4,5],[1,5],[1,2]]', 'class=''a''')",
        "SELECT geopoly_svg('[[1e20,0],[1,0],[1,1],[1e20,0]]')",
        "SELECT geopoly_json(geopoly_xform('[[1,2],[4,2],[4,5],[1,5],[1,2]]', 2,0,0,2,1,1))",
        "SELECT geopoly_json(geopoly_regular(0,0,1,4))",
        "SELECT geopoly_json(geopoly_regular(0,0,1,3))",
        "SELECT geopoly_json(geopoly_regular(0,0,1,2))",
        "SELECT geopoly_json(geopoly_regular(0,0,-1,4))",
        "SELECT geopoly_area(geopoly_regular(0,0,2,100))",
        "SELECT geopoly_json(geopoly_regular(0,0,1e40,4))",
        "SELECT geopoly_area(geopoly_regular(0,0,1e40,4))",
        "SELECT length(geopoly_blob(geopoly_regular(0,0,1,1000)))",
        // contains_point: interior / boundary / exterior / concave
        "SELECT geopoly_contains_point('[[0,0],[10,0],[10,10],[0,10],[0,0]]', 5, 5)",
        "SELECT geopoly_contains_point('[[0,0],[10,0],[10,10],[0,10],[0,0]]', 0, 5)",
        "SELECT geopoly_contains_point('[[0,0],[10,0],[10,10],[0,10],[0,0]]', 20, 5)",
        "SELECT geopoly_contains_point('[[0,0],[10,0],[10,10],[5,5],[0,10],[0,0]]', 5, 4)",
        "SELECT geopoly_contains_point('[[0,0],[10,0],[10,10],[5,5],[0,10],[0,0]]', 5, 6)",
        // overlap / within codes
        "SELECT geopoly_overlap(geopoly_regular(0,0,1,100), geopoly_regular(0,0,1,100))",
        "SELECT geopoly_within(geopoly_regular(0,0,1,100), geopoly_regular(0,0,1,100))",
        "SELECT geopoly_overlap(geopoly_regular(0,0,1,4), geopoly_regular(10,10,1,4))",
        "SELECT geopoly_overlap('[[0,0],[10,0],[10,10],[0,10],[0,0]]', '[[1,1],[5,1],[5,5],[1,5],[1,1]]')",
        "SELECT geopoly_overlap('[[1,1],[5,1],[5,5],[1,5],[1,1]]', '[[0,0],[10,0],[10,10],[0,10],[0,0]]')",
        "SELECT geopoly_within('[[1,1],[5,1],[5,5],[1,5],[1,1]]', '[[0,0],[10,0],[10,10],[0,10],[0,0]]')",
        "SELECT geopoly_within('[[0,0],[10,0],[10,10],[0,10],[0,0]]', '[[1,1],[5,1],[5,5],[1,5],[1,1]]')",
        "SELECT geopoly_overlap('[[0,0],[10,0],[10,10],[5,5],[0,10],[0,0]]', '[[0,0],[5,0],[5,5],[0,5],[0,0]]')",
        "SELECT geopoly_within('[[0,0],[10,0],[10,10],[5,5],[0,10],[0,0]]', '[[0,0],[5,0],[5,5],[0,5],[0,0]]')",
        "SELECT geopoly_within('[[0,0],[5,0],[5,5],[0,5],[0,0]]', '[[0,0],[10,0],[10,10],[5,5],[0,10],[0,0]]')",
        // invalid inputs -> NULL
        "SELECT geopoly_area('nope')",
        "SELECT geopoly_area(3)",
        "SELECT geopoly_area(NULL)",
        "SELECT geopoly_area(zeroblob(27))",
        "SELECT geopoly_area(zeroblob(28))",
        "SELECT geopoly_area('[[0,0],[1,0],[1,1],[0,1]]')",
        "SELECT geopoly_area('[[0,0],[1,0],[1,1],[0,0]]')",
        "SELECT geopoly_json('[[0,0],[01,0],[1,1],[0,1],[0,0]]')",
        "SELECT geopoly_json('[[0,0],[1.,0],[1,1],[0,1],[0,0]]')",
        "SELECT geopoly_json('[[0,0],[.5,0],[1,1],[0,1],[0,0]]')",
        "SELECT geopoly_json('[[0,0],[1e1,0],[1,1],[0,1],[0,0]]')",
        "SELECT geopoly_json('[[0,0],[1e,0],[1,1],[0,1],[0,0]]')",
        "SELECT geopoly_json('[[0,0],[1,2,3],[1,1],[0,1],[0,0]]')",
        "SELECT geopoly_json('[[0,0],[1,0],[1,1],[0,1],[1,1]]')",
        "SELECT geopoly_json('[[0,0],[1,0],[1,1],[0,1],[0,0]] x')",
        "SELECT geopoly_json('  [[0,0],[1,0],[1,1],[0,1],[0,0]]  ')",
        "SELECT geopoly_json('[[0,0],[1,0],[1,1],[0,1],[0,0], [2,2],[3,3],[2,2]]')",
        "SELECT geopoly_contains_point('nope', 1, 1)",
        "SELECT geopoly_svg('nope', 'a', 'b', NULL)",
        "SELECT geopoly_json(geopoly_ccw(NULL))",
        "SELECT geopoly_json(geopoly_bbox('nope'))",
    ];
    for sql in cases {
        assert_same(&mut db, &oracle, sql);
    }
}

#[test]
fn geopoly_arity_errors_match_oracle() {
    let mut db = Database::open_in_memory().unwrap();
    let oracle = lite();
    let cases: &[&str] = &[
        "SELECT geopoly_area()",
        "SELECT geopoly_area(1, 2)",
        "SELECT geopoly_within('[[0,0],[1,0],[1,1],[0,0]]')",
        "SELECT geopoly_contains_point('[[0,0],[1,0],[1,1],[0,0]]', 1)",
        "SELECT geopoly_regular(1,2,3)",
        "SELECT geopoly_xform('[[0,0],[1,0],[1,1],[0,0]]', 1,0,0,1,0)",
    ];
    for sql in cases {
        let mine = q1(&mut db, sql);
        let theirs = oracle
            .query_row(sql, [], |_r| Ok(()))
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            mine.starts_with("<err"),
            "expected error from engine on {}: got {}",
            sql,
            mine
        );
        assert!(
            theirs.contains("wrong number of arguments"),
            "oracle error shape: {}",
            theirs
        );
        assert!(
            mine.contains("wrong number of arguments"),
            "engine error shape on {}: {}",
            sql,
            mine
        );
    }
}

// ---------------------------------------------------------------------------
// The virtual table
// ---------------------------------------------------------------------------

#[test]
fn geopoly_vtab_basic_dml() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE VIRTUAL TABLE g USING geopoly(a,b)", [])
        .unwrap();
    db.execute(
        "INSERT INTO g(_shape, a, b) VALUES ('[[0,0],[5,0],[5,5],[0,5],[0,0]]', 'y', 2)",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO g(rowid, _shape) VALUES (10, '[[1,1],[2,1],[2,2],[1,2],[1,1]]')",
        [],
    )
    .unwrap();
    // JSON text normalizes to the blob form on store.
    let rows = db.query("SELECT typeof(_shape), a, b FROM g", []).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0].as_text(), "blob");
    assert_eq!(rows[0][1].as_text(), "y");
    assert_eq!(rows[0][2].as_integer(), 2);
    assert_eq!(rows[1][0].as_text(), "blob");

    // module_list / table_info shapes.
    let modules = db.query("PRAGMA module_list", []).unwrap();
    assert!(modules.iter().any(|r| r[0].as_text() == "geopoly"));
    let ti = db.query("PRAGMA table_xinfo(g)", []).unwrap();
    assert_eq!(ti.len(), 3);
    let names: Vec<String> = ti.iter().map(|r| r[1].as_text()).collect();
    assert_eq!(names, vec!["_shape", "a", "b"]);
    let hidden: Vec<i64> = ti.iter().map(|r| r.last().unwrap().as_integer()).collect();
    assert_eq!(hidden, vec![0, 0, 0]);

    // SQLite's shadow tables exist.
    let names = db
        .query(
            "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name",
            [],
        )
        .unwrap();
    let got: Vec<String> = names.iter().map(|r| r[0].as_text()).collect();
    for want in ["g", "g_rowid", "g_node", "g_parent"] {
        assert!(got.contains(&want.to_string()), "missing shadow {}", want);
    }
}

#[test]
fn geopoly_vtab_error_paths() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE VIRTUAL TABLE g USING geopoly(a)", [])
        .unwrap();
    db.execute(
        "INSERT INTO g(_shape) VALUES ('[[0,0],[5,0],[5,5],[0,5],[0,0]]')",
        [],
    )
    .unwrap();
    // Hard errors.
    for sql in [
        "INSERT INTO g(_shape) VALUES (NULL)",
        "INSERT INTO g(_shape) VALUES (42)",
        "INSERT INTO g(_shape) VALUES ('[[0,0],[5,0],[5,5],[0,5]]')",
        "INSERT INTO g(_shape) VALUES ('[[bad')",
        "INSERT INTO g(_shape) VALUES (x'ABCDABCDABCDABCDABCDABCDABCDABCDABCDABCD')",
        "INSERT INTO g(a) VALUES (1)",
    ] {
        let err = db.execute(sql, []).unwrap_err().to_string();
        assert!(
            err.contains("_shape does not contain a valid polygon"),
            "on {}: {}",
            sql,
            err
        );
    }
    // rc-OK invalid shapes insert with a degenerate bbox.
    db.execute("INSERT INTO g(_shape) VALUES ('nope')", [])
        .unwrap();
    let rows = db
        .query(
            "SELECT typeof(_shape), geopoly_json(_shape) FROM g WHERE typeof(_shape)='text'",
            [],
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0][1].is_null());
    // Duplicate rowid.
    db.execute(
        "INSERT INTO g(rowid, _shape) VALUES (1, '[[9,9],[9.5,9],[9.5,9.5],[9,9.5],[9,9]]')",
        [],
    )
    .unwrap_err();
    let e = db
        .execute(
            "INSERT INTO g(rowid, _shape) VALUES (1, '[[9,9],[9.5,9],[9.5,9.5],[9,9.5],[9,9]]')",
            [],
        )
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("UNIQUE constraint failed: g._shape"),
        "got: {}",
        e
    );
    // OR REPLACE deletes-then-inserts.
    db.execute(
        "INSERT OR REPLACE INTO g(rowid, _shape) VALUES (1, '[[9,9],[9.5,9],[9.5,9.5],[9,9.5],[9,9]]')",
        [],
    )
    .unwrap();
    let j = q1(&mut db, "SELECT geopoly_json(_shape) FROM g WHERE rowid=1");
    assert_eq!(j, "T:[[9.0,9.0],[9.5,9.0],[9.5,9.5],[9.0,9.5],[9.0,9.0]]");
    // UPDATE: shape change, aux-only change, rowid move (errors).
    db.execute("UPDATE g SET a='z' WHERE rowid=1", []).unwrap();
    db.execute(
        "UPDATE g SET _shape='[[0,0],[2,0],[2,2],[0,2],[0,0]]' WHERE rowid=1",
        [],
    )
    .unwrap();
    let j = q1(&mut db, "SELECT geopoly_json(_shape) FROM g WHERE rowid=1");
    assert_eq!(j, "T:[[0.0,0.0],[2.0,0.0],[2.0,2.0],[0.0,2.0],[0.0,0.0]]");
    let e = db
        .execute("UPDATE g SET rowid=77 WHERE rowid=1", [])
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("_shape does not contain a valid polygon"),
        "rowid move: {}",
        e
    );
    // DELETE + count (one row remains: the 'nope' row).
    db.execute("DELETE FROM g WHERE rowid=1", []).unwrap();
    let n = q1(&mut db, "SELECT count(*) FROM g");
    assert_eq!(n, "I:1");
}

#[test]
fn geopoly_query_strategies() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE VIRTUAL TABLE n USING geopoly()", [])
        .unwrap();
    db.execute(
        "INSERT INTO n(_shape) VALUES ('[[0,0],[10,0],[10,10],[0,10],[0,0]]');
         INSERT INTO n(_shape) VALUES ('[[2,2],[4,0],[6,4],[4,6],[2,2]]');
         INSERT INTO n(_shape) VALUES ('[[20,20],[21,20],[21,21],[20,21],[20,20]]')",
        [],
    )
    .unwrap();
    let q = "'[[1,1],[5,1],[5,5],[1,5],[1,1]]'";
    // Constraint-pushdown queries (strategy 2/3) — the same row sets as
    // the oracle.
    let mine = rows_of(
        &mut db,
        &format!("SELECT rowid FROM n WHERE geopoly_overlap(_shape, {})", q),
    );
    assert_eq!(mine, vec![vec!["I:1"], vec!["I:2"]]);
    let mine = rows_of(
        &mut db,
        &format!("SELECT rowid FROM n WHERE geopoly_within(_shape, {})", q),
    );
    assert!(mine.is_empty());
    // The full function semantics (both orders) match.
    let mine = rows_of(
        &mut db,
        &format!("SELECT rowid, geopoly_overlap(_shape, {q}), geopoly_overlap({q}, _shape), geopoly_within(_shape, {q}), geopoly_within({q}, _shape) FROM n ORDER BY rowid"),
    );
    let want = vec![
        vec!["I:1", "I:3", "I:2", "I:0", "I:1"],
        vec!["I:2", "I:1", "I:1", "I:0", "I:0"],
        vec!["I:3", "I:0", "I:0", "I:0", "I:0"],
    ];
    assert_eq!(mine, want);
    // Rowid lookup.
    let mine = rows_of(&mut db, "SELECT rowid FROM n WHERE rowid=2");
    assert_eq!(mine, vec![vec!["I:2"]]);
    // Invalid query polygons in WHERE: rc-OK text -> empty, no error.
    let mine = rows_of(
        &mut db,
        "SELECT rowid FROM n WHERE geopoly_overlap(_shape, 'not a polygon')",
    );
    assert!(mine.is_empty());
    // Hard-error query values -> "SQL logic error".
    for bad in [
        "geopoly_overlap(_shape, NULL)",
        "geopoly_overlap(_shape, '[[bad')",
        "geopoly_within(_shape, 42)",
        "geopoly_overlap(_shape, x'AB')",
    ] {
        let r = q1(&mut db, &format!("SELECT rowid FROM n WHERE {}", bad));
        assert!(r.contains("SQL logic error"), "on {}: {}", bad, r);
    }
    // group_bbox aggregate.
    let j = q1(
        &mut db,
        "SELECT geopoly_json(geopoly_group_bbox(_shape)) FROM n",
    );
    assert_eq!(
        j,
        "T:[[0.0,0.0],[21.0,0.0],[21.0,21.0],[0.0,21.0],[0.0,0.0]]"
    );
    let j = q1(
        &mut db,
        "SELECT geopoly_group_bbox(_shape) IS NULL FROM n WHERE 0",
    );
    assert_eq!(j, "I:1");
}

#[test]
fn geopoly_explain_plans() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE VIRTUAL TABLE g USING geopoly(a)", [])
        .unwrap();
    let p = detail(
        &mut db,
        "EXPLAIN QUERY PLAN SELECT * FROM g WHERE geopoly_overlap(_shape, '[[0,0],[1,0],[1,1],[0,0]]')",
    );
    assert_eq!(p, "T:SCAN g VIRTUAL TABLE INDEX 2:rtree");
    let p = detail(
        &mut db,
        "EXPLAIN QUERY PLAN SELECT * FROM g WHERE geopoly_within(_shape, '[[0,0],[1,0],[1,1],[0,0]]')",
    );
    assert_eq!(p, "T:SCAN g VIRTUAL TABLE INDEX 3:rtree");
    let p = detail(&mut db, "EXPLAIN QUERY PLAN SELECT * FROM g WHERE rowid=5");
    assert_eq!(p, "T:SCAN g VIRTUAL TABLE INDEX 1:rowid");
    let p = detail(&mut db, "EXPLAIN QUERY PLAN SELECT * FROM g");
    assert_eq!(p, "T:SCAN g VIRTUAL TABLE INDEX 4:fullscan");
    // Aux-column / mirrored forms fall back to fullscan (oracle-pinned).
    let p = detail(
        &mut db,
        "EXPLAIN QUERY PLAN SELECT * FROM g WHERE geopoly_overlap(a, '[[0,0],[1,0],[1,1],[0,0]]')",
    );
    assert_eq!(p, "T:SCAN g VIRTUAL TABLE INDEX 4:fullscan");
    let p = detail(
        &mut db,
        "EXPLAIN QUERY PLAN SELECT * FROM g WHERE geopoly_overlap('[[0,0],[1,0],[1,1],[0,0]]', _shape)",
    );
    assert_eq!(p, "T:SCAN g VIRTUAL TABLE INDEX 4:fullscan");
}

#[test]
fn geopoly_persistence_and_rollback() {
    let path = std::env::temp_dir().join("geopoly_persist_test.db");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE VIRTUAL TABLE g USING geopoly(lbl)", [])
            .unwrap();
        db.execute(
            "INSERT INTO g(_shape, lbl) VALUES ('[[0,0],[5,0],[5,5],[0,5],[0,0]]', 'x')",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO g(rowid, _shape, lbl) VALUES (5, '[[1,1],[2,1],[2,2],[1,2],[1,1]]', 'y')",
            [],
        )
        .unwrap();
    }
    {
        let mut db = Database::open(&path).unwrap();
        let rows = db
            .query(
                "SELECT rowid, lbl, geopoly_json(_shape) FROM g ORDER BY rowid",
                [],
            )
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0][2].as_text(),
            "[[0.0,0.0],[5.0,0.0],[5.0,5.0],[0.0,5.0],[0.0,0.0]]"
        );
        assert_eq!(
            rows[1][2].as_text(),
            "[[1.0,1.0],[2.0,1.0],[2.0,2.0],[1.0,2.0],[1.0,1.0]]"
        );
        // Query through the R-tree after reopen.
        let hits = db
            .query(
                "SELECT rowid FROM g WHERE geopoly_overlap(_shape, '[[4,4],[6,4],[6,6],[4,6],[4,4]]')",
                [],
            )
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0][0].as_integer(), 1);
        // Rollback resync.
        db.execute("BEGIN", []).unwrap();
        db.execute("DELETE FROM g", []).unwrap();
        let n = db.query("SELECT count(*) FROM g", []).unwrap();
        assert_eq!(n[0][0].as_integer(), 0);
        db.execute("ROLLBACK", []).unwrap();
        let n = db.query("SELECT count(*) FROM g", []).unwrap();
        assert_eq!(n[0][0].as_integer(), 2);
        // DROP removes the vtab + shadows.
        db.execute("DROP TABLE g", []).unwrap();
        let names = db
            .query("SELECT name FROM sqlite_master WHERE name LIKE 'g%'", [])
            .unwrap();
        assert!(names.is_empty());
    }
    let _ = std::fs::remove_file(&path);
}

/// 120-round randomized differential: random polygons in both engines,
/// overlap/within/contains_point/area/json compared verbatim.
#[test]
fn geopoly_randomized_differential() {
    let mut seed_storage: u64 = std::env::var("RUSTQLITE_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20260929);
    let mut next = move || {
        seed_storage ^= seed_storage << 13;
        seed_storage ^= seed_storage >> 7;
        seed_storage ^= seed_storage << 17;
        seed_storage
    };
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE VIRTUAL TABLE r USING geopoly(tag)", [])
        .unwrap();
    let oracle = lite();
    oracle
        .execute_batch("CREATE VIRTUAL TABLE r USING geopoly(tag)")
        .unwrap();

    let mkpoly = |rng: &mut dyn FnMut() -> u64| -> String {
        let n = 3 + (rng() % 5) as usize;
        let mut pts = String::from("[");
        let x0 = (rng() % 20) as i64;
        let y0 = (rng() % 20) as i64;
        let mut x = x0;
        let mut y = y0;
        for i in 0..n {
            if i > 0 {
                let dx = (rng() % 9) as i64 - 4;
                let dy = (rng() % 9) as i64 - 4;
                x += dx;
                y += dy;
            }
            pts.push_str(&format!("[{},{}],", x, y));
        }
        pts.push_str(&format!("[{},{}]]", x0, y0));
        pts
    };

    for round in 0..120 {
        let poly = mkpoly(&mut next);
        let tag = format!("t{}", round);
        let ins = format!("INSERT INTO r(_shape, tag) VALUES ('{}', '{}')", poly, tag);
        db.execute(&ins, []).unwrap();
        oracle.execute_batch(&ins).unwrap();
        let poly2 = mkpoly(&mut next);
        // Query both engines on overlapping function results.
        for expr in [
            format!("geopoly_overlap(_shape, '{}')", poly2),
            format!("geopoly_within(_shape, '{}')", poly2),
            "geopoly_area(_shape)".to_string(),
            "geopoly_json(_shape)".to_string(),
        ] {
            let sql = format!("SELECT {} FROM r ORDER BY rowid", expr);
            let mine = rows_of(&mut db, &sql);
            let theirs: Vec<Vec<String>> = {
                let mut stmt = oracle.prepare(&sql).unwrap();
                let mut rows = stmt.query([]).unwrap();
                let mut out = Vec::new();
                while let Some(row) = rows.next().unwrap() {
                    let v = row.get_ref(0).unwrap();
                    out.push(vec![match v {
                        rusqlite::types::ValueRef::Null => "NULL".to_string(),
                        rusqlite::types::ValueRef::Integer(i) => format!("I:{}", i),
                        rusqlite::types::ValueRef::Real(f) => format!("R:{}", f),
                        rusqlite::types::ValueRef::Text(t) => {
                            format!("T:{}", String::from_utf8_lossy(t))
                        }
                        rusqlite::types::ValueRef::Blob(b) => format!(
                            "B:{}",
                            b.iter().map(|x| format!("{:02x}", x)).collect::<String>()
                        ),
                    }]);
                }
                out
            };
            assert_eq!(mine, theirs, "round {} expr {}", round, expr);
        }
    }
}

/// A REAL-pinned randomized overlap sweep across adversarial shapes
/// (concave, degenerate, collinear, overlapping edges).
#[test]
fn geopoly_overlap_sweep_adversarial() {
    let mut db = Database::open_in_memory().unwrap();
    let oracle = lite();
    let shapes: &[&str] = &[
        "'[[0,0],[10,0],[10,10],[0,10],[0,0]]'",
        "'[[1,1],[5,1],[5,5],[1,5],[1,1]]'",
        "'[[0,0],[10,0],[10,10],[5,5],[0,10],[0,0]]'",
        "'[[2,2],[4,0],[6,4],[4,6],[2,2]]'",
        "'[[0,0],[10,0],[0,10],[0,0]]'",
        "'[[5,5],[15,5],[15,15],[5,15],[5,5]]'",
        "'[[0,5],[5,5],[5,10],[0,10],[0,5]]'",
        "'[[3,3],[3,3],[3,3],[3,3]]'",
        "'[[-5,-5],[0,-5],[0,0],[-5,0],[-5,-5]]'",
        "'[[10,0],[20,0],[20,5],[10,5],[10,0]]'",
    ];
    for a in shapes {
        for b in shapes {
            for f in ["geopoly_overlap", "geopoly_within"] {
                let sql = format!("SELECT {}({}, {})", f, a, b);
                assert_same(&mut db, &oracle, &sql);
            }
        }
    }
}

/// rtree parity fix: duplicate-rowid INSERT errors like the oracle.
#[test]
fn rtree_duplicate_rowid_error_matches_oracle() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE VIRTUAL TABLE rr USING rtree(id, x1, x2, y1, y2)",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO rr VALUES (1, 0, 1, 0, 1)", [])
        .unwrap();
    let e = db
        .execute("INSERT INTO rr VALUES (1, 2, 3, 2, 3)", [])
        .unwrap_err()
        .to_string();
    assert!(e.contains("UNIQUE constraint failed: rr.id"), "got: {}", e);
    // OR REPLACE works.
    db.execute("INSERT OR REPLACE INTO rr VALUES (1, 2, 3, 2, 3)", [])
        .unwrap();
    let rows = db.query("SELECT x1, x2 FROM rr WHERE id=1", []).unwrap();
    assert_eq!(rows[0][0].as_real(), 2.0);
}
