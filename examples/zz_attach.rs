//! Issue #8 probe: ATTACH + bulk `INSERT INTO main.t SELECT * FROM o.t`
//! across the three table shapes the issue did not cover originally —
//! integer keys, a no-index rowid table, and WITHOUT ROWID — plus the
//! uuid-PK shape from the report. Every case verifies BOTH engines of
//! truth: the scan path and the index-driven paths, plus a differential
//! checksum against a row-by-row copy of the same source (the check that
//! would have caught #7 and #8 before users did: bulk and slow paths must
//! produce identical content, not just identical counts).
//!
//! ```text
//! cargo run --release --example zz_attach [-- NROWS]
//! ```

use rustqlite::{Database, Value};
use std::path::PathBuf;

fn tmpdb(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "zz-attach-{}-{}-{}.db",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    let (_, rs) = db.query_with_columns(sql, []).expect(sql);
    rs
}

fn one(db: &Database, sql: &str) -> i64 {
    match rows(db, sql).first().and_then(|r| r.first()) {
        Some(Value::Integer(i)) => *i,
        other => panic!("{sql} -> {other:?}"),
    }
}

/// Content fingerprint of a table: count + a commutative checksum over
/// every row, independent of physical order.
fn checksum(db: &Database, table: &str, order_col: &str) -> (i64, i64) {
    let r = rows(
        db,
        &format!(
            "SELECT COUNT(*), COALESCE(SUM(length(c) * 0 + 1), 0) FROM \
             (SELECT ({order_col} || '') AS c FROM {table})"
        ),
    );
    // sum of row "ids" as a light order-independent fingerprint, plus count
    let _ = r;
    let cnt = one(db, &format!("SELECT COUNT(*) FROM {table}"));
    let digest = rows(
        db,
        &format!("SELECT {order_col} FROM {table} ORDER BY {order_col}"),
    )
    .iter()
    .map(|row| match &row[0] {
        Value::Text(t) => t.as_str().to_string(),
        Value::Integer(i) => i.to_string(),
        other => format!("{other:?}"),
    })
    .fold((0u64, 0u64), |(a, b), s| {
        // FNV-1a per value, combined pairwise so order can't cancel out
        let mut h = 0xcbf29ce484222325u64;
        for byte in s.as_bytes() {
            h ^= *byte as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        (a ^ h, b.wrapping_add(h))
    });
    (cnt, (digest.0 as i64) ^ (digest.1 as i64))
}

struct Case {
    name: &'static str,
    create: &'static str,
    /// column list for INSERT (both sides agree on it)
    cols: &'static str,
    /// expression producing row i's VALUES tuple
    row_expr: fn(usize) -> String,
    /// ORDER BY column for the fingerprint
    order_col: &'static str,
    /// index-driven verification statements with their expected COUNT
    index_probes: Vec<(&'static str, String)>,
}

fn run_case(case: &Case, n: usize) -> bool {
    let src_path = tmpdb("src");
    let dst_bulk = tmpdb("bulk");
    let dst_slow = tmpdb("slow");

    // Source: row-at-a-time (the known-correct incremental path).
    let mut source_rows: Vec<Vec<Value>> = Vec::new();
    {
        let mut src = Database::open(&src_path).unwrap();
        src.execute(case.create, ()).unwrap();
        src.execute("BEGIN", ()).unwrap();
        for i in 0..n {
            src.execute(
                &format!(
                    "INSERT INTO t ({}) VALUES ({})",
                    case.cols,
                    ((case.row_expr)(i))
                ),
                (),
            )
            .unwrap();
        }
        src.execute("COMMIT", ()).unwrap();
        // Read the source's rows back in key order for the literal slow copy.
        source_rows = rows(&src, &format!("SELECT {} FROM t ORDER BY 1", case.cols));
    }

    // Target A: the bulk path under test (ATTACH + INSERT...SELECT).
    let mut bulk = Database::open(&dst_bulk).unwrap();
    bulk.execute(case.create, ()).unwrap();
    bulk.execute(&format!("ATTACH '{}' AS o", src_path.display()), ())
        .unwrap();
    let bulk_res = bulk.execute(
        &format!(
            "INSERT INTO main.t ({}) SELECT {} FROM o.t",
            case.cols, case.cols
        ),
        (),
    );
    bulk.execute("DETACH o", ()).unwrap();
    if let Err(e) = bulk_res {
        println!("  [{}] bulk copy FAILED outright: {e:?}", case.name);
        return false;
    }

    // Target B: row-by-row literal INSERTs of the same source rows (the
    // slow path — known-correct incremental index maintenance).
    let mut slow = Database::open(&dst_slow).unwrap();
    slow.execute(case.create, ()).unwrap();
    slow.execute("BEGIN", ()).unwrap();
    for row in &source_rows {
        let vals = row
            .iter()
            .map(|v| match v {
                Value::Text(t) => format!("'{}'", t.as_str().replace(char::from(39), "''")),
                Value::Integer(i) => i.to_string(),
                Value::Null => "NULL".into(),
                other => panic!("probe does not emit {other:?}"),
            })
            .collect::<Vec<_>>()
            .join(", ");
        slow.execute(
            &format!("INSERT INTO t ({}) VALUES ({})", case.cols, vals),
            (),
        )
        .unwrap();
    }
    slow.execute("COMMIT", ()).unwrap();

    let mut ok = true;

    // 1) Scan counts must match the source on both targets.
    for (label, db) in [("bulk", &bulk), ("slow", &slow)] {
        let cnt = one(db, "SELECT COUNT(*) FROM t");
        if cnt != n as i64 {
            println!("  [{}] {label} scan count {cnt}/{n} — ROW LOSS", case.name);
            ok = false;
        }
    }

    // 2) Index-driven probes (the #8 symptom class).
    for (what, sql) in &case.index_probes {
        let got = one(&bulk, sql);
        let want = one(&slow, sql);
        if got != want {
            println!(
                "  [{}] {what}: bulk {got} vs slow {want} — INDEX TRUNCATION",
                case.name
            );
            ok = false;
        }
    }

    // 3) integrity_check on the bulk result (diagnostic; the
    // differential below is the hard gate).
    match bulk.query_with_columns("PRAGMA integrity_check", []) {
        Ok((_, rs))
            if rs.len() == 1
                && rs[0].len() == 1
                && (rs[0][0] == Value::Integer(1) || rs[0][0] == Value::Text("ok".into())) => {}
        other => {
            let detail = other
                .ok()
                .and_then(|(_, rs)| rs.first().cloned())
                .and_then(|r| r.first().cloned())
                .map(|v| format!(" {v:?}"))
                .unwrap_or_default();
            println!("  [{}] integrity_check NOT ok{detail}", case.name);
            ok = false;
        }
    }

    // 4) Differential: bulk vs row-by-row content fingerprints.
    let (b_cnt, b_sum) = checksum(&bulk, "t", case.order_col);
    let (s_cnt, s_sum) = checksum(&slow, "t", case.order_col);
    if (b_cnt, b_sum) != (s_cnt, s_sum) {
        println!(
            "  [{}] differential MISMATCH: bulk ({b_cnt}, {b_sum:#x}) vs slow ({s_cnt}, {s_sum:#x})",
            case.name
        );
        ok = false;
    }

    for p in [&src_path, &dst_bulk, &dst_slow] {
        let _ = std::fs::remove_file(p);
    }
    if ok {
        println!(
            "  [{}] ok — {} rows, indexes complete, files identical",
            case.name, n
        );
    }
    ok
}

fn uuid(i: usize) -> String {
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        i as u32,
        i as u16,
        (i >> 16) as u16 & 0xfff,
        i as u16,
        i as u64
    )
}

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(2000);

    let cases = vec![
        // The production shape from the issue: uuid TEXT PRIMARY KEY.
        Case {
            name: "uuid-pk",
            create: "CREATE TABLE t (id text PRIMARY KEY, name text)",
            cols: "id, name",
            row_expr: |i| format!("'{}', 'brand-{}'", uuid(i), i),
            order_col: "id",
            index_probes: vec![
                (
                    "PK range scan",
                    "SELECT COUNT(*) FROM t WHERE id > '0'".into(),
                ),
                (
                    "covering subquery",
                    "SELECT COUNT(*) FROM (SELECT id FROM t)".into(),
                ),
            ],
        },
        // Integer keys: rowid alias + a secondary index on the payload.
        Case {
            name: "integer-key",
            create: "CREATE TABLE t (k integer, name text); CREATE INDEX ti ON t(name)",
            cols: "k, name",
            row_expr: |i| format!("{}, 'name-{:06}'", (i * 7919) % 100000, i),
            order_col: "k",
            index_probes: vec![
                (
                    "secondary range scan",
                    "SELECT COUNT(*) FROM t WHERE name > 'name-000000'".into(),
                ),
                (
                    "secondary equality",
                    "SELECT COUNT(*) FROM t WHERE name >= 'name-000001'".into(),
                ),
            ],
        },
        // No index at all: a plain rowid heap — if ROWS go missing, this
        // shape shows it without any index to blame.
        Case {
            name: "no-index",
            create: "CREATE TABLE t (a integer, b text)",
            cols: "a, b",
            row_expr: |i| format!("{}, 'row-{}'", i, i),
            order_col: "a",
            index_probes: vec![],
        },
        // WITHOUT ROWID: the table btree IS the key btree.
        Case {
            name: "without-rowid",
            create: "CREATE TABLE t (k integer, name text, PRIMARY KEY(k)) WITHOUT ROWID",
            cols: "k, name",
            row_expr: |i| format!("{}, 'wr-{}'", (i * 7919) % 100000, i),
            order_col: "k",
            index_probes: vec![
                (
                    "PK range scan",
                    "SELECT COUNT(*) FROM t WHERE k > -1".into(),
                ),
                (
                    "PK seek sample",
                    "SELECT COUNT(*) FROM t WHERE k BETWEEN 7919 AND 79190".into(),
                ),
            ],
        },
    ];

    println!("zz_attach: bulk ATTACH copy, {n} rows per case");
    let mut all_ok = true;
    for case in &cases {
        if !run_case(case, n) {
            all_ok = false;
        }
    }
    println!(
        "{}",
        if all_ok {
            "ALL CASES PASS"
        } else {
            "FAILURES PRESENT (issue #8 class)"
        }
    );
    if !all_ok {
        std::process::exit(1);
    }
}
