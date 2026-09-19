//! Empirical boundary matrix: rowid seek semantics vs real SQLite.
use rustqlite::Database;

fn q_ours(db: &mut Database, sql: &str) -> String {
    match db.query(sql, []) {
        Ok(rows) => rows
            .into_iter()
            .map(|r| {
                r.iter()
                    .map(|v| format!("{v:?}"))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .collect::<Vec<_>>()
            .join(" ; "),
        Err(e) => format!("ERR: {e}"),
    }
}

fn q_sq(conn: &rusqlite::Connection, sql: &str) -> String {
    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(e) => return format!("ERR: {e}"),
    };
    let ncols = stmt.column_count();
    let mut out_rows: Vec<String> = Vec::new();
    if ncols == 0 {
        return match stmt.execute([]) {
            Ok(_) => "OK".into(),
            Err(e) => format!("ERR: {e}"),
        };
    }
    match stmt.query([]) {
        Ok(mut rows) => loop {
            match rows.next() {
                Ok(Some(r)) => {
                    let mut vals = Vec::new();
                    for i in 0..ncols {
                        match r.get_ref(i).unwrap() {
                            rusqlite::types::ValueRef::Null => vals.push("NULL".into()),
                            rusqlite::types::ValueRef::Integer(x) => vals.push(format!("i({x})")),
                            rusqlite::types::ValueRef::Real(x) => vals.push(format!("r({x})")),
                            rusqlite::types::ValueRef::Text(t) => {
                                vals.push(format!("t('{}')", String::from_utf8_lossy(t)))
                            }
                            rusqlite::types::ValueRef::Blob(b) => {
                                vals.push(format!("b({})", b.len()))
                            }
                        }
                    }
                    out_rows.push(vals.join(","));
                }
                Ok(None) => break,
                Err(e) => return format!("ERR: {e}"),
            }
        },
        Err(e) => return format!("ERR: {e}"),
    }
    out_rows.join(" ; ")
}

fn e_ours(db: &mut Database, sql: &str) -> String {
    match db.execute(sql, []) {
        Ok(_) => "OK".into(),
        Err(e) => format!("ERR: {e}"),
    }
}

fn e_sq(conn: &rusqlite::Connection, sql: &str) -> String {
    match conn.execute(sql, []) {
        Ok(_) => "OK".into(),
        Err(e) => format!("ERR: {e}"),
    }
}

fn main() {
    // ── Part 1: INSERT rowid with boundary REAL values ──
    println!("=== INSERT rowid boundary values ===");
    let inserts = [
        (
            "real -2^63",
            "INSERT INTO t1 (rowid) VALUES (-9.2233720368547758e18)",
        ),
        (
            "real 2^63",
            "INSERT INTO t1 (rowid) VALUES (9.2233720368547758e18)",
        ),
        (
            "real 2^63-1024",
            "INSERT INTO t1 (rowid) VALUES (9223372036854774784.0)",
        ),
        (
            "real -(2^63-1024)",
            "INSERT INTO t1 (rowid) VALUES (-9223372036854774784.0)",
        ),
        (
            "real 2^63+2048",
            "INSERT INTO t1 (rowid) VALUES (9223372036854777856.0)",
        ),
        ("real 1e19", "INSERT INTO t1 (rowid) VALUES (1e19)"),
        (
            "text i64::MIN",
            "INSERT INTO t1 (rowid) VALUES ('-9223372036854775808')",
        ),
        (
            "text i64::MAX",
            "INSERT INTO t1 (rowid) VALUES ('9223372036854775807')",
        ),
        (
            "real-text -2^63",
            "INSERT INTO t1 (rowid) VALUES ('-9.2233720368547758e18')",
        ),
    ];
    for (name, sql) in inserts {
        let mut db = Database::open_in_memory().unwrap();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        db.execute("CREATE TABLE t1 (v TEXT)", []).unwrap();
        conn.execute("CREATE TABLE t1 (v TEXT)", []).unwrap();
        let ours = e_ours(&mut db, sql);
        let theirs = e_sq(&conn, sql);
        let mark = if ours == theirs { "SAME" } else { "DIFF <<<" };
        println!("[{mark}] INSERT {name}:\n  ours:   {ours}\n  sqlite: {theirs}");
    }

    // ── Part 2: equality seek against pre-seeded boundary rowids ──
    println!("\n=== equality seek vs boundary rowids ===");
    let seed = [
        "CREATE TABLE t2 (id INTEGER PRIMARY KEY, v TEXT)",
        "INSERT INTO t2 (id) VALUES (-9223372036854775808)",
        "INSERT INTO t2 (id) VALUES (9223372036854775807)",
        "INSERT INTO t2 (id) VALUES (9223372036854774784)",
        "INSERT INTO t2 (id) VALUES (-9223372036854774784)",
        "INSERT INTO t2 (id) VALUES (5)",
    ];
    let mut db = Database::open_in_memory().unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for s in seed {
        db.execute(s, []).unwrap();
        conn.execute(s, []).unwrap();
    }
    let seeks = [
        ("eq real -2^63", "SELECT count(*) FROM t2 WHERE id = -9.2233720368547758e18"),
        ("eq real 2^63", "SELECT count(*) FROM t2 WHERE id = 9.2233720368547758e18"),
        ("eq real 2^63-1024", "SELECT count(*) FROM t2 WHERE id = 9223372036854774784.0"),
        ("eq real -(2^63-1024)", "SELECT count(*) FROM t2 WHERE id = -9223372036854774784.0"),
        ("eq real 5.0", "SELECT count(*) FROM t2 WHERE id = 5.0"),
        ("eq real 5.5", "SELECT count(*) FROM t2 WHERE id = 5.5"),
        ("eq text '-92233...808'", "SELECT count(*) FROM t2 WHERE id = '-9223372036854775808'"),
        ("eq text '92233...807'", "SELECT count(*) FROM t2 WHERE id = '9223372036854775807'"),
        ("eq text '-9.22e18'", "SELECT count(*) FROM t2 WHERE id = '-9.2233720368547758e18'"),
        ("eq text '9.22e18'", "SELECT count(*) FROM t2 WHERE id = '9.2233720368547758e18'"),
        ("eq text '9223372036854774784.0'", "SELECT count(*) FROM t2 WHERE id = '9223372036854774784.0'"),
        ("eq real 2^63+2048", "SELECT count(*) FROM t2 WHERE id = 9223372036854777856.0"),
        ("IN boundary", "SELECT count(*) FROM t2 WHERE id IN (-9.2233720368547758e18, 9.2233720368547758e18, 5.0, 5.5, '5')"),
        ("IN text boundary", "SELECT count(*) FROM t2 WHERE id IN ('-9223372036854775808', '9223372036854775807')"),
        ("join real -2^63", "SELECT count(*) FROM k k JOIN t2 ON k.f = t2.id"),
    ];
    for (name, sql) in seeks {
        let ours = q_ours(&mut db, sql);
        let theirs = q_sq(&conn, sql);
        let mark = if ours == theirs { "SAME" } else { "DIFF <<<" };
        println!("[{mark}] {name}:\n  ours:   {ours}\n  sqlite: {theirs}");
    }

    // ── Part 3: range seeks at boundaries ──
    println!("\n=== range seek vs boundary rowids ===");
    let ranges = [
        ("> real -2^63", "SELECT count(*) FROM t2 WHERE id > -9.2233720368547758e18"),
        (">= real -2^63", "SELECT count(*) FROM t2 WHERE id >= -9.2233720368547758e18"),
        ("< real 2^63", "SELECT count(*) FROM t2 WHERE id < 9.2233720368547758e18"),
        ("<= real 2^63", "SELECT count(*) FROM t2 WHERE id <= 9.2233720368547758e18"),
        ("> real 2^63", "SELECT count(*) FROM t2 WHERE id > 9.2233720368547758e18"),
        (">= real 2^63", "SELECT count(*) FROM t2 WHERE id >= 9.2233720368547758e18"),
        ("< real -2^63", "SELECT count(*) FROM t2 WHERE id < -9.2233720368547758e18"),
        ("<= real -2^63", "SELECT count(*) FROM t2 WHERE id <= -9.2233720368547758e18"),
        ("> real 2^63-1024", "SELECT count(*) FROM t2 WHERE id > 9223372036854774784.0"),
        (">= real 1e19", "SELECT count(*) FROM t2 WHERE id >= 1e19"),
        ("> real 1e19", "SELECT count(*) FROM t2 WHERE id > 1e19"),
        ("< real -1e19", "SELECT count(*) FROM t2 WHERE id < -1e19"),
        ("BETWEEN -2^63 and 2^63", "SELECT count(*) FROM t2 WHERE id BETWEEN -9.2233720368547758e18 AND 9.2233720368547758e18"),
    ];
    for (name, sql) in ranges {
        let ours = q_ours(&mut db, sql);
        let theirs = q_sq(&conn, sql);
        let mark = if ours == theirs { "SAME" } else { "DIFF <<<" };
        println!("[{mark}] {name}:\n  ours:   {ours}\n  sqlite: {theirs}");
    }

    // ── Part 4: the JOIN shape from the fuzz failure, with key table ──
    println!("\n=== join probe ===");
    for s in [
        "CREATE TABLE k (f REAL)",
        "INSERT INTO k VALUES (-9.2233720368547758e18)",
        "INSERT INTO k VALUES (9.2233720368547758e18)",
        "INSERT INTO k VALUES (5.0)",
    ] {
        db.execute(s, []).unwrap();
        conn.execute(s, []).unwrap();
    }
    let joins = [
        (
            "join all",
            "SELECT k.f, t2.id FROM k JOIN t2 ON k.f = t2.id",
        ),
        (
            "join rev",
            "SELECT k.f, t2.id FROM t2 JOIN k ON k.f = t2.id",
        ),
        (
            "update join-shape",
            "UPDATE t2 SET v = 'x' WHERE id IN (SELECT f FROM k)",
        ),
    ];
    for (name, sql) in joins {
        let ours = q_ours(&mut db, sql);
        let theirs = q_sq(&conn, sql);
        let mark = if ours == theirs { "SAME" } else { "DIFF <<<" };
        println!("[{mark}] {name}:\n  ours:   {ours}\n  sqlite: {theirs}");
    }
    // Scalar control
    let ctrl = "SELECT -9223372036854775808 = -9.2233720368547758e18, 9223372036854775807 = 9.2233720368547758e18";
    println!(
        "[ctrl] scalar:\n  ours:   {}\n  sqlite: {}",
        q_ours(&mut db, ctrl),
        q_sq(&conn, ctrl)
    );
}
