//! Window-function regression pins — the window3/window8 round.
//!
//! Every pin below was a REAL divergence against SQLite's own Tcl suite
//! (run differentially by tests/tcl/run.py; the two files went 561
//! failing tests to 0). Each family names the mechanism it pins so a
//! regression says which contract broke:
//!   - per-row extra args (nth_value N, lead/lag offset + default)
//!   - frame bounds that live outside the partition (empty, not clamped)
//!   - GROUPS boundary arithmetic (ends at the boundary group's LAST row)
//!   - EXCLUDE applied to first_value/last_value/nth_value
//!   - RANGE helpers (both directions, fractional offsets, NULL keys)
//!   - NULLS FIRST/LAST honored by the window's own sort and output order
//!
//! Expected values are SQLite's own outputs from the corpus (window3.test,
//! window8.test at the pinned corpus ref).

use rustqlite::{Database, Value};

fn exec_rows(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql, ())
        .expect(sql)
        .iter()
        .map(|r| r.to_vec())
        .collect()
}

fn one_col(db: &mut Database, sql: &str) -> Vec<Value> {
    exec_rows(db, sql)
        .into_iter()
        .map(|r| r[0].clone())
        .collect()
}

fn setup() -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b INT, c TEXT)", ())
        .unwrap();
    db.execute(
        "INSERT INTO t VALUES (1,89,'x'),(2,81,'y'),(3,96,'z'),(4,59,'w'),\
         (5,38,'x'),(6,68,'y'),(7,39,'z'),(8,62,'w')",
        (),
    )
    .unwrap();
    db
}

fn assert_vals(got: Vec<Value>, want: &[Value], what: &str) {
    let fmt = |v: &Value| match v {
        Value::Null => "NULL".to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => r.to_string(),
        Value::Text(s) => s.to_string(),
        Value::Blob(b) => format!("blob({})", b.len()),
    };
    let gs: Vec<String> = got.iter().map(fmt).collect();
    let ws: Vec<String> = want.iter().map(fmt).collect();
    assert_eq!(gs, ws, "{what}: got {gs:?} want {ws:?}");
}

// ---------------------------------------------------------------------------
// 1. nth_value's N is a PER-ROW expression (window3 1.1.10).
// ---------------------------------------------------------------------------
#[test]
fn nth_value_dynamic_n() {
    let mut db = setup();
    // N = (a%3)+1 over a frame of rows 1..k: NULL for k<N, then the
    // frame's N-th value. SQLite's own expected shape.
    let got = one_col(
        &mut db,
        "SELECT nth_value(b, (a%3)+1) OVER \
         (ORDER BY a ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t",
    );
    assert_vals(
        got,
        &[
            Value::Null,        // a=1: N=2, frame 1 row
            Value::Null,        // a=2: N=3, frame 2 rows
            Value::Integer(89), // a=3: N=1 -> 1st of 3
            Value::Integer(81), // a=4: N=2 -> 2nd of 4
            Value::Integer(96), // a=5: N=3 -> 3rd of 5
            Value::Integer(89), // a=6: N=1
            Value::Integer(81), // a=7: N=2
            Value::Integer(96), // a=8: N=3
        ],
        "nth_value(b, (a%3)+1)",
    );
}

#[test]
fn nth_value_bad_n_is_an_error_not_null() {
    let db = setup();
    // windowCheckValue: N must be a positive integer at EVERY row — a
    // zero/NULL/negative N fails the statement with SQLite's message.
    for bad_n in ["0", "NULL", "-1", "'abc'", "2.5"] {
        let sql = format!("SELECT nth_value(b, {bad_n}) OVER (ORDER BY a) FROM t");
        let mut stmt = match db.prepare(&sql) {
            Ok(s) => s,
            Err(_) => continue, // prepare-time rejection is also fine
        };
        let err = stmt.step().expect_err(bad_n);
        assert!(
            err.to_string()
                .contains("second argument to nth_value must be a positive integer"),
            "{bad_n}: {err}"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. lead/lag: per-row offset AND per-row default; negative offsets are
//    legal; non-integral/NULL offsets fall to the default (no error).
// ---------------------------------------------------------------------------
#[test]
fn lead_lag_dynamic_offset_and_default() {
    let mut db = setup();
    let got = one_col(&mut db, "SELECT lead(b, a%3) OVER (ORDER BY a) FROM t");
    assert_vals(
        got,
        &[
            Value::Integer(81), // a=1: offset 1 -> row 2
            Value::Integer(59), // a=2: offset 2 -> row 4
            Value::Integer(96), // a=3: offset 0 -> itself
            Value::Integer(38), // a=4: offset 1 -> row 5
            Value::Integer(39), // a=5: offset 2 -> row 7
            Value::Integer(68), // a=6: offset 0 -> itself
            Value::Integer(62), // a=7: offset 1 -> row 8
            Value::Null,        // a=8: offset 2 past the end
        ],
        "lead(b, a%3)",
    );

    let got = one_col(&mut db, "SELECT lag(b, 1, 'D') OVER (ORDER BY a) FROM t");
    assert_vals(
        got,
        &[
            Value::Text("D".into()),
            Value::Integer(89),
            Value::Integer(81),
            Value::Integer(96),
            Value::Integer(59),
            Value::Integer(38),
            Value::Integer(68),
            Value::Integer(39),
        ],
        "lag(b, 1, 'D')",
    );

    // A NULL offset misses the row seek -> the DEFAULT expression.
    let got = one_col(&mut db, "SELECT lead(b, NULL, c) OVER (ORDER BY a) FROM t");
    assert_vals(
        got,
        &[
            Value::Text("x".into()),
            Value::Text("y".into()),
            Value::Text("z".into()),
            Value::Text("w".into()),
            Value::Text("x".into()),
            Value::Text("y".into()),
            Value::Text("z".into()),
            Value::Text("w".into()),
        ],
        "lead(b, NULL, c)",
    );

    // A negative offset moves the opposite direction (lead backwards).
    let got = one_col(&mut db, "SELECT lead(b, -2) OVER (ORDER BY a) FROM t");
    assert_vals(
        got,
        &[
            Value::Null,
            Value::Null,
            Value::Integer(89),
            Value::Integer(81),
            Value::Integer(96),
            Value::Integer(59),
            Value::Integer(38),
            Value::Integer(68),
        ],
        "lead(b, -2)",
    );
}

// ---------------------------------------------------------------------------
// 3. Frame bounds outside the partition are EMPTY frames, never clamped
//    (window3 1.5.x: `ROWS BETWEEN UNBOUNDED PRECEDING AND 4 PRECEDING`).
// ---------------------------------------------------------------------------
#[test]
fn bounded_end_preceding_is_empty_not_clamped() {
    let mut db = setup();
    let got = one_col(
        &mut db,
        "SELECT max(b) OVER \
         (ORDER BY a ROWS BETWEEN UNBOUNDED PRECEDING AND 4 PRECEDING) FROM t",
    );
    assert_vals(
        got,
        &[
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Integer(89),
            Value::Integer(89),
            Value::Integer(96),
            Value::Integer(96),
        ],
        "max over UP..4 PRECEDING",
    );

    // A start beyond the partition end is empty too (no clip to last row).
    let got = one_col(
        &mut db,
        "SELECT sum(b) OVER \
         (ORDER BY a ROWS BETWEEN 100 FOLLOWING AND UNBOUNDED FOLLOWING) FROM t",
    );
    assert_vals(
        got,
        &vec![Value::Null; 8],
        "sum over 100 FOLLOWING..UF (empty)",
    );

    // Inverted bounds (start after end) are empty — never swapped.
    let got = one_col(
        &mut db,
        "SELECT sum(b) OVER \
         (ORDER BY a ROWS BETWEEN 2 FOLLOWING AND 1 FOLLOWING) FROM t",
    );
    assert_vals(got, &vec![Value::Null; 8], "inverted bounds empty");
}

// ---------------------------------------------------------------------------
// 4. GROUPS boundary arithmetic (window8 1.x): the end lands on the
//    boundary group's LAST row; the bound is constant across a group.
// ---------------------------------------------------------------------------
#[test]
fn groups_frame_boundaries() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE g(k TEXT, v INT)", ()).unwrap();
    db.execute(
        "INSERT INTO g VALUES ('A',1),('A',2),('B',3),('B',4),('B',5),('C',6)",
        (),
    )
    .unwrap();
    let got = one_col(
        &mut db,
        "SELECT sum(v) OVER (ORDER BY k GROUPS BETWEEN UNBOUNDED PRECEDING \
         AND 1 PRECEDING) FROM g",
    );
    assert_vals(
        got,
        &[
            Value::Null,
            Value::Null, // A rows: no previous group
            Value::Integer(3),
            Value::Integer(3),
            Value::Integer(3),  // B rows: through A (1+2)
            Value::Integer(15), // C: through B (3+4+5+1+2 = 15)
        ],
        "GROUPS UP..1 PRECEDING",
    );

    let got = one_col(
        &mut db,
        "SELECT sum(v) OVER (ORDER BY k GROUPS BETWEEN UNBOUNDED PRECEDING \
         AND 1 FOLLOWING) FROM g",
    );
    assert_vals(
        got,
        &[
            Value::Integer(15),
            Value::Integer(15), // A: through B = 1+2+3+4+5
            Value::Integer(21),
            Value::Integer(21),
            Value::Integer(21), // B: through C = all
            Value::Integer(21), // C: through D (clipped) = all
        ],
        "GROUPS UP..1 FOLLOWING",
    );
}

// ---------------------------------------------------------------------------
// 5. EXCLUDE reaches the value functions (window3 1.18/1.20).
// ---------------------------------------------------------------------------
#[test]
fn exclude_applies_to_value_functions() {
    let mut db = setup();
    let got = one_col(
        &mut db,
        "SELECT last_value(b) OVER (ORDER BY a ROWS BETWEEN 4 PRECEDING \
         AND UNBOUNDED FOLLOWING EXCLUDE CURRENT ROW) FROM t",
    );
    // Every row but the last sees the true last row (62); the last row
    // is excluded from its own frame so it sees 39.
    assert_vals(
        got,
        &[
            Value::Integer(62),
            Value::Integer(62),
            Value::Integer(62),
            Value::Integer(62),
            Value::Integer(62),
            Value::Integer(62),
            Value::Integer(62),
            Value::Integer(39),
        ],
        "last_value EXCLUDE CURRENT ROW",
    );

    let got = one_col(
        &mut db,
        "SELECT first_value(b) OVER (ORDER BY a ROWS BETWEEN UNBOUNDED \
         PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE GROUP) FROM t",
    );
    // EXCLUDE GROUP drops the current row's PEERS; with a unique key per
    // row that is the current row itself -> rows 2..8 see 89, row 1 sees 81.
    assert_vals(
        got,
        &[
            Value::Integer(81),
            Value::Integer(89),
            Value::Integer(89),
            Value::Integer(89),
            Value::Integer(89),
            Value::Integer(89),
            Value::Integer(89),
            Value::Integer(89),
        ],
        "first_value EXCLUDE GROUP",
    );
}

// ---------------------------------------------------------------------------
// 6. RANGE: fractional offsets, both directions, NULL keys by placement.
// ---------------------------------------------------------------------------
#[test]
fn range_fractional_and_directions() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE r(a REAL, b INT)", ()).unwrap();
    db.execute(
        "INSERT INTO r VALUES (5,10),(10,20),(13,26),(13,26),(15,30),(20,40),(22,80),(30,90)",
        (),
    )
    .unwrap();
    // window8 3.2's own expected vector (fractional-free, both-preceding).
    let got = one_col(
        &mut db,
        "SELECT sum(b) OVER win FROM r WINDOW win AS \
         (ORDER BY a RANGE BETWEEN 10 PRECEDING AND 5 PRECEDING)",
    );
    assert_vals(
        got,
        &[
            Value::Null,
            Value::Integer(10),
            Value::Integer(10),
            Value::Integer(10),
            Value::Integer(30),
            Value::Integer(102),
            Value::Integer(82),
            Value::Integer(120),
        ],
        "RANGE 10 PRECEDING..5 PRECEDING",
    );

    // window8 3.8: fractional offsets keep their fraction.
    let got = one_col(
        &mut db,
        "SELECT sum(b) OVER win FROM r WINDOW win AS \
         (ORDER BY a RANGE BETWEEN 10.2 PRECEDING AND 5.4 PRECEDING)",
    );
    assert_vals(
        got,
        &[
            Value::Null,
            Value::Null,
            Value::Integer(10),
            Value::Integer(10),
            Value::Integer(10),
            Value::Integer(72),
            Value::Integer(82),
            Value::Integer(120),
        ],
        "RANGE 10.2 PRECEDING..5.4 PRECEDING",
    );

    // A start-FOLLOWING bound that no row reaches is EMPTY.
    let got = one_col(
        &mut db,
        "SELECT sum(b) OVER (ORDER BY a DESC RANGE BETWEEN 5 FOLLOWING \
         AND 10 FOLLOWING) FROM r",
    );
    // DESC: following = smaller keys. a=30: [20,25] -> 40. a=22: [12,17] ->
    // rows 13,13,15 -> 26+26+30=82... computed against SQLite's own file.
    assert_vals(
        got,
        &[
            Value::Integer(120), // a=30: keys {22,20} -> 80+40
            Value::Integer(82),  // a=22: {15,13,13} -> 30+26+26
            Value::Integer(102), // a=20: {15,13,13,10} -> 30+26+26+20
            Value::Integer(30),  // a=15: {10,5} -> 20+10
            Value::Integer(10),  // a=13: {5} -> 10
            Value::Integer(10),  // a=13: {5} -> 10
            Value::Integer(10),  // a=10: {5} -> 10
            Value::Null,         // a=5: {cur-10,cur-5} = [-5,0] -> none
        ],
        "RANGE DESC 5..10 FOLLOWING",
    );
}

#[test]
fn range_null_keys_by_placement() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE n(a INT, b INT)", ()).unwrap();
    db.execute(
        "INSERT INTO n VALUES(1,65),(2,NULL),(3,NULL),(4,NULL),(5,66),(6,67)",
        (),
    )
    .unwrap();
    // window8 7.1.1: a NULL that sorts LAST participates as larger than
    // any real — every numeric row's 6-FOLLOWING frame includes the NULL
    // rows; the NULL rows' own frame is their peer group.
    let got = one_col(
        &mut db,
        "SELECT sum(a) OVER win FROM n WINDOW win AS \
         (ORDER BY b NULLS LAST RANGE BETWEEN 6 FOLLOWING AND UNBOUNDED FOLLOWING)",
    );
    assert_vals(
        got,
        &[
            Value::Integer(9),
            Value::Integer(9),
            Value::Integer(9),
            Value::Integer(9),
            Value::Integer(9),
            Value::Integer(9),
        ],
        "NULLS LAST sentinel in 6 FOLLOWING..UF",
    );

    // window8 7.1.2: the same NULLs are EXCLUDED from a numeric row's
    // preceding window, and the NULL rows see their peer group.
    let got = one_col(
        &mut db,
        "SELECT sum(a) OVER win FROM n WINDOW win AS \
         (ORDER BY b NULLS LAST RANGE BETWEEN 1 PRECEDING AND 2 PRECEDING)",
    );
    assert_vals(
        got,
        &[
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Integer(9),
            Value::Integer(9),
            Value::Integer(9),
        ],
        "NULLS LAST excluded from preceding",
    );
}

// ---------------------------------------------------------------------------
// 7. NULLS FIRST/LAST drives the window's own sort and OUTPUT order
//    (window8 7.x: the rows leave the window in its sort order).
// ---------------------------------------------------------------------------
#[test]
fn window_output_order_honors_nulls_last() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE o(a INT, b INT)", ()).unwrap();
    db.execute(
        "INSERT INTO o VALUES(1,65),(2,NULL),(3,NULL),(4,NULL),(5,66),(6,67)",
        (),
    )
    .unwrap();
    let rows = exec_rows(
        &mut db,
        "SELECT a, sum(a) OVER (ORDER BY b NULLS LAST) FROM o",
    );
    let order: Vec<i64> = rows
        .iter()
        .filter_map(|r| match &r[0] {
            Value::Integer(i) => Some(*i),
            _ => None,
        })
        .collect();
    assert_eq!(order, vec![1, 5, 6, 2, 3, 4], "NULLS LAST output order");
    let sums: Vec<String> = rows
        .iter()
        .map(|r| match &r[1] {
            Value::Integer(i) => i.to_string(),
            Value::Null => "NULL".to_string(),
            _ => "?".to_string(),
        })
        .collect();
    assert_eq!(sums, vec!["1", "6", "12", "21", "21", "21"]);
}

// ---------------------------------------------------------------------------
// The window1 round (the misuse / validation / parser families).
// ---------------------------------------------------------------------------

/// The 22.x family: frame offsets are validated per row — NULL / negative /
/// blob / non-numeric text error with SQLite's exact messages; numeric
/// text and fractional RANGE values pass.
#[test]
fn frame_offset_value_validation() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE a(x, y); INSERT INTO a VALUES(1, 2)", ())
        .unwrap();
    let bad_range = ["NULL", "-0.1", "''", "'2.0x'", "x'1234'"];
    for off in bad_range {
        let sql = format!(
            "SELECT sum(x) OVER (ORDER BY y RANGE BETWEEN {off} PRECEDING \
             AND UNBOUNDED FOLLOWING) FROM a"
        );
        let err = db.query(&sql, ()).expect_err(off);
        assert!(
            err.sqlite_message()
                .contains("frame starting offset must be a non-negative number"),
            "{off}: {}",
            err.sqlite_message()
        );
    }
    // ROWS/GROUPS want integers: 4.5 errors, numeric text passes.
    let err = db
        .query(
            "SELECT sum(x) OVER (ORDER BY y ROWS BETWEEN 4.5 PRECEDING \
             AND UNBOUNDED FOLLOWING) FROM a",
            (),
        )
        .err()
        .unwrap();
    assert!(err
        .sqlite_message()
        .contains("frame starting offset must be a non-negative integer"));
    exec_rows(
        &mut db,
        "SELECT sum(x) OVER (ORDER BY y ROWS BETWEEN '2' PRECEDING \
         AND UNBOUNDED FOLLOWING) FROM a",
    );
    // A per-row COLUMN offset reads the current row's value.
    db.execute("CREATE TABLE b(d); INSERT INTO b VALUES(1.5),(2.5)", ())
        .unwrap();
    let err = db
        .query(
            "SELECT sum(1) OVER (ORDER BY 1 ROWS BETWEEN d FOLLOWING \
             AND UNBOUNDED FOLLOWING) FROM b",
            (),
        )
        .err()
        .unwrap();
    assert!(err
        .sqlite_message()
        .contains("frame starting offset must be a non-negative integer"));
}

/// window1 5.x / 44.2.x: ntile's bucket count is validated at every row.
#[test]
fn ntile_argument_validation() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t(x); INSERT INTO t VALUES(1),(2)", ())
        .unwrap();
    for bad in ["0", "-1", "'zbc'", "2.5", "NULL"] {
        let sql = format!("SELECT ntile({bad}) OVER () FROM t");
        let err = db.query(&sql, ()).expect_err(bad);
        assert!(
            err.sqlite_message()
                .contains("argument of ntile must be a positive integer"),
            "{bad}: {}",
            err.sqlite_message()
        );
    }
}

/// window1 3.x / 7.1.x: the misuse family — window-only builtins without
/// OVER, OVER in WHERE/GROUP BY/HAVING, scalar OVER, FILTER on
/// non-aggregates, window arity.
#[test]
fn window_misuse_validation() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t(x, y); INSERT INTO t VALUES(1, 2)", ())
        .unwrap();
    let cases: &[(&str, &str)] = &[
        (
            "SELECT nth_value(x, 1) FROM t",
            "misuse of window function nth_value()",
        ),
        (
            "SELECT * FROM t WHERE nth_value(x, 1) OVER (ORDER BY y)",
            "misuse of window function nth_value()",
        ),
        (
            "SELECT * FROM t GROUP BY sum(y) OVER ()",
            "misuse of window function sum()",
        ),
        (
            "SELECT * FROM t GROUP BY y HAVING sum(x) OVER ()",
            "misuse of window function sum()",
        ),
        (
            "SELECT trim(x) OVER (ORDER BY y) FROM t",
            "trim() may not be used as a window function",
        ),
        (
            "SELECT row_number(x) OVER () FROM t",
            "wrong number of arguments to function row_number()",
        ),
        (
            "SELECT lag(x) FILTER (WHERE x>0) OVER (ORDER BY x) FROM t",
            "FILTER clause may only be used with aggregate window functions",
        ),
        (
            "SELECT nth_value() OVER () FROM t",
            "wrong number of arguments to function nth_value()",
        ),
        // CREATE INDEX expressions carry no window functions at all.
        (
            "CREATE INDEX ti ON t(x) WHERE sum(y) OVER ()",
            "misuse of window function sum()",
        ),
        (
            "CREATE INDEX tj ON t(sum(y) OVER ())",
            "misuse of window function sum()",
        ),
    ];
    for (sql, want) in cases {
        let err = db.query(sql, ()).err().unwrap_or_else(|| panic!("{sql}"));
        assert!(
            err.sqlite_message().contains(want),
            "{sql}: {} (want {want})",
            err.sqlite_message()
        );
    }
}

/// window1 9.3: `SELECT *` never projects the window machinery's scratch
/// columns — `SELECT *, min(z) OVER (...)` over a 3-column CTE is a
/// 4-column result, not 5.
#[test]
fn star_never_projects_window_scratch_columns() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t4(x, y); INSERT INTO t4 VALUES(1,'g'),(2,'i')",
        (),
    )
    .unwrap();
    let rows = exec_rows(
        &mut db,
        "WITH aaa(x, y, z) AS (SELECT x, y, max(y) OVER (ORDER BY x) FROM t4) \
         SELECT *, min(z) OVER (ORDER BY x) FROM aaa",
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].len(), 4, "4 columns, not 5");
    assert_eq!(rows[0][3], Value::Text("g".into()));
    // And at the same level: star + window over a plain table.
    let rows = exec_rows(&mut db, "SELECT *, min(x) OVER (ORDER BY x) FROM t4");
    assert_eq!(rows[0].len(), 3);
}

/// window1 21.x / 10.x / 79.x: the window-family keywords (and OUTER) are
/// plain identifiers; numeric literals take `_` digit separators.
#[test]
fn window_keywords_as_identifiers_and_digit_separators() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE k(current, exclude, filter, following, groups, no, \
         others, over, partition, preceding, range, ties, unbounded, window)",
        (),
    )
    .unwrap();
    db.execute("INSERT INTO k VALUES(1,2,3,4,5,6,7,8,9,10,11,12,13,14)", ())
        .unwrap();
    let rows = exec_rows(&mut db, "SELECT current, filter, over, window FROM k");
    assert_eq!(rows[0][0], Value::Integer(1));
    assert_eq!(rows[0][2], Value::Integer(8));
    // `outer` as a correlated alias (SQLite's own answer for this
    // exact query: "A,BA" / "A,BB").
    db.execute("CREATE TABLE s(e, r)", ()).unwrap();
    db.execute("INSERT INTO s VALUES('A','N'),('B','S')", ())
        .unwrap();
    let rows = exec_rows(
        &mut db,
        "SELECT (SELECT group_concat(e) OVER () || upper(outer.e) FROM s) FROM s AS outer",
    );
    assert_eq!(rows[0][0], Value::Text("A,BA".into()));
    assert_eq!(rows[1][0], Value::Text("A,BB".into()));
    // Digit separators.
    let v = one_col(&mut db, "SELECT 10_000_000_000 + 1_0");
    assert_eq!(v[0], Value::Integer(10_000_000_010));
    let v = exec_rows(&mut db, "SELECT 1.5_5, 0x1_f, 1e1_0");
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].len(), 3);
    assert_eq!(v[0][1], Value::Integer(31));
    assert_eq!(v[0][2], Value::Real(1e10));
}

/// window1 35.x: a bare `*` with no FROM is "no tables specified" —
/// before any other name resolution.
#[test]
fn bare_star_without_from_is_no_tables_specified() {
    let db = Database::open_in_memory().unwrap();
    let err = db.query("SELECT *", ()).err().unwrap();
    assert!(err.sqlite_message().contains("no tables specified"));
    let err = db.query("SELECT * WHERE nonexist", ()).err().unwrap();
    assert!(err.sqlite_message().contains("no tables specified"));
    let err = db.query("VALUES(1) INTERSECT SELECT *", ()).err().unwrap();
    assert!(err.sqlite_message().contains("no tables specified"));
}

/// The window1 round, part 2: compound arity, trigger variables, the
/// recursive-CTE window rejection, peer output order, and aggregates
/// inside window specifications.
#[test]
fn window1_part2_pins() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t(x, y); INSERT INTO t VALUES(1, 2)", ())
        .unwrap();
    // Compound arms must agree on arity (SQLite's multiSelect check).
    for sql in [
        "SELECT 1, 2 UNION ALL SELECT 3",
        "SELECT 1, 2 UNION SELECT 3",
        "SELECT 1, 2 EXCEPT SELECT 3",
        "SELECT 1, 2 INTERSECT SELECT 3",
        "VALUES(1,2) UNION ALL SELECT 3",
    ] {
        let err = db.query(sql, ()).err().unwrap_or_else(|| panic!("{sql}"));
        assert!(
            err.sqlite_message()
                .contains("do not have the same number of result columns"),
            "{sql}: {}",
            err.sqlite_message()
        );
    }
    // Host parameters never appear in a trigger (window specs included).
    let err = db
        .query(
            "CREATE TRIGGER tr AFTER INSERT ON t BEGIN \
             INSERT INTO t(x) SELECT max(x) OVER w FROM t \
             WINDOW w AS (PARTITION BY EXISTS(SELECT 1 WHERE y=?1)); \
             END",
            (),
        )
        .err()
        .unwrap();
    assert!(err
        .sqlite_message()
        .contains("trigger cannot use variables"));
    // Recursive CTEs reject window functions outright.
    let err = db
        .query(
            "WITH RECURSIVE q(id) AS (\
             SELECT 1 UNION ALL SELECT row_number() OVER () FROM q WHERE id < 2) \
             SELECT * FROM q",
            (),
        )
        .err()
        .unwrap();
    assert!(err
        .sqlite_message()
        .contains("cannot use window functions in recursive queries"));
    // ... but a window in the OUTER query over the recursive CTE is
    // legal, and one inside an aggregate's ARGUMENTS is a misuse.
    let rows = exec_rows(
        &mut db,
        "WITH RECURSIVE q(id) AS (SELECT 1 UNION ALL SELECT id+1 FROM q \
         WHERE id < 2) SELECT row_number() OVER () FROM q",
    );
    assert_eq!(rows.len(), 2);
    let err = db
        .query("SELECT count(row_number() OVER ()) FROM t", ())
        .err()
        .unwrap();
    assert!(err
        .sqlite_message()
        .contains("misuse of window function row_number()"));
    // A bare aggregate inside a window spec makes the query aggregate:
    // one row even over an empty input (the window runs over the
    // aggregate row).
    db.execute("CREATE TABLE e(a)", ()).unwrap();
    let rows = exec_rows(&mut db, "SELECT max(a) OVER (ORDER BY sum(1)) FROM e");
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][0], Value::Null));
    // ... and with data the query still collapses to the single
    // aggregate row; the window ARG is a bare column, whose
    // representative is the first-seen row (SQLite answers 1 here).
    db.execute("CREATE TABLE d(v); INSERT INTO d VALUES(1),(2),(3)", ())
        .unwrap();
    let rows = exec_rows(&mut db, "SELECT max(v) OVER (ORDER BY sum(v)) FROM d");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Integer(1));
    // Window calls on both sides of LIKE (the expr_has_window gap).
    let rows = exec_rows(
        &mut db,
        "SELECT count(v) OVER () LIKE lead(10) OVER () FROM d",
    );
    assert_eq!(rows.len(), 3);
    // Peer order: the FIRST SORTING window's order is the emission
    // order when the outer query has none (window1 52.3 — a no-sort
    // `count() OVER ()` does not pin the stream; the next sorting
    // window's order is what comes out).
    db.execute("CREATE TABLE p(a, b, c)", ()).unwrap();
    db.execute(
        "INSERT INTO p VALUES('AA','bb',356),('CC','aa',158),('BB','aa',399),('FF','bb',938)",
        (),
    )
    .unwrap();
    let got: Vec<String> = exec_rows(
        &mut db,
        "SELECT count() OVER (), sum(c) OVER w, first_value(c) OVER w, \
         count(a) OVER (ORDER BY b) FROM p \
         WINDOW w AS (PARTITION BY 6 ORDER BY a)",
    )
    .into_iter()
    .map(|r| match &r[1] {
        Value::Integer(i) => i.to_string(),
        other => format!("{other:?}"),
    })
    .collect();
    // Running sums in a-order (AA, BB, CC, FF) — rowid order would
    // read 356, 514, 913, 1851; the emission order is the first
    // SORTING window's.
    assert_eq!(got, vec!["356", "755", "913", "1851"]);
}

/// The window1 round, part 3: views with declared column lists over
/// star/join bodies — resolution, positional DML mapping, and RETURNING
/// through INSTEAD OF triggers.
#[test]
fn window1_part3_view_rename_pins() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t1(a INT); INSERT INTO t1 VALUES(1),(2),(4)",
        (),
    )
    .unwrap();
    // A star-over-join body with a declared column list: the view's
    // interface is b/c (the body's own names are ambiguous bare names).
    db.execute(
        "CREATE VIEW t2(b,c) AS SELECT * FROM t1 JOIN t1 A \
         ORDER BY sum(0) OVER (PARTITION BY 0)",
        (),
    )
    .unwrap();
    let rows = exec_rows(&mut db, "SELECT b, c FROM t2 WHERE b=4");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][0], Value::Integer(4));
    // Star over the view: renamed interface, real values.
    let rows = exec_rows(&mut db, "SELECT * FROM t2");
    assert_eq!(rows.len(), 9);
    assert_eq!(rows[0][0], Value::Integer(1));
    // The rename keeps the body columns' DECLARED collations: v(z)
    // over a NOCASE column still dedups a UNION under NOCASE.
    db.execute(
        "CREATE TABLE t2b(id INTEGER PRIMARY KEY, h TEXT COLLATE NOCASE);\
         INSERT INTO t2b VALUES(1,'a'),(2,'X')",
        (),
    )
    .unwrap();
    db.execute("CREATE VIEW v(z) AS SELECT h FROM t2b", ())
        .unwrap();
    let n = exec_rows(
        &mut db,
        "SELECT count(*) FROM (SELECT z FROM v UNION SELECT a FROM t1)",
    );
    assert_eq!(
        n[0][0],
        Value::Integer(5),
        "NOCASE dedup through the rename"
    );
    // UPDATE ... RETURNING through an INSTEAD OF trigger: one projected
    // NEW row per affected view row.
    db.execute(
        "CREATE TRIGGER x1 INSTEAD OF UPDATE ON t2 BEGIN SELECT true; END",
        (),
    )
    .unwrap();
    let (cols, rows) = db
        .query_dml_returning_mut("UPDATE t2 SET c=99 WHERE b=4 RETURNING *", Vec::new())
        .unwrap();
    assert_eq!(cols, vec!["b".to_string(), "c".to_string()]);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][1], Value::Integer(99));
}

/// window1 43.x: a window-function output alias may be referenced
/// DIRECTLY in ORDER BY (`ORDER BY abc`, `ORDER BY abc+5`) but a bare
/// reference from INSIDE a subquery in the ORDER BY is
/// "misuse of aliased window function {name}" — a correlated read
/// cannot reach the window's per-row value.
#[test]
fn window1_part4_aliased_window_pins() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t1(a INTEGER PRIMARY KEY, b INT);\
         INSERT INTO t1(a,b) VALUES(1,10),(2,15),(3,-5)",
        (),
    )
    .unwrap();
    // Direct references are legal and order by the window value.
    let rows = exec_rows(
        &mut db,
        "SELECT a, sum(b) OVER (ORDER BY a) AS abc FROM t1 ORDER BY abc",
    );
    assert_eq!(rows.len(), 3);
    // running sums 10 (a=1), 25 (a=2), 20 (a=3): ascending order.
    assert_eq!(rows[0][0], Value::Integer(1));
    assert_eq!(rows[1][0], Value::Integer(3));
    // Arithmetic on the direct reference too.
    let rows = exec_rows(
        &mut db,
        "SELECT a, sum(b) OVER (ORDER BY a) AS abc FROM t1 ORDER BY abc+5",
    );
    assert_eq!(rows[0][0], Value::Integer(1));
    // ... and ORDER BY <ordinal> 2 (the alias's position) is the same.
    let rows = exec_rows(
        &mut db,
        "SELECT a, sum(b) OVER (ORDER BY a) AS abc FROM t1 ORDER BY 2",
    );
    assert_eq!(rows[0][0], Value::Integer(1));
    // But from inside a subquery it is the misuse — plain alias...
    let err = db
        .query("SELECT count() OVER() AS m FROM t1 ORDER BY (SELECT m)", ())
        .err()
        .unwrap();
    assert!(
        err.sqlite_message()
            .contains("misuse of aliased window function m"),
        "{}",
        err.sqlite_message()
    );
    // ... alias of an expression AROUND the window call ...
    let err = db
        .query(
            "SELECT a, 1+sum(b) OVER (ORDER BY a) AS abc FROM t1 \
             ORDER BY (SELECT abc)",
            (),
        )
        .err()
        .unwrap();
    assert!(
        err.sqlite_message()
            .contains("misuse of aliased window function abc"),
        "{}",
        err.sqlite_message()
    );
    // ... even nested two subqueries deep.
    let err = db
        .query(
            "SELECT a, sum(b) OVER (ORDER BY a) AS abc FROM t1 \
             ORDER BY (SELECT (SELECT abc))",
            (),
        )
        .err()
        .unwrap();
    assert!(
        err.sqlite_message()
            .contains("misuse of aliased window function abc"),
        "{}",
        err.sqlite_message()
    );
}

// ---------------------------------------------------------------------------
// Aggregate promotion out of subqueries (SQLite's resolve.c NameContext
// climb) — the window1 pullup round. An aggregate call inside a subquery
// whose refs bind into the OUTER query belongs to the outer query; the
// subquery reads the computed slot back. Landing-zone errors fire where
// the outer query cannot host the aggregate.
// ---------------------------------------------------------------------------

#[test]
fn agg_promotion_select_list_classic() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t1(a)", ()).unwrap();
    db.execute("INSERT INTO t1 VALUES (2),(3)", ()).unwrap();
    // The promoted query becomes an aggregate query: ONE row.
    assert_vals(
        one_col(&mut db, "SELECT (SELECT sum(a)) FROM t1"),
        &[Value::Integer(5)],
        "classic promotion",
    );
    // Mixed with a direct aggregate: both read their own slots.
    let rows = exec_rows(&mut db, "SELECT max(a), (SELECT sum(a)) FROM t1");
    assert_vals(
        rows[0].clone(),
        &[Value::Integer(3), Value::Integer(5)],
        "promotion beside a direct aggregate",
    );
    // GROUP BY: the promoted call reads per GROUP.
    db.execute("INSERT INTO t1 VALUES (2)", ()).unwrap();
    let rows = exec_rows(
        &mut db,
        "SELECT a%2 AS k, (SELECT sum(a)) FROM t1 GROUP BY a%2 ORDER BY k",
    );
    assert_vals(
        rows[0].clone(),
        &[Value::Integer(0), Value::Integer(4)],
        "per-group promotion (k=0: 2+2)",
    );
    assert_vals(
        rows[1].clone(),
        &[Value::Integer(1), Value::Integer(3)],
        "per-group promotion (k=1: 3)",
    );
    // min/max bare-column rule: a promoted min() drags bare columns to
    // the min-achieving row.
    db.execute("CREATE TABLE t2(a, b)", ()).unwrap();
    db.execute("INSERT INTO t2 VALUES (1,'x'),(5,'y'),(3,'z')", ())
        .unwrap();
    let rows = exec_rows(&mut db, "SELECT (SELECT min(a)), (SELECT b) FROM t2");
    assert_vals(
        rows[0].clone(),
        &[Value::Integer(1), Value::Text("x".into())],
        "promoted min + bare follows min row",
    );
    let rows = exec_rows(&mut db, "SELECT (SELECT max(a)), (SELECT b) FROM t2");
    assert_vals(
        rows[0].clone(),
        &[Value::Integer(5), Value::Text("y".into())],
        "promoted max + bare follows max row",
    );
}

#[test]
fn agg_promotion_misuse_landing_zones() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t1(a)", ()).unwrap();
    db.execute("INSERT INTO t1 VALUES (2),(3)", ()).unwrap();
    // WHERE landing.
    let err = db
        .query("SELECT 1 FROM t1 WHERE (SELECT sum(a)) > 0", ())
        .err()
        .unwrap();
    assert!(
        err.sqlite_message().contains("misuse of aggregate: sum()"),
        "{}",
        err.sqlite_message()
    );
    // Window function ARGUMENT landing (window1 59.1's ntile shape) —
    // the name is reported as written.
    db.execute("CREATE TABLE t(x INTEGER PRIMARY KEY)", ())
        .unwrap();
    db.execute("INSERT INTO t VALUES (123)", ()).unwrap();
    let err = db
        .query("SELECT ntile((SELECT sum(x))) OVER(ORDER BY x) FROM t", ())
        .err()
        .unwrap();
    assert!(
        err.sqlite_message().contains("misuse of aggregate: sum()"),
        "{}",
        err.sqlite_message()
    );
    // FILTER's outer reference promotes (the name AS WRITTEN, uppercase).
    db.execute("CREATE TABLE t0(c0)", ()).unwrap();
    db.execute("CREATE TABLE t1c(c0, c1 UNIQUE)", ()).unwrap();
    db.execute("INSERT INTO t0 VALUES (1)", ()).unwrap();
    db.execute("INSERT INTO t1c VALUES (2,3)", ()).unwrap();
    let err = db
        .query(
            "SELECT COUNT(*) FROM t0, t1c WHERE (SELECT AVG(0) FILTER(WHERE t1c.c1))",
            (),
        )
        .err()
        .unwrap();
    assert!(
        err.sqlite_message().contains("misuse of aggregate: AVG()"),
        "{}",
        err.sqlite_message()
    );
    // ORDER BY of a query that is not otherwise aggregate.
    let err = db
        .query("SELECT a FROM t1 ORDER BY (SELECT sum(a))", ())
        .err()
        .unwrap();
    assert!(
        err.sqlite_message().contains("misuse of aggregate: sum()"),
        "{}",
        err.sqlite_message()
    );
    // ...legal when the query aggregates by its own means.
    assert_vals(
        one_col(&mut db, "SELECT sum(a) FROM t1 ORDER BY (SELECT sum(a))"),
        &[Value::Integer(5)],
        "ORDER BY landing on an aggregate query",
    );
    // GROUP BY landing: SQLite's GROUP-BY-specific message.
    let err = db
        .query("SELECT a FROM t1 GROUP BY (SELECT sum(a))", ())
        .err()
        .unwrap();
    assert!(
        err.sqlite_message()
            .contains("aggregate functions are not allowed in the GROUP BY clause"),
        "{}",
        err.sqlite_message()
    );
    // HAVING on a non-aggregate query — a promotion does not rescue it.
    let err = db
        .query("SELECT 1 FROM t1 HAVING (SELECT sum(a)) > 0", ())
        .err()
        .unwrap();
    assert!(
        err.sqlite_message()
            .contains("HAVING clause on a non-aggregate query"),
        "{}",
        err.sqlite_message()
    );
    let err = db.query("SELECT 1 FROM t1 HAVING 1", ()).err().unwrap();
    assert!(
        err.sqlite_message()
            .contains("HAVING clause on a non-aggregate query"),
        "{}",
        err.sqlite_message()
    );
}

#[test]
fn agg_promotion_window_interplay() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t1(a)", ()).unwrap();
    db.execute("CREATE TABLE t2(y)", ()).unwrap();
    // window1 61.4.2: the promotion makes the outer query aggregate; the
    // subquery's own windows run over the aggregate output row.
    assert_vals(
        one_col(
            &mut db,
            "SELECT (SELECT count(a) OVER ( ORDER BY sum(a) ) + total(a) OVER()) FROM t1",
        ),
        &[Value::Real(0.0)],
        "61.4.2 over empty input",
    );
    db.execute("INSERT INTO t1 VALUES (2),(3)", ()).unwrap();
    assert_vals(
        one_col(
            &mut db,
            "SELECT (SELECT count(a) OVER ( ORDER BY sum(a) ) + total(a) OVER()) FROM t1",
        ),
        &[Value::Real(3.0)],
        "61.4.2 over {2,3}: count(bare=2)=1 + total(2)=2.0",
    );
    // window1 57.3: the promotion passes THROUGH intermediate subqueries
    // to the scope where the refs bind (t3), collapsing the output to the
    // aggregate's one row.
    db.execute("CREATE TABLE t3(y)", ()).unwrap();
    db.execute("INSERT INTO t3 VALUES (5),(11),(-9)", ())
        .unwrap();
    db.execute("CREATE TABLE t1a(a)", ()).unwrap();
    db.execute("INSERT INTO t1a VALUES (22)", ()).unwrap();
    assert_vals(
        one_col(
            &mut db,
            "SELECT (SELECT max(y) OVER( ORDER BY (SELECT x FROM (SELECT sum(y) AS x FROM t1a)))) FROM t3",
        ),
        &[Value::Integer(5)],
        "57.3: one row, max over the bare representative",
    );
    // A subquery's OWN aggregate (its refs bind to its own FROM) never
    // promotes — window8 9.2's shape stays per-row (zero rows over
    // empty input).
    assert_vals(
        one_col(
            &mut db,
            "SELECT (SELECT max(a) OVER ( ORDER BY (SELECT sum(a) FROM t1) )) FROM t1 WHERE 0",
        ),
        &[],
        "local aggregate in a spec subquery does not promote",
    );
    // A MIXED aggregate (some refs local to the atom, some outer) stays
    // inside — a correlated aggregate.
    db.execute("CREATE TABLE t4(b)", ()).unwrap();
    db.execute("INSERT INTO t4 VALUES (10),(20)", ()).unwrap();
    let rows = exec_rows(
        &mut db,
        "SELECT (SELECT sum(b+a) FROM t4) FROM t1 ORDER BY 1 LIMIT 2",
    );
    assert_vals(
        rows[0].clone(),
        &[Value::Integer(34)],
        "mixed aggregate stays inside (2+10+20)",
    );
    assert_vals(
        rows[1].clone(),
        &[Value::Integer(36)],
        "mixed aggregate stays inside (3+10+20)",
    );
}

#[test]
fn agg_promotion_intermediate_scope() {
    // window1 53.0: the promotion lands in the INTERMEDIATE subquery
    // (`FROM a AS d`), not the top — and the join's constant-ON spine
    // must project the right side (the reorder/covering round).
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE a(c UNIQUE)", ()).unwrap();
    db.execute("INSERT INTO a VALUES (4),(0),(9),(-9)", ())
        .unwrap();
    let got = one_col(
        &mut db,
        "SELECT a.c FROM a JOIN a AS b ON a.c=4 JOIN a AS e ON a.c=e.c \
         WHERE a.c=(SELECT (SELECT coalesce(lead(2) OVER(),0) + sum(d.c)) FROM a AS d WHERE a.c)",
    );
    assert_vals(
        got,
        &[
            Value::Integer(4),
            Value::Integer(4),
            Value::Integer(4),
            Value::Integer(4),
        ],
        "53.0: the middle subquery aggregates, the top stays per-row",
    );
}

#[test]
fn reorder_restoration_reads_the_right_sides() {
    // The atom-column model must match the lookup executors' output
    // naming (alias-qualified / table-qualified), and a covering INLJ
    // under a restoration Project must be declined (the restoration
    // reads every atom column).
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE a(c UNIQUE)", ()).unwrap();
    db.execute("INSERT INTO a VALUES (4),(0),(9),(-9)", ())
        .unwrap();
    // The 3-way self-join with a constant ON: a is constrained to c=4
    // by the ON; the projection reads a's side.
    assert_vals(
        one_col(
            &mut db,
            "SELECT a.c FROM a JOIN a AS b ON a.c=4 JOIN a AS e ON a.c=e.c",
        ),
        &[
            Value::Integer(4),
            Value::Integer(4),
            Value::Integer(4),
            Value::Integer(4),
        ],
        "constant-ON 3-way self-join projects the constrained side",
    );
    // Reading the OTHER sides too (the covering-gate fix: e's columns
    // must come from the table, not NULLs).
    let rows = exec_rows(
        &mut db,
        "SELECT a.c, b.c, e.c FROM a JOIN a AS b ON a.c=4 JOIN a AS e ON a.c=e.c ORDER BY b.c",
    );
    assert_vals(
        rows[0].clone(),
        &[Value::Integer(4), Value::Integer(-9), Value::Integer(4)],
        "all three sides present after the reorder",
    );
    // Distinct tables (the aliased-atom naming case).
    db.execute("CREATE TABLE z(c UNIQUE)", ()).unwrap();
    db.execute("INSERT INTO z VALUES (4),(1)", ()).unwrap();
    assert_vals(
        one_col(
            &mut db,
            "SELECT a.c FROM a JOIN a AS b ON a.c=4 JOIN z AS e ON a.c=e.c",
        ),
        &[
            Value::Integer(4),
            Value::Integer(4),
            Value::Integer(4),
            Value::Integer(4),
        ],
        "distinct-table 3-way join",
    );
}

#[test]
fn star_with_order_only_aggregate_reads_its_own_slot() {
    // The star expansion's aggregate-set rebuild must include the ORDER
    // terms: `SELECT sum(b), * FROM t GROUP BY a ORDER BY count(b)` used
    // to project count's slot as the star columns (index drift between
    // the two build sites).
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t(a, b)", ()).unwrap();
    db.execute("INSERT INTO t VALUES (1,2),(1,3),(2,4)", ())
        .unwrap();
    let rows = exec_rows(
        &mut db,
        "SELECT sum(b), * FROM t GROUP BY a ORDER BY count(b)",
    );
    // Groups: a=1 -> sum 5, count 2, rep b=2; a=2 -> sum 4, count 1, rep b=4.
    // ORDER BY count(b): a=2 first (count 1).
    assert_vals(
        rows[0].clone(),
        &[Value::Integer(4), Value::Integer(2), Value::Integer(4)],
        "star beside order-only aggregate (a=2 group)",
    );
    assert_vals(
        rows[1].clone(),
        &[Value::Integer(5), Value::Integer(1), Value::Integer(2)],
        "star beside order-only aggregate (a=1 group)",
    );
}

#[test]
fn group_by_window_misuse_and_row_value_subselect() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t1(a)", ()).unwrap();
    db.execute("CREATE TABLE t2(k)", ()).unwrap();
    db.execute("INSERT INTO t2 VALUES (1)", ()).unwrap();
    // window1 47.2: GROUP BY <ordinal> resolving to a window select item
    // is "misuse of window function f()" — at PREPARE time, so a WHERE
    // OR short-circuit cannot skip the subquery that carries it.
    let err = db
        .query(
            "SELECT 234 FROM t2 WHERE k=1 OR (SELECT k FROM t2 WHERE (SELECT sum(a) OVER() FROM t1 GROUP BY 1))",
            (),
        )
        .err()
        .unwrap();
    assert!(
        err.sqlite_message()
            .contains("misuse of window function sum()"),
        "{}",
        err.sqlite_message()
    );
    let err = db
        .query("SELECT 1, sum(a) OVER() FROM t1 GROUP BY 2", ())
        .err()
        .unwrap();
    assert!(
        err.sqlite_message()
            .contains("misuse of window function sum()"),
        "{}",
        err.sqlite_message()
    );
    // A literal ordinal (pointing at a non-window item) stays legal.
    db.query("SELECT 1, sum(a) OVER() FROM t1 GROUP BY 1", ())
        .unwrap();
    // GROUP BY <alias> of a window item: the aliased misuse message,
    // unless a real source column of the same name wins.
    let err = db
        .query("SELECT sum(a) OVER() AS s FROM t1 GROUP BY s", ())
        .err()
        .unwrap();
    assert!(
        err.sqlite_message()
            .contains("misuse of aliased window function s"),
        "{}",
        err.sqlite_message()
    );
    // window1 51.1: a scalar sub-select whose single result item is a
    // ROW VALUE is "row value misused" at prepare time (zero-row inputs
    // never evaluate anything; the HAVING over zero groups would
    // otherwise skip it entirely).
    let err = db
        .query(
            "SELECT c FROM t1 GROUP BY a HAVING(SELECT(sum(a), sum(a)))",
            (),
        )
        .err()
        .unwrap();
    assert!(
        err.sqlite_message().contains("row value misused"),
        "{}",
        err.sqlite_message()
    );
    // The multi-COLUMN sub-select form stays legal in row comparisons.
    db.execute("INSERT INTO t1 VALUES (7)", ()).unwrap();
    assert_vals(
        one_col(&mut db, "SELECT (7,7) = (SELECT a, a FROM t1)"),
        &[Value::Integer(1)],
        "two-column sub-select vs row comparison",
    );
}

#[test]
fn recursive_cte_window_error_precedes_resolution() {
    // window1 15.0: windows in a recursive CTE's arms error at prepare
    // time, BEFORE the arms' name resolution (the q.id-style resolution
    // noise never surfaces). The setup arm stays exempt.
    let mut db = Database::open_in_memory().unwrap();
    let err = db
        .query(
            "WITH t(id, parent) AS (SELECT 1, NULL UNION ALL SELECT 2, NULL), \
             q AS (SELECT t.*, ROW_NUMBER() OVER (ORDER BY t.id) AS rn FROM t WHERE parent IS NULL \
             UNION ALL \
             SELECT t.*, ROW_NUMBER() OVER (ORDER BY t.id) AS rn FROM q JOIN t ON t.parent = q.id) \
             SELECT * FROM q",
            (),
        )
        .err()
        .unwrap();
    assert!(
        err.sqlite_message()
            .contains("cannot use window functions in recursive queries"),
        "{}",
        err.sqlite_message()
    );
    // The setup arm alone is exempt (runs once, before recursion).
    db.query(
        "WITH c(a) AS (SELECT row_number() OVER () UNION ALL SELECT a+1 FROM c WHERE a < 3) \
         SELECT * FROM c",
        (),
    )
    .unwrap();
}
