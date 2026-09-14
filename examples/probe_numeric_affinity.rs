//! Probe: what does REAL SQLite do with NUMERIC affinity and CAST?
//!
//! Pins the exact coercion semantics the engine's new `Affinity::Numeric`
//! must reproduce:
//! * INSERT of text/real/integer/blob values into a NUMERIC column
//! * CAST(x AS NUMERIC / DECIMAL / DECIMAL(10,2)) for every storage class
//! * whether DECIMAL(10,2) rounds (it does NOT — SQLite ignores the
//!   precision/scale, which is the documented footgun our PG-style
//!   rounding deliberately fixes for declared columns)
use rusqlite::Connection;

fn main() {
    let conn = Connection::open_in_memory().unwrap();

    println!("=== column affinity coercion (NUMERIC column) ===");
    conn.execute_batch(
        "CREATE TABLE t(a NUMERIC, b DECIMAL(10,2));
         INSERT INTO t VALUES
           ('123',    '123'),
           ('1.5',    '1.5'),
           ('abc',    'abc'),
           ('123abc', '123abc'),
           (5.0,      5.0),
           (5.5,      5.5),
           (7,        7),
           (x'00',    x'00');",
    )
    .unwrap();
    let mut stmt = conn
        .prepare("SELECT rowid, typeof(a), CAST(a AS TEXT), typeof(b), CAST(b AS TEXT) FROM t ORDER BY rowid")
        .unwrap();
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
        ))
    })
    .unwrap();
    for row in rows {
        let (id, ta, va, tb, vb) = row.unwrap();
        println!("row {id}: typeof(a)={ta} a={va} | typeof(b)={tb} b={vb}");
    }

    println!("\n=== CAST into NUMERIC-family type names ===");
    let cases = [
        "CAST('123' AS NUMERIC)",
        "CAST('1.5' AS NUMERIC)",
        "CAST('abc' AS NUMERIC)",
        "CAST('123abc' AS NUMERIC)",
        "CAST(12.5 AS NUMERIC)",
        "CAST(12.0 AS NUMERIC)",
        "CAST(5 AS NUMERIC)",
        "CAST(x'00' AS NUMERIC)",
        "CAST('abc' AS DECIMAL)",
        "CAST('1.555' AS DECIMAL(10,2))",
        "CAST(NULL AS NUMERIC)",
    ];
    for c in cases {
        let v: String = conn
            .query_row(&format!("SELECT typeof({c}) || ' -> ' || {c}"), [], |r| r.get(0))
            .unwrap();
        println!("{c:32} => {v}");
    }

    println!("\n=== DECIMAL(10,2) rounding on INSERT? ===");
    let v: String = conn
        .query_row(
            "SELECT typeof(b) || ' ' || b FROM t WHERE rowid = 2",
            [],
            |r| r.get(0),
        )
        .unwrap();
    println!("inserted 1.5 into DECIMAL(10,2): {v} (SQLite: no rounding)");
    conn.execute("INSERT INTO t(b) VALUES ('1.555')", []).unwrap();
    let v2: String = conn
        .query_row("SELECT typeof(b) || ' ' || b FROM t WHERE rowid = 9", [], |r| {
            r.get(0)
        })
        .unwrap();
    println!("inserted 1.555 into DECIMAL(10,2): {v2} (SQLite: no rounding — the footgun)");

    println!("\n=== typeof/affinity of declared names ===");
    for ty in ["NUMERIC", "DECIMAL", "DEC", "FIXED", "MONEY", "BOOLEAN", "DATE"] {
        let v: String = conn
            .query_row(
                &format!("SELECT typeof(CAST(1 AS {ty})) || ', ' || typeof(CAST('x' AS {ty}))"),
                [],
                |r| r.get(0),
            )
            .unwrap();
        println!("{ty:10} => {v}");
    }
}
