//! SQLite's BARE-COLUMN semantics in aggregate queries (differential,
//! against bundled SQLite):
//!
//! - A bare column in an aggregate query's SELECT list / HAVING / ORDER
//!   BY takes the value from ONE row of its group (SQLite docs:
//!   "arbitrary"; in practice the first row in scan order).
//! - When the query's ONLY aggregate is min() or max(), the bare columns
//!   take their values from the row ACHIEVING the extreme.
//! - `SELECT *` over a GROUP BY expands to every FROM column — the
//!   non-key columns are bare representatives.
//! - ORDER-ONLY aggregates (`ORDER BY sum(v)` with sum nowhere in the
//!   SELECT list) ride along on the Aggregate node's output.
//!
//! The multi-aggregate (>1 non-bare aggregate) representative is
//! implementation-defined in SQLite itself ("arbitrary row") — those
//! shapes are deliberately NOT pinned here.

use rustqlite::{Database, Value};

fn mem() -> Database {
    Database::open_in_memory().unwrap()
}

fn lite() -> rusqlite::Connection {
    rusqlite::Connection::open_in_memory().unwrap()
}

fn engine_strs(rows: &[Vec<Value>]) -> Vec<String> {
    rows.iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    Value::Null => "Null".to_string(),
                    Value::Integer(i) => format!("Integer({})", i),
                    Value::Real(f) => format!("Real({:?})", f),
                    Value::Text(t) => format!("Text({:?})", t.to_string()),
                    Value::Blob(b) => format!("Blob({:?})", b),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn lite_rows(con: &rusqlite::Connection, sql: &str) -> Result<Vec<String>, rusqlite::Error> {
    let mut st = con.prepare(sql)?;
    let n = st.column_count();
    let rows: Vec<String> = st
        .query_map([], |r| {
            let mut parts = Vec::new();
            for i in 0..n {
                let v: rusqlite::types::Value = r.get(i)?;
                parts.push(format!("{:?}", v));
            }
            Ok(parts.join("|"))
        })?
        .map(|r| r.unwrap())
        .collect();
    Ok(rows)
}

fn diff(con: &rusqlite::Connection, db: &Database, sql: &str) {
    let ours = match db.query(sql, []) {
        Ok(r) => engine_strs(&r),
        Err(e) => vec![format!("__ERR__{:?}", e)],
    };
    let theirs = match lite_rows(con, sql) {
        Ok(r) => r,
        Err(e) => vec![format!("__ERR__{:?}", e)],
    };
    assert_eq!(
        ours, theirs,
        "MISMATCH on: {}\n  ours:   {:?}\n  theirs: {:?}",
        sql, ours, theirs
    );
}

fn setup_both(script: &[&str]) -> (Database, rusqlite::Connection) {
    let mut db = mem();
    let con = lite();
    for s in script {
        let r1 = db.execute(s, []);
        let r2 = con.execute_batch(s);
        if let Err(e) = r2 {
            panic!("SQLite rejected setup {:?}: {:?}", s, e);
        }
        r1.unwrap_or_else(|e| panic!("engine rejected setup {:?}: {:?}", s, e));
    }
    (db, con)
}

// ---------------------------------------------------------------------------
// 1. Bare columns in the projection (first-seen representative)
// ---------------------------------------------------------------------------

#[test]
fn bare_column_basic_group_by() {
    let (db, con) = setup_both(&[
        "CREATE TABLE c(k, v)",
        "INSERT INTO c VALUES ('a',1),('a',5),('b',10),('b',2)",
    ]);
    diff(&con, &db, "SELECT k, v FROM c GROUP BY k ORDER BY k");
    diff(&con, &db, "SELECT k, v FROM c GROUP BY k");
    diff(
        &con,
        &db,
        "SELECT k, count(*), v FROM c GROUP BY k ORDER BY k",
    );
    diff(&con, &db, "SELECT count(*), v FROM c");
    // Multiple bare columns (both representatives from the first row).
    diff(&con, &db, "SELECT k, v, v FROM c GROUP BY k ORDER BY k");
}

#[test]
fn bare_column_expression() {
    let (db, con) = setup_both(&[
        "CREATE TABLE c(k, v)",
        "INSERT INTO c VALUES ('a',1),('a',5),('b',10),('b',2)",
    ]);
    diff(&con, &db, "SELECT k, v + 100 FROM c GROUP BY k ORDER BY k");
    diff(
        &con,
        &db,
        "SELECT k, v * 2 + 1 FROM c GROUP BY k ORDER BY k",
    );
    diff(&con, &db, "SELECT k, -v FROM c GROUP BY k ORDER BY k");
}

// ---------------------------------------------------------------------------
// 2. The single-min/max rule (representative = the extreme-achieving row)
// ---------------------------------------------------------------------------

#[test]
fn bare_column_min_rule() {
    let (db, con) = setup_both(&[
        "CREATE TABLE c(k, v)",
        "INSERT INTO c VALUES ('a',1),('a',5),('b',10),('b',2)",
    ]);
    diff(
        &con,
        &db,
        "SELECT k, min(v), v FROM c GROUP BY k ORDER BY k",
    );
    diff(&con, &db, "SELECT min(v), v FROM c");
    // min over a different column than the bare one.
    diff(
        &con,
        &db,
        "SELECT k, min(k), v FROM c GROUP BY k ORDER BY k",
    );
}

#[test]
fn bare_column_max_rule() {
    let (db, con) = setup_both(&[
        "CREATE TABLE c(k, v)",
        "INSERT INTO c VALUES ('a',1),('a',5),('b',10),('b',2)",
    ]);
    diff(
        &con,
        &db,
        "SELECT k, max(v), v FROM c GROUP BY k ORDER BY k",
    );
    diff(&con, &db, "SELECT max(v), v FROM c");
}

#[test]
fn bare_column_min_rule_with_ties() {
    // Ties on the extreme: the FIRST achieving row wins (strict < / >).
    let (db, con) = setup_both(&[
        "CREATE TABLE c(k, v)",
        "INSERT INTO c VALUES ('a',5),('a',5),('a',1)",
    ]);
    diff(&con, &db, "SELECT k, min(v), v FROM c GROUP BY k");
    diff(&con, &db, "SELECT k, max(v), v FROM c GROUP BY k");
}

#[test]
fn bare_column_register_writer_rules() {
    // The LAST-declared plain min()/max() is the register WRITER: its
    // strict improvements overwrite the bare columns. Other aggregates
    // (count/sum) ride along without disabling the rule. All pinned
    // against bundled SQLite.
    let (db, con) = setup_both(&[
        "CREATE TABLE c(k, v)",
        "INSERT INTO c VALUES ('a',1),('a',5)",
    ]);
    // min declared last → the min writes (rows 1,5: min never improves
    // after row 1 → first-seen 1).
    diff(&con, &db, "SELECT k, max(v), min(v), v FROM c GROUP BY k");
    let (db, con) = setup_both(&[
        "CREATE TABLE c(k, v)",
        "INSERT INTO c VALUES ('a',5),('a',1)",
    ]);
    // Rows (5,1): the min improves at row 2 → v=1.
    diff(&con, &db, "SELECT k, max(v), min(v), v FROM c GROUP BY k");
    // max declared last → the max writes.
    diff(&con, &db, "SELECT k, min(v), max(v), v FROM c GROUP BY k");
    let (db, con) = setup_both(&[
        "CREATE TABLE c(k, v)",
        "INSERT INTO c VALUES ('a',1),('a',5)",
    ]);
    diff(&con, &db, "SELECT k, min(v), max(v), v FROM c GROUP BY k");
    // count rides along — the min still writes.
    let (db, con) = setup_both(&[
        "CREATE TABLE c(k, v)",
        "INSERT INTO c VALUES ('a',NULL),('a',5)",
    ]);
    diff(&con, &db, "SELECT k, count(*), min(v), v FROM c GROUP BY k");
    let (db, con) = setup_both(&[
        "CREATE TABLE c(k, v)",
        "INSERT INTO c VALUES ('a',5),('a',1)",
    ]);
    diff(&con, &db, "SELECT k, count(*), min(v), v FROM c GROUP BY k");
    diff(&con, &db, "SELECT k, sum(v), min(v), v FROM c GROUP BY k");
    // The writer is a min over a DIFFERENT column: its improvements
    // still write the bare v.
    let (db, con) = setup_both(&[
        "CREATE TABLE c(k, v, w)",
        "INSERT INTO c VALUES ('a',5,100),('a',1,200)",
    ]);
    diff(&con, &db, "SELECT k, min(v), max(w), v FROM c GROUP BY k");
}

#[test]
fn bare_column_min_max_no_rule() {
    // count() (not min/max) + bare: first-seen (deterministic in SQLite).
    let (db, con) = setup_both(&[
        "CREATE TABLE c(k, v)",
        "INSERT INTO c VALUES ('a',1),('a',5),('b',10)",
    ]);
    diff(
        &con,
        &db,
        "SELECT k, count(v), v FROM c GROUP BY k ORDER BY k",
    );
    diff(
        &con,
        &db,
        "SELECT k, sum(v), v FROM c GROUP BY k ORDER BY k",
    );
    // DISTINCT min: the single-extreme rule requires a plain min().
    diff(
        &con,
        &db,
        "SELECT k, min(DISTINCT v), v FROM c GROUP BY k ORDER BY k",
    );
}

// ---------------------------------------------------------------------------
// 3. HAVING on bare columns (plain and aliased)
// ---------------------------------------------------------------------------

#[test]
fn bare_column_having() {
    let (db, con) = setup_both(&[
        "CREATE TABLE c(k, v)",
        "INSERT INTO c VALUES ('a',1),('a',5),('b',10)",
    ]);
    diff(
        &con,
        &db,
        "SELECT k, v FROM c GROUP BY k HAVING v > 2 ORDER BY k",
    );
    diff(
        &con,
        &db,
        "SELECT k, v AS v2 FROM c GROUP BY k HAVING v2 > 2 ORDER BY k",
    );
    diff(
        &con,
        &db,
        "SELECT k, count(*), v AS v2 FROM c GROUP BY k HAVING v2 > 2 ORDER BY k",
    );
    // HAVING mixing an aggregate predicate and a bare-column predicate.
    diff(
        &con,
        &db,
        "SELECT k, v FROM c GROUP BY k HAVING count(*) > 1 AND v > 2 ORDER BY k",
    );
}

// ---------------------------------------------------------------------------
// 4. ORDER BY on bare columns (plain, aliased, DESC)
// ---------------------------------------------------------------------------

#[test]
fn bare_column_order_by() {
    let (db, con) = setup_both(&[
        "CREATE TABLE c(k, v)",
        "INSERT INTO c VALUES ('a',1),('a',5),('b',10),('b',2)",
    ]);
    diff(
        &con,
        &db,
        "SELECT k, v, count(*) FROM c GROUP BY k ORDER BY v DESC",
    );
    diff(
        &con,
        &db,
        "SELECT k, v, count(*) FROM c GROUP BY k ORDER BY v",
    );
    diff(
        &con,
        &db,
        "SELECT k, v AS vv, count(*) FROM c GROUP BY k ORDER BY vv",
    );
    // ORDER BY bare column that is NOT in the projection.
    diff(&con, &db, "SELECT k, count(*) FROM c GROUP BY k ORDER BY v");
    diff(
        &con,
        &db,
        "SELECT k, count(*) FROM c GROUP BY k ORDER BY v DESC",
    );
}

// ---------------------------------------------------------------------------
// 5. ORDER-ONLY aggregates (in ORDER BY but not in the SELECT list)
// ---------------------------------------------------------------------------

#[test]
fn order_only_aggregate() {
    let (db, con) = setup_both(&[
        "CREATE TABLE c(k, v)",
        "INSERT INTO c VALUES ('a',1),('a',5),('b',10),('b',2)",
    ]);
    diff(
        &con,
        &db,
        "SELECT k, count(*) FROM c GROUP BY k ORDER BY sum(v) DESC",
    );
    diff(&con, &db, "SELECT k FROM c GROUP BY k ORDER BY sum(v)");
    diff(&con, &db, "SELECT k FROM c GROUP BY k ORDER BY max(v), k");
    diff(
        &con,
        &db,
        "SELECT k FROM c GROUP BY k ORDER BY count(*) DESC, k",
    );
}

// ---------------------------------------------------------------------------
// 6. Star over GROUP BY
// ---------------------------------------------------------------------------

#[test]
fn star_over_group_by() {
    let (db, con) = setup_both(&[
        "CREATE TABLE c(k, v, w)",
        "INSERT INTO c VALUES ('a',1,'x'),('a',5,'y'),('b',10,'z'),('b',2,'w')",
    ]);
    diff(&con, &db, "SELECT * FROM c GROUP BY k ORDER BY k");
    diff(&con, &db, "SELECT c.* FROM c GROUP BY k ORDER BY k");
    diff(&con, &db, "SELECT *, count(*) FROM c GROUP BY k ORDER BY k");
    // Join star: both sides' columns, in FROM order.
    let (db, con) = setup_both(&[
        "CREATE TABLE t1(a, b)",
        "CREATE TABLE t2(p, q)",
        "INSERT INTO t1 VALUES (1,'x'),(1,'y'),(2,'z')",
        "INSERT INTO t2 VALUES (1,'p'),(2,'q')",
    ]);
    diff(
        &con,
        &db,
        "SELECT * FROM t1 JOIN t2 ON t2.p = t1.a GROUP BY t1.a ORDER BY t1.a",
    );
}

// ---------------------------------------------------------------------------
// 7. Composite / qualified shapes
// ---------------------------------------------------------------------------

#[test]
fn bare_column_qualified_join() {
    let (db, con) = setup_both(&[
        "CREATE TABLE a(id INTEGER PRIMARY KEY, tag TEXT, n INTEGER)",
        "CREATE TABLE b(id INTEGER PRIMARY KEY, a_id INTEGER REFERENCES a(id), v INTEGER)",
        "INSERT INTO a VALUES (1,'x',10),(2,'X',20),(3,'y',30)",
        "INSERT INTO b VALUES (10,1,1),(11,1,2),(12,2,3),(13,3,4)",
    ]);
    diff(
        &con,
        &db,
        "SELECT a.tag, b.v, count(*) FROM a JOIN b ON b.a_id = a.id GROUP BY a.tag ORDER BY a.tag",
    );
    diff(
        &con,
        &db,
        "SELECT a.tag, max(b.v), b.v FROM a JOIN b ON b.a_id = a.id GROUP BY a.tag ORDER BY a.tag",
    );
}

#[test]
fn bare_column_multi_key() {
    let (db, con) = setup_both(&[
        "CREATE TABLE c(k, j, v)",
        "INSERT INTO c VALUES ('a',1,10),('a',2,20),('a',1,11),('b',1,30)",
    ]);
    diff(
        &con,
        &db,
        "SELECT k, j, v FROM c GROUP BY k, j ORDER BY k, j",
    );
    diff(
        &con,
        &db,
        "SELECT k, j, min(v), v FROM c GROUP BY k, j ORDER BY k, j",
    );
}

#[test]
fn bare_column_group_by_alias_expr() {
    // GROUP BY an expression: a bare column NOT in the key is a
    // representative; the key expression maps to the group column.
    let (db, con) = setup_both(&[
        "CREATE TABLE t(id INTEGER PRIMARY KEY, v INTEGER)",
        "INSERT INTO t VALUES (1,3),(2,31),(3,60),(4,90),(5,5)",
    ]);
    diff(
        &con,
        &db,
        "SELECT v / 30 AS bucket, COUNT(*) AS n FROM t GROUP BY v / 30 ORDER BY bucket",
    );
    // Same, but ORDER BY the bare column (representative) with the
    // arithmetic key.
    diff(
        &con,
        &db,
        "SELECT v / 30 AS bucket, v, COUNT(*) FROM t GROUP BY v / 30 ORDER BY bucket",
    );
}

#[test]
fn bare_column_empty_group() {
    // Aggregate over an empty input: one row, NULL representatives.
    let (db, con) = setup_both(&["CREATE TABLE c(k, v)"]);
    diff(&con, &db, "SELECT count(*), v FROM c");
    diff(&con, &db, "SELECT min(v), v FROM c");
}

#[test]
fn bare_column_filter_clause() {
    // FILTERed aggregate + bare: the FILTER shapes the aggregate's rows,
    // not the representative's.
    let (db, con) = setup_both(&[
        "CREATE TABLE c(k, v)",
        "INSERT INTO c VALUES ('a',1),('a',5),('b',10)",
    ]);
    diff(
        &con,
        &db,
        "SELECT k, count(*) FILTER (WHERE v > 2), v FROM c GROUP BY k ORDER BY k",
    );
}

#[test]
fn bare_column_null_first_row() {
    // NULL first-row representative stays NULL (first-seen, not
    // first-non-NULL).
    let (db, con) = setup_both(&[
        "CREATE TABLE c(k, v)",
        "INSERT INTO c VALUES ('a',NULL),('a',5)",
    ]);
    diff(&con, &db, "SELECT k, count(*), v FROM c GROUP BY k");
    // min() skips NULLs — the extreme row is the 5 row, but the rule
    // only applies to the bare representative when min is the ONLY
    // aggregate... count is present too, so first-seen (NULL) wins.
    diff(&con, &db, "SELECT k, count(*), min(v), v FROM c GROUP BY k");
}

#[test]
fn bare_column_collated_group() {
    // Collated GROUP BY + bare representative: the group output value is
    // the first-seen ORIGINAL.
    let (db, con) = setup_both(&[
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
        "INSERT INTO t VALUES (1,'apple'),(2,'APPLE'),(3,'Banana')",
    ]);
    diff(
        &con,
        &db,
        "SELECT v, count(*) FROM t GROUP BY v COLLATE NOCASE ORDER BY v COLLATE NOCASE",
    );
}
