//! WITHOUT ROWID table-scan order (native container): SQLite's WITHOUT
//! ROWID table b-tree is keyed by the PRIMARY KEY, so an un-ORDER BYed
//! scan (and the tie order of every sort built above one) comes out in
//! PK order — NOT insert order. The engine's native container stores
//! rows in an internal rowid b-tree; the scan drivers therefore walk the
//! engine-internal PK index (`IndexOrigin::WithoutRowidPk`) instead.
//! These tests pin every driver + the PK-clause collation/order
//! semantics against the real SQLite oracle (rusqlite bundled).

use rustqlite::{Database, Value};

fn texts(rows: &[Vec<Value>], col: usize) -> Vec<String> {
    rows.iter().map(|r| r[col].as_text().to_string()).collect()
}

/// The headline shape: bare scan, filtered scan, LIMIT'd scan, projected
/// scan — all in PK order, on native (first open + reopen) AND
/// SQLite-format containers.
#[test]
fn worow_scan_order_all_drivers() {
    let dir = std::env::temp_dir().join(format!("worow_ord_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let keys = ["m", "z", "a", "q", "b", "y", "c", "k"];

    let mk = |db: &mut Database, sql: &str| {
        db.execute(sql, []).unwrap();
        for k in keys {
            db.execute("INSERT INTO t VALUES (?1, 1)", [Value::Text(k.into())])
                .unwrap();
        }
    };

    // Native container, first open.
    let path = dir.join("native.rdb");
    let mut db = Database::open(&path).unwrap();
    mk(
        &mut db,
        "CREATE TABLE t(a TEXT PRIMARY KEY, b INT) WITHOUT ROWID",
    );
    let expect: Vec<String> = {
        let mut v = keys.to_vec();
        v.sort();
        v.into_iter().map(String::from).collect()
    };
    assert_eq!(texts(&db.query("SELECT a FROM t", []).unwrap(), 0), expect);
    // Filtered (compiled predicate path).
    assert_eq!(
        texts(&db.query("SELECT a FROM t WHERE a > 'c'", []).unwrap(), 0),
        vec!["k", "m", "q", "y", "z"],
    );
    // LIMIT pushdown.
    assert_eq!(
        texts(&db.query("SELECT a FROM t LIMIT 3", []).unwrap(), 0),
        vec!["a", "b", "c"],
    );
    // Filter + LIMIT.
    assert_eq!(
        texts(
            &db.query("SELECT a FROM t WHERE a > 'c' LIMIT 2", [])
                .unwrap(),
            0
        ),
        vec!["k", "m"],
    );
    // Projection fused into the scan.
    assert_eq!(
        texts(&db.query("SELECT a FROM t WHERE b = 1", []).unwrap(), 0),
        expect,
    );
    // Aggregate with GROUP_CONCAT follows scan order (SQLite practice).
    assert_eq!(
        db.query("SELECT group_concat(a) FROM t", []).unwrap()[0][0].as_text(),
        "a,b,c,k,m,q,y,z",
    );
    // JOIN: the worow table is the probe (left) side.
    db.execute("CREATE TABLE o(k TEXT, v INT)", []).unwrap();
    for (k, v) in [("m", 1), ("a", 2), ("z", 3), ("q", 4)] {
        db.execute(
            "INSERT INTO o VALUES (?1, ?2)",
            [Value::Text(k.into()), Value::Integer(v)],
        )
        .unwrap();
    }
    let j = db
        .query("SELECT a, v FROM t JOIN o ON t.a = o.k", [])
        .unwrap();
    let got: Vec<(String, i64)> = j
        .iter()
        .map(|r| (r[0].as_text().to_string(), r[1].as_integer()))
        .collect();
    assert_eq!(
        got,
        vec![
            ("a".into(), 2),
            ("m".into(), 1),
            ("q".into(), 4),
            ("z".into(), 3),
        ]
    );
    drop(db);

    // Native container, REOPEN (the PK index rebuild path).
    let db = Database::open(&path).unwrap();
    assert_eq!(texts(&db.query("SELECT a FROM t", []).unwrap(), 0), expect);
    drop(db);

    // SQLite-format round trip (rows insert in arbitrary order): the
    // engine exports real SQLite bytes, reloads them through the
    // interop reader, and the scan must still read PK order.
    let mut sdb = Database::open_in_memory().unwrap();
    mk(
        &mut sdb,
        "CREATE TABLE t(a TEXT PRIMARY KEY, b INT) WITHOUT ROWID",
    );
    sdb.export_sqlite_format(dir.join("sfmt.db")).unwrap();
    let sdb = Database::open(dir.join("sfmt.db")).unwrap();
    assert_eq!(texts(&sdb.query("SELECT a FROM t", []).unwrap(), 0), expect);
}

/// Composite PK with a DESC column and a PK-clause COLLATE: SQLite's
/// table order is the PK's DECLARED direction (a DESC, b COLLATE NOCASE
/// ASC). Pinned against the real SQLite oracle.
#[test]
fn worow_composite_desc_collated_pk() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE c(a INT, b TEXT, v INT, PRIMARY KEY(a DESC, b COLLATE NOCASE)) WITHOUT ROWID",
        [],
    )
    .unwrap();
    let rows_in = [
        (2, "x"),
        (1, "Y"),
        (3, "a"),
        (1, "b"),
        (2, "A"),
        (3, "Z"),
        (1, "a"),
    ];
    for (a, b) in rows_in {
        db.execute(
            "INSERT INTO c VALUES (?1, ?2, 0)",
            [Value::Integer(a), Value::Text(b.into())],
        )
        .unwrap();
    }
    let got: Vec<(i64, String)> = db
        .query("SELECT a, b FROM c", [])
        .unwrap()
        .iter()
        .map(|r| (r[0].as_integer(), r[1].as_text().to_string()))
        .collect();

    // Oracle: real SQLite on the same data.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute(
        "CREATE TABLE c(a INT, b TEXT, v INT, PRIMARY KEY(a DESC, b COLLATE NOCASE)) WITHOUT ROWID",
        [],
    )
    .unwrap();
    for (a, b) in rows_in {
        conn.execute("INSERT INTO c VALUES (?1, ?2, 0)", rusqlite::params![a, b])
            .unwrap();
    }
    let mut stmt = conn.prepare("SELECT a, b FROM c").unwrap();
    let expect: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(
        got, expect,
        "composite DESC+NOCASE PK order must match SQLite"
    );
    // Self-consistency: the scan order equals its own PK ORDER BY.
    let ordered: Vec<(i64, String)> = db
        .query("SELECT a, b FROM c ORDER BY a DESC, b COLLATE NOCASE", [])
        .unwrap()
        .iter()
        .map(|r| (r[0].as_integer(), r[1].as_text().to_string()))
        .collect();
    assert_eq!(got, ordered);
}

/// The PK-clause COLLATE also governs UNIQUENESS (NOCASE PK: 'Y' after
/// 'y' is a duplicate) — pinned against SQLite's exact rejection.
#[test]
fn worow_pk_clause_collation_uniqueness() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE u(a TEXT PRIMARY KEY COLLATE NOCASE) WITHOUT ROWID",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO u VALUES ('y')", []).unwrap();
    let dup = db.execute("INSERT INTO u VALUES ('Y')", []);
    assert!(dup.is_err(), "NOCASE PK must reject 'Y' after 'y'");

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute(
        "CREATE TABLE u(a TEXT PRIMARY KEY COLLATE NOCASE) WITHOUT ROWID",
        [],
    )
    .unwrap();
    conn.execute("INSERT INTO u VALUES ('y')", []).unwrap();
    let oracle_dup = conn.execute("INSERT INTO u VALUES ('Y')", []);
    assert!(oracle_dup.is_err(), "oracle sanity: SQLite rejects it too");
}

/// Mixed-type PK ordering: SQLite's cross-type order (INTEGER < TEXT <
/// BLOB) applies to the PK b-tree too.
#[test]
fn worow_mixed_type_pk_order() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE m(k, v INT, PRIMARY KEY(k)) WITHOUT ROWID", [])
        .unwrap();
    db.execute("INSERT INTO m VALUES ('txt', 1)", []).unwrap();
    db.execute("INSERT INTO m VALUES (7, 1)", []).unwrap();
    db.execute("INSERT INTO m VALUES (X'0A0B', 1)", []).unwrap();
    db.execute("INSERT INTO m VALUES (2, 1)", []).unwrap();
    db.execute("INSERT INTO m VALUES ('aaa', 1)", []).unwrap();
    let rows = db.query("SELECT k FROM m", []).unwrap();
    let kind = |v: &Value| -> u8 {
        match v {
            Value::Integer(_) => 0,
            Value::Text(_) => 1,
            Value::Blob(_) => 2,
            _ => 3,
        }
    };
    let kinds: Vec<u8> = rows.iter().map(|r| kind(&r[0])).collect();
    assert_eq!(
        kinds,
        vec![0, 0, 1, 1, 2],
        "INTEGER < TEXT < BLOB in PK order"
    );

    // Oracle: same ordering from real SQLite.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute("CREATE TABLE m(k, v INT, PRIMARY KEY(k)) WITHOUT ROWID", [])
        .unwrap();
    for ins in [
        "INSERT INTO m VALUES ('txt', 1)",
        "INSERT INTO m VALUES (7, 1)",
        "INSERT INTO m VALUES (X'0A0B', 1)",
        "INSERT INTO m VALUES (2, 1)",
        "INSERT INTO m VALUES ('aaa', 1)",
    ] {
        conn.execute(ins, []).unwrap();
    }
    let mut stmt = conn.prepare("SELECT typeof(k) FROM m").unwrap();
    let expect: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(expect, vec!["integer", "integer", "text", "text", "blob"]);
}

/// DML RETURNING follows the scan order (PK order), for UPDATE and
/// DELETE — pinned against SQLite.
#[test]
fn worow_returning_order() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE d(a TEXT PRIMARY KEY, v INT) WITHOUT ROWID",
        [],
    )
    .unwrap();
    for (k, v) in [("m", 1), ("z", 2), ("a", 3), ("q", 4), ("b", 5)] {
        db.execute(
            "INSERT INTO d VALUES (?1, ?2)",
            [Value::Text(k.into()), Value::Integer(v)],
        )
        .unwrap();
    }
    let upd = db
        .query("UPDATE d SET v = v + 10 WHERE v > 1 RETURNING a", [])
        .unwrap();
    // v > 1 matches z(2), a(3), q(4), b(5) — PK order.
    assert_eq!(texts(&upd, 0), vec!["a", "b", "q", "z"]);
    let del = db.query("DELETE FROM d RETURNING a", []).unwrap();
    assert_eq!(texts(&del, 0), vec!["a", "b", "m", "q", "z"]);

    // Oracle.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute(
        "CREATE TABLE d(a TEXT PRIMARY KEY, v INT) WITHOUT ROWID",
        [],
    )
    .unwrap();
    for (k, v) in [("m", 1), ("z", 2), ("a", 3), ("q", 4), ("b", 5)] {
        conn.execute("INSERT INTO d VALUES (?1, ?2)", rusqlite::params![k, v])
            .unwrap();
    }
    let upd: Vec<String> = conn
        .prepare("UPDATE d SET v = v + 10 WHERE v > 1 RETURNING a")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(upd, vec!["a", "b", "q", "z"]);
    let del: Vec<String> = conn
        .prepare("DELETE FROM d RETURNING a")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(del, vec!["a", "b", "m", "q", "z"]);
}

/// ORDER BY ties over a WITHOUT ROWID table resolve in PK order (the
/// scan order feeds the stable sort) — same as SQLite's stable sorter
/// over its PK b-tree.
#[test]
fn worow_order_by_ties_follow_pk_order() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE s(a TEXT PRIMARY KEY, grp INT) WITHOUT ROWID",
        [],
    )
    .unwrap();
    for (k, g) in [("m", 2), ("z", 1), ("a", 2), ("q", 1), ("b", 2), ("y", 1)] {
        db.execute(
            "INSERT INTO s VALUES (?1, ?2)",
            [Value::Text(k.into()), Value::Integer(g)],
        )
        .unwrap();
    }
    let got = texts(&db.query("SELECT a FROM s ORDER BY grp", []).unwrap(), 0);
    assert_eq!(got, vec!["q", "y", "z", "a", "b", "m"]);

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute(
        "CREATE TABLE s(a TEXT PRIMARY KEY, grp INT) WITHOUT ROWID",
        [],
    )
    .unwrap();
    for (k, g) in [("m", 2), ("z", 1), ("a", 2), ("q", 1), ("b", 2), ("y", 1)] {
        conn.execute("INSERT INTO s VALUES (?1, ?2)", rusqlite::params![k, g])
            .unwrap();
    }
    let expect: Vec<String> = conn
        .prepare("SELECT a FROM s ORDER BY grp")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(got, expect);
}

/// Randomized differential sweep: shuffled inserts over single/composite
/// PKs (ASC/DESC mixes, collated members), bare scan vs real SQLite.
#[test]
fn worow_randomized_differential() {
    let mut seed = 0x1234_5678_9abc_def0u64;
    let mut next = || {
        // xorshift64*
        seed ^= seed >> 12;
        seed ^= seed << 25;
        seed ^= seed >> 27;
        seed.wrapping_mul(0x2545F4914F6CDD1D)
    };
    let shapes: [&str; 5] = [
        "CREATE TABLE t(a TEXT PRIMARY KEY, b INT) WITHOUT ROWID",
        "CREATE TABLE t(a INT, b TEXT, PRIMARY KEY(a, b)) WITHOUT ROWID",
        "CREATE TABLE t(a INT, b TEXT, PRIMARY KEY(a DESC, b)) WITHOUT ROWID",
        "CREATE TABLE t(a INT, b TEXT, PRIMARY KEY(a, b DESC)) WITHOUT ROWID",
        "CREATE TABLE t(a INT, b TEXT COLLATE NOCASE, PRIMARY KEY(a, b)) WITHOUT ROWID",
    ];
    for (si, shape) in shapes.iter().enumerate() {
        for round in 0..6 {
            let mut db = Database::open_in_memory().unwrap();
            db.execute(shape, []).unwrap();
            let conn = rusqlite::Connection::open_in_memory().unwrap();
            conn.execute(shape, []).unwrap();
            // 40 shuffled distinct rows.
            let n = 40u64;
            let mut perm: Vec<u64> = (0..n).collect();
            for i in (1..perm.len()).rev() {
                let j = (next() % (i as u64 + 1)) as usize;
                perm.swap(i, j);
            }
            for &p in &perm {
                let a = format!("k{:03}", p);
                let b = ((p * 37) % 11) as i64;
                let (sql, params) = if si % 2 == 0 {
                    (
                        "INSERT INTO t VALUES (?1, ?2)",
                        (
                            vec![Value::Text(a.clone().into()), Value::Integer(b)],
                            rusqlite::params![a, b],
                        ),
                    )
                } else {
                    (
                        "INSERT INTO t VALUES (?1, ?2)",
                        (
                            vec![Value::Integer(b), Value::Text(a.clone().into())],
                            rusqlite::params![b, a],
                        ),
                    )
                };
                db.execute(sql, params.0.clone()).unwrap();
                conn.execute(sql, params.1).unwrap();
            }
            let order_col = if si % 2 == 0 { "a" } else { "b" };
            let got = db.query(&format!("SELECT {order_col} FROM t"), []).unwrap();
            let got: Vec<String> = got
                .iter()
                .map(|r| match &r[0] {
                    Value::Text(t) => t.to_string(),
                    v => format!("{v:?}"),
                })
                .collect();
            let mut stmt = conn.prepare(&format!("SELECT {order_col} FROM t")).unwrap();
            let expect: Vec<String> = stmt
                .query_map([], |r| {
                    Ok(match r.get_ref(0)? {
                        rusqlite::types::ValueRef::Text(t) => {
                            String::from_utf8_lossy(t).to_string()
                        }
                        v => format!("{v:?}"),
                    })
                })
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            assert_eq!(
                got, expect,
                "shape {si} round {round}: scan order diverged from SQLite"
            );
        }
    }
}
