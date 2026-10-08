//! SLT-audit findings: minimal repros, engine vs bundled SQLite.

use rustqlite::Database;

fn both(label: &str, sql: &str) {
    // Engine
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (pk INTEGER PRIMARY KEY, col0 INT, col1 INT, col4 REAL)",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO t VALUES (16, 685, 3, 300.5), (17, 619, 4, 100.5)",
        [],
    )
    .unwrap();
    fn val(v: &rustqlite::Value) -> String {
        use rustqlite::Value::*;
        match v {
            Null => "NULL".into(),
            Integer(i) => i.to_string(),
            Real(f) => format!("{f}"),
            Text(t) => t.to_string(),
            Blob(b) => format!("X'{b:?}'"),
        }
    }
    let eng = match db.query(sql, []) {
        Ok(rows) => rows
            .iter()
            .map(|r| r.iter().map(val).collect::<Vec<_>>().join(","))
            .collect::<Vec<_>>()
            .join(" | "),
        Err(e) => format!("ERR: {e}"),
    };
    // SQLite
    let sq = {
        let rc = rusqlite::Connection::open_in_memory().unwrap();
        rc.execute_batch(
            "CREATE TABLE t (pk INTEGER PRIMARY KEY, col0 INT, col1 INT, col4 REAL);
            INSERT INTO t VALUES (16, 685, 3, 300.5), (17, 619, 4, 100.5);",
        )
        .unwrap();
        let mut stmt = rc.prepare(sql).unwrap();
        let ncols = stmt.column_count();
        let rows: Vec<String> = stmt
            .query_map([], move |r| {
                Ok((0..ncols)
                    .map(|i| {
                        let v: rusqlite::types::Value = match r.get(i) {
                            Ok(v) => v,
                            Err(_) => rusqlite::types::Value::Null,
                        };
                        match v {
                            rusqlite::types::Value::Null => "NULL".to_string(),
                            rusqlite::types::Value::Integer(i) => i.to_string(),
                            rusqlite::types::Value::Real(f) => format!("{f}"),
                            rusqlite::types::Value::Text(t) => t,
                            rusqlite::types::Value::Blob(b) => format!("X'{b:?}'"),
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(","))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        rows.join(" | ")
    };
    let tag = if eng == sq { "OK  " } else { "DIFF" };
    println!("{tag} {label}\n     eng: {eng}\n     sqli: {sq}");
}

fn main() {
    // A. Contradictory equality on the same column.
    both(
        "A1: col0=685 AND col0=619",
        "SELECT pk FROM t WHERE col0 = 685 AND col0 = 619",
    );
    both(
        "A2: reversed spelling",
        "SELECT pk FROM t WHERE 685 = col0 AND 619 = col0",
    );
    both(
        "A3: non-contradictory control",
        "SELECT pk FROM t WHERE col0 = 685 AND col0 = 685",
    );

    // B. NOT BETWEEN with a NULL bound (3-valued logic: x NOT BETWEEN
    // lo AND hi == (x < lo) OR (x > hi)).
    both(
        "B1: NOT BETWEEN NULL AND x",
        "SELECT 79 FROM t WHERE ( + 94 ) - + ( + 1 ) NOT BETWEEN NULL AND ( col1 + + 33 )",
    );
    both(
        "B2: plain between NULL (control)",
        "SELECT 79 FROM t WHERE ( + 94 ) - + ( + 1 ) BETWEEN NULL AND ( col1 + + 33 )",
    );

    // C. SELECT ALL / aggregate ALL.
    both("C1: SELECT ALL", "SELECT ALL pk FROM t");
    both("C2: MIN(ALL ...)", "SELECT min(ALL col0) FROM t");
    both(
        "C3: arithmetic ALL",
        "SELECT 21 + 33 - 9 / - MIN ( ALL ( 94 ) ) * - 46",
    );

    // D. The random/expr shape (sign folding in BETWEEN bounds).
    both(
        "D1: negated col BETWEEN",
        "SELECT ALL * FROM t WHERE - 62 NOT IN ( 9, - ( + + 67 ) ) AND - col0 * + 51 BETWEEN + 8 AND - - 95",
    );
    both(
        "D2: isolated BETWEEN",
        "SELECT pk FROM t WHERE - col0 * + 51 BETWEEN + 8 AND - - 95",
    );

    // A4. The full audit shape: contradiction + a range predicate.
    both(
        "A4: contradiction + col4 range",
        "SELECT pk FROM t WHERE col0 = 685 AND col0 = 619 AND ((col4 > 259.98))",
    );
    both(
        "A5: satisfiable + col4 range",
        "SELECT pk FROM t WHERE col0 = 685 AND col0 = 685 AND ((col4 > 259.98))",
    );
    both(
        "A6: single eq + col4 range",
        "SELECT pk FROM t WHERE col0 = 685 AND ((col4 > 259.98))",
    );
    // D-family isolation.
    both(
        "D3: literal BETWEEN true",
        "SELECT 5 WHERE 10 BETWEEN 8 AND 95",
    );
    both(
        "D4: literal BETWEEN false",
        "SELECT 5 WHERE -100 BETWEEN 8 AND 95",
    );
    both(
        "D5: neg-col BETWEEN",
        "SELECT 5 WHERE - 1 * 10 BETWEEN + 8 AND - - 95",
    );
    both(
        "D6: neg-col BETWEEN simple",
        "SELECT 5 WHERE - 10 BETWEEN 8 AND 95",
    );

    // E. IN (SELECT ...) shape (the "expected error, got success" class:
    // SQLite REJECTS multi-column subquery in IN).
    both(
        "E1: IN (SELECT x,y)",
        "SELECT 5 WHERE 1 IN (SELECT col0, col1 FROM t)",
    );
    both("E2: IN (SELECT *", "SELECT 5 WHERE 1 IN (SELECT * FROM t)");
}
