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
        .map(|r| r.iter().cloned().collect())
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
    let mut db = setup();
    // windowCheckValue: N must be a positive integer at EVERY row — a
    // zero/NULL/negative N fails the statement with SQLite's message.
    for bad_n in ["0", "NULL", "-1", "'abc'", "2.5"] {
        let sql = format!("SELECT nth_value(b, {bad_n}) OVER (ORDER BY a) FROM t");
        let mut stmt = match db.prepare(&sql) {
            Ok(s) => s,
            Err(_) => continue, // prepare-time rejection is also fine
        };
        let err = stmt.step().err().expect(bad_n);
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
