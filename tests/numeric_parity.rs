//! Bit-exact numeric parity with real SQLite for SUM / TOTAL / AVG.
//!
//! SQLite accumulates aggregates with two disciplines rustqlite now
//! mirrors exactly:
//! - INTEGER inputs sum exactly in i64; ONE conversion to double at
//!   finalize (avg over 2^53-scale integers keeps the exact bits).
//! - REAL inputs go through Kahan–Babuška–Neumaier compensated summation
//!   (`kahanBabuskaNeumaierStep`), folded once at finalize
//!   (`rSum + rErr`).
//!
//! This suite compares f64 BITS (not tolerance) against the bundled
//! SQLite (rusqlite `bundled` feature), across the plain, filtered,
//! GROUP BY, window-frame, and worker-split (parallel) shapes. The
//! avg-of-2^53 case and the 0.1/0.2/0.3 cases below FAIL against a
//! naive-rounding or naive-f64-fold implementation — they are the
//! regression locks for the discipline.

use rusqlite::Connection;
use rustqlite::{Database, Value};

/// Run one SQL program on both engines and compare the final SELECT's
/// REAL outputs bit-for-bit.
fn assert_bit_parity(name: &str, setup: &str, select: &str) {
    let sq = Connection::open_in_memory().unwrap();
    sq.execute_batch(setup).unwrap();
    let expected: Vec<Option<f64>> = {
        let mut stmt = sq.prepare(select).unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, Option<f64>>(0)).unwrap();
        rows.map(|r| r.unwrap()).collect()
    };

    let mut rq = Database::open_in_memory().unwrap();
    rq.execute(setup, []).unwrap();
    let got = rq.query(select, []).unwrap();
    assert_eq!(
        got.len(),
        expected.len(),
        "{name}: row count {} vs sqlite {}",
        got.len(),
        expected.len()
    );
    for (i, (g, e)) in got.iter().zip(expected.iter()).enumerate() {
        match (g.first().cloned().unwrap_or(Value::Null), e) {
            (Value::Null, None) => {}
            (Value::Real(x), Some(y)) => assert_eq!(
                x.to_bits(),
                y.to_bits(),
                "{name}: row {i} bits {:x} vs sqlite {:x} ({x} vs {y})",
                x.to_bits(),
                y.to_bits()
            ),
            (Value::Integer(x), Some(y)) => assert_eq!(
                (x as f64).to_bits(),
                y.to_bits(),
                "{name}: row {i} int {x} vs sqlite {y}"
            ),
            (g, e) => panic!("{name}: row {i} type mismatch {g:?} vs {e:?}"),
        }
    }
}

#[test]
fn avg_integers_exact_i64_path() {
    // avg(2^53+1, 1): iSum = 2^53+2 (exact), one conversion, /2.
    // A naive f64 fold rounds 2^53+1 to 2^53 first and loses a bit.
    assert_bit_parity(
        "avg huge ints",
        "CREATE TABLE t (v); INSERT INTO t VALUES (9007199254740993), (1);",
        "SELECT avg(v) FROM t",
    );
    assert_bit_parity(
        "sum huge ints",
        "CREATE TABLE t (v); INSERT INTO t VALUES (9007199254740993), (1);",
        "SELECT sum(v), total(v) FROM t",
    );
}

#[test]
fn avg_sum_reals_kbn_compensated() {
    // 0.1 + 0.2 + 0.3: naive fold gives 0.6000000000000001, KBN gives
    // exactly 0.6 — SQLite's compensated discipline.
    assert_bit_parity(
        "avg 0.1 0.2 0.3",
        "CREATE TABLE r (v); INSERT INTO r VALUES (0.1),(0.2),(0.3);",
        "SELECT avg(v) FROM r",
    );
    assert_bit_parity(
        "sum 0.1..0.8",
        "CREATE TABLE r (v); INSERT INTO r VALUES (0.1),(0.2),(0.3),(0.4),(0.5),(0.6),(0.7),(0.8);",
        "SELECT sum(v), total(v), avg(v) FROM r",
    );
}

#[test]
fn int_to_real_flip_matches_sqlite() {
    // Integers then a REAL: the integer partial folds in once at the
    // flip, compensation starts from zero.
    assert_bit_parity(
        "flip 1 2 3 0.5 4",
        "CREATE TABLE m (v); INSERT INTO m VALUES (1),(2),(3),(0.5),(4);",
        "SELECT sum(v), total(v), avg(v) FROM m",
    );
    assert_bit_parity(
        "flip reals first",
        "CREATE TABLE m (v); INSERT INTO m VALUES (0.5),(1),(2),(3),(4);",
        "SELECT sum(v), total(v), avg(v) FROM m",
    );
}

#[test]
fn avg_groupby_and_filter_parity() {
    assert_bit_parity(
        "groupby avg",
        "CREATE TABLE g (k, v);
         INSERT INTO g VALUES (1,0.1),(2,0.2),(1,0.3),(2,0.4),(1,0.2),(2,0.1);",
        "SELECT k, sum(v), avg(v), total(v) FROM g GROUP BY k ORDER BY k",
    );
    assert_bit_parity(
        "filtered avg",
        "CREATE TABLE g (k, v);
         INSERT INTO g VALUES (1,0.1),(2,0.2),(1,0.3),(2,0.4),(1,0.2),(2,0.1);",
        "SELECT sum(v), avg(v) FROM g WHERE k = 1",
    );
}

#[test]
fn avg_window_frames_parity() {
    assert_bit_parity(
        "window sum/avg floats",
        "CREATE TABLE w (k, v);
         INSERT INTO w VALUES (1,0.1),(2,0.2),(3,0.3),(4,0.4),(5,0.5);",
        "SELECT sum(v) OVER (ORDER BY k ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM w ORDER BY k",
    );
    assert_bit_parity(
        "window avg floats",
        "CREATE TABLE w (k, v);
         INSERT INTO w VALUES (1,0.1),(2,0.2),(3,0.3),(4,0.4),(5,0.5);",
        "SELECT avg(v) OVER (ORDER BY k ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM w ORDER BY k",
    );
    assert_bit_parity(
        "window sum int flip",
        "CREATE TABLE w (k, v);
         INSERT INTO w VALUES (1,1),(2,2),(3,0.5),(4,4),(5,5);",
        "SELECT sum(v) OVER (ORDER BY k ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM w ORDER BY k",
    );
}

#[test]
fn avg_null_semantics() {
    // AVG over all NULLs is NULL; NULLs are skipped otherwise.
    assert_bit_parity(
        "all null",
        "CREATE TABLE n (v); INSERT INTO n VALUES (NULL),(NULL);",
        "SELECT avg(v), sum(v) FROM n",
    );
    assert_bit_parity(
        "nulls skipped",
        "CREATE TABLE n (v); INSERT INTO n VALUES (1),(NULL),(2),(NULL),(4);",
        "SELECT avg(v), sum(v), count(v) FROM n",
    );
}

#[test]
fn avg_parallel_split_bit_exact_integers() {
    // Above the parallel-scan threshold: INTEGER accumulation is exact,
    // so the worker-split merge must be BIT-identical to SQLite's serial
    // scan. 300k rows, v = id % 100000 (INTEGER).
    let n = 300_000i64;
    let build = |db: &mut Database| {
        db.execute("CREATE TABLE big (id INTEGER PRIMARY KEY, v INTEGER)", [])
            .unwrap();
        db.execute("BEGIN", []).unwrap();
        let mut i = 0i64;
        while i < n {
            let hi = (i + 1000).min(n);
            let mut sql = String::from("INSERT INTO big (id, v) VALUES ");
            for j in i..hi {
                if j > i {
                    sql.push(',');
                }
                sql.push_str(&format!("({}, {})", j, j % 100000));
            }
            db.execute(&sql, []).unwrap();
            i = hi;
        }
        db.execute("COMMIT", []).unwrap();
    };

    let mut sq = Connection::open_in_memory().unwrap();
    {
        use rusqlite::params;
        sq.execute("CREATE TABLE big (id INTEGER PRIMARY KEY, v INTEGER)", [])
            .unwrap();
        let tx = sq.transaction().unwrap();
        {
            let mut stmt = tx.prepare("INSERT INTO big (id, v) VALUES (?, ?)").unwrap();
            for j in 0..n {
                stmt.execute(params![j, j % 100000]).unwrap();
            }
        }
        tx.commit().unwrap();
    }
    let (sq_total, sq_avg, sq_t1): (f64, f64, f64) = sq
        .query_row(
            "SELECT total(v), avg(v), total(v * 1.0) FROM big",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();

    let mut rq = Database::open_in_memory().unwrap();
    build(&mut rq);
    let par = rq
        .query("SELECT total(v), avg(v), total(v * 1.0) FROM big", [])
        .unwrap();
    let mut ser = Database::open_in_memory().unwrap();
    build(&mut ser);
    ser.execute("PRAGMA parallel_scan=0", []).unwrap();
    let ser_rows = ser
        .query("SELECT total(v), avg(v), total(v * 1.0) FROM big", [])
        .unwrap();

    for (label, got, want) in [
        ("parallel total", &par[0][0], sq_total),
        ("parallel avg", &par[0][1], sq_avg),
        ("parallel total(v*1.0)", &par[0][2], sq_t1),
    ] {
        match got {
            Value::Real(x) => assert_eq!(
                x.to_bits(),
                want.to_bits(),
                "{label}: {:x} vs sqlite {:x} ({x} vs {want})",
                x.to_bits(),
                want.to_bits()
            ),
            Value::Integer(x) => assert_eq!(
                (*x as f64).to_bits(),
                want.to_bits(),
                "{label}: int {x} vs sqlite {want}"
            ),
            other => panic!("{label}: unexpected type {other:?}"),
        }
    }
    assert_eq!(par, ser_rows, "parallel must equal serial exactly");
}

#[test]
fn avg_no_rounding_lock() {
    // The old behavior rounded to 10 decimals — 1/3-style values must
    // now carry the FULL double expansion, matching SQLite exactly.
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (v);
         INSERT INTO t VALUES (1),(2),(4),(8),(16),(32),(64),(128),(256);",
        [],
    )
    .unwrap();
    let rows = db.query("SELECT avg(v) FROM t", []).unwrap();
    let x = match &rows[0][0] {
        Value::Real(x) => *x,
        other => panic!("avg must be REAL, got {other:?}"),
    };
    // 511 / 9 = 56.77777777777778... the raw double.
    assert_eq!(x.to_bits(), (511.0f64 / 9.0).to_bits());
    assert!(
        format!("{x}").len() > 12,
        "must not be rounded to 10 dp: {x}"
    );
}
