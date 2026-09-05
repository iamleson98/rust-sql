//! SQLite feature-parity audit: probe every feature of SQLite's SQL
//! surface and report support. Run:
//! `cargo run --example parity_audit --features sqlx 2>/dev/null | sort`
//!
//! Output lines: `PASS <feature>` / `FAIL <feature> : <error>`

use rustqlite::{Database, Value};

fn probe(db: &mut Database, name: &str, sql: &str, want_rows: Option<usize>) -> bool {
    let res = db.execute(sql, []);
    if let Err(e) = res {
        println!("FAIL {name} : {e}");
        return false;
    }
    if let Some(want) = want_rows {
        match db.query(sql, []) {
            Ok(rows) if rows.len() == want => {}
            Ok(rows) => {
                println!(
                    "FAIL {name} : query returned {} rows, expected {want}",
                    rows.len()
                );
                return false;
            }
            Err(e) => {
                println!("FAIL {name} : re-query: {e}");
                return false;
            }
        }
    }
    true
}

fn probe_err(db: &mut Database, name: &str, sql: &str, want_substr: &str) -> bool {
    match db.execute(sql, []) {
        Ok(()) => {
            println!("FAIL {name} : expected error containing {want_substr:?}, got Ok");
            false
        }
        Err(e) => {
            let msg = format!("{e}");
            if msg.to_lowercase().contains(&want_substr.to_lowercase()) {
                true
            } else {
                println!("FAIL {name} : error {msg:?} does not contain {want_substr:?}");
                false
            }
        }
    }
}

fn scalar(db: &mut Database, sql: &str) -> Option<Value> {
    db.query(sql, [])
        .ok()?
        .into_iter()
        .next()?
        .into_iter()
        .next()
}

fn expect(db: &mut Database, name: &str, sql: &str, want: &Value) -> bool {
    match scalar(db, sql) {
        Some(v) if &v == want => true,
        other => {
            println!("FAIL {name} : {sql} => {other:?}, want {want:?}");
            false
        }
    }
}

fn main() {
    let mut ok = 0usize;
    let mut fail = 0usize;
    macro_rules! p {
        ($db:expr, $name:expr, $sql:expr) => {{
            let good = probe(&mut $db, $name, $sql, None);
            if good {
                ok += 1;
            } else {
                fail += 1;
            }
        }};
        ($db:expr, $name:expr, $sql:expr, $rows:expr) => {{
            let good = probe(&mut $db, $name, $sql, Some($rows));
            if good {
                ok += 1;
            } else {
                fail += 1;
            }
        }};
    }
    macro_rules! e {
        ($db:expr, $name:expr, $sql:expr, $err:expr) => {{
            let good = probe_err(&mut $db, $name, $sql, $err);
            if good {
                ok += 1;
            } else {
                fail += 1;
            }
        }};
    }
    macro_rules! w {
        ($db:expr, $name:expr, $sql:expr, $want:expr) => {{
            let good = expect(&mut $db, $name, $sql, $want);
            if good {
                ok += 1;
            } else {
                fail += 1;
            }
        }};
    }

    // ============ DDL: tables ============
    let mut db = Database::open_in_memory().unwrap();
    p!(
        db,
        "ddl.create",
        "CREATE TABLE t1 (a INTEGER PRIMARY KEY, b TEXT, c REAL)"
    );
    p!(
        db,
        "ddl.pk.composite",
        "CREATE TABLE t2 (a INTEGER, b TEXT, PRIMARY KEY (a, b))"
    );
    p!(db, "ddl.unique.col", "CREATE TABLE t3 (a INTEGER UNIQUE)");
    p!(
        db,
        "ddl.unique.table",
        "CREATE TABLE t4 (a INTEGER, b INTEGER, UNIQUE (a, b))"
    );
    p!(db, "ddl.check", "CREATE TABLE t5 (a INTEGER CHECK (a > 0))");
    p!(
        db,
        "ddl.check.table",
        "CREATE TABLE t6 (a INTEGER, CHECK (a > 0 AND a < 100))"
    );
    p!(db, "ddl.notnull", "CREATE TABLE t7 (a TEXT NOT NULL)");
    p!(
        db,
        "ddl.default.literal",
        "CREATE TABLE t8 (a INTEGER DEFAULT 42)"
    );
    p!(
        db,
        "ddl.default.expr",
        "CREATE TABLE t9 (a TEXT DEFAULT ('x' || 'y'))"
    );
    p!(
        db,
        "ddl.default.current",
        "CREATE TABLE t10 (a TEXT DEFAULT CURRENT_TIMESTAMP)"
    );
    p!(
        db,
        "ddl.collate",
        "CREATE TABLE t11 (a TEXT COLLATE NOCASE)"
    );
    p!(
        db,
        "ddl.without_rowid",
        "CREATE TABLE t12 (a TEXT PRIMARY KEY, b INTEGER) WITHOUT ROWID"
    );
    p!(
        db,
        "ddl.strict",
        "CREATE TABLE t13 (a INTEGER, b TEXT) STRICT"
    );
    p!(
        db,
        "ddl.generated.virt",
        "CREATE TABLE t14 (a INTEGER, b AS (a * 2))"
    );
    p!(
        db,
        "ddl.generated.stored",
        "CREATE TABLE t15 (a INTEGER, b AS (a * 2) STORED)"
    );
    p!(
        db,
        "ddl.if_not_exists",
        "CREATE TABLE IF NOT EXISTS t1 (a INTEGER PRIMARY KEY)"
    );
    p!(db, "ddl.temp", "CREATE TEMP TABLE tmp1 (a INTEGER)");
    p!(db, "ddl.as_select", "CREATE TABLE t16 AS SELECT 1 AS x");
    p!(db, "ddl.column.any", "CREATE TABLE t17 (a ANY)");

    // STRICT enforcement
    p!(
        db,
        "ddl.strict.badtype.insert",
        "CREATE TABLE ts (a INTEGER) STRICT; INSERT INTO ts VALUES (1)"
    );
    e!(
        db,
        "ddl.strict.reject",
        "INSERT INTO ts VALUES ('text')",
        "type"
    );

    // ============ DDL: index / view / trigger ============
    p!(db, "ddl.index", "CREATE INDEX i1 ON t1 (b)");
    p!(db, "ddl.index.unique", "CREATE UNIQUE INDEX i2 ON t1 (b)");
    p!(db, "ddl.index.desc", "CREATE INDEX i3 ON t1 (b DESC)");
    p!(db, "ddl.index.expr", "CREATE INDEX i4 ON t1 (a + b)");
    p!(
        db,
        "ddl.index.partial",
        "CREATE INDEX i5 ON t1 (a) WHERE a > 10"
    );
    p!(
        db,
        "ddl.index.collate",
        "CREATE INDEX i6 ON t1 (b COLLATE NOCASE)"
    );
    p!(db, "ddl.view", "CREATE VIEW v1 AS SELECT a, b FROM t1");
    p!(db, "ddl.view.query", "SELECT * FROM v1", 0);
    p!(
        db,
        "ddl.trigger.insert",
        "CREATE TRIGGER tr1 AFTER INSERT ON t1 BEGIN INSERT INTO t3 (a) VALUES (new.a); END"
    );
    p!(
        db,
        "ddl.trigger.delete",
        "CREATE TRIGGER tr2 AFTER DELETE ON t1 BEGIN INSERT INTO t3 (a) VALUES (old.a); END"
    );
    p!(
        db,
        "ddl.trigger.update",
        "CREATE TRIGGER tr3 AFTER UPDATE OF b ON t1 BEGIN INSERT INTO t3 (a) VALUES (new.a); END"
    );
    p!(db, "ddl.trigger.insteof", "CREATE TRIGGER tr4 INSTEAD OF INSERT ON v1 BEGIN INSERT INTO t1 (a, b) VALUES (new.a, new.b); END");

    // ============ DDL: ALTER ============
    p!(db, "alter.rename.table", "ALTER TABLE t3 RENAME TO t3b");
    p!(
        db,
        "alter.rename.col",
        "ALTER TABLE t1 RENAME COLUMN b TO bb"
    );
    p!(
        db,
        "alter.add.col",
        "ALTER TABLE t1 ADD COLUMN d INTEGER DEFAULT 0"
    );
    p!(db, "alter.drop.col", "ALTER TABLE t1 DROP COLUMN c");

    // ============ DML ============
    // Fresh database: the ALTER probes above renamed/dropped columns.
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t1 (a INTEGER PRIMARY KEY, b TEXT, c REAL)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE t2 (a INTEGER, b TEXT, PRIMARY KEY (a, b))",
        [],
    )
    .unwrap();
    db.execute("CREATE TABLE t3 (a INTEGER UNIQUE)", [])
        .unwrap();
    db.execute("CREATE TABLE t8 (a INTEGER DEFAULT 42)", [])
        .unwrap();
    p!(
        db,
        "dml.insert.values",
        "INSERT INTO t1 (a, b, c) VALUES (1, 'x', 1.5)"
    );
    p!(
        db,
        "dml.insert.multi",
        "INSERT INTO t1 (a, b, c) VALUES (2, 'y', 2.5), (3, 'z', 3.5)"
    );
    p!(
        db,
        "dml.insert.default_values",
        "INSERT INTO t8 DEFAULT VALUES"
    );
    p!(
        db,
        "dml.insert.select",
        "INSERT INTO t1 (a, b, c) SELECT a + 100, b, c FROM t1"
    );
    p!(
        db,
        "dml.insert.or_ignore",
        "INSERT OR IGNORE INTO t3 (a) VALUES (1)"
    );
    p!(
        db,
        "dml.insert.or_replace",
        "INSERT OR REPLACE INTO t3 (a) VALUES (1)"
    );
    p!(
        db,
        "dml.insert.or_abort",
        "INSERT OR ABORT INTO t8 (a) VALUES (7)"
    );
    p!(
        db,
        "dml.upsert.nothing",
        "INSERT INTO t3 (a) VALUES (1) ON CONFLICT (a) DO NOTHING"
    );
    p!(
        db,
        "dml.upsert.update",
        "INSERT INTO t3 (a) VALUES (1) ON CONFLICT (a) DO UPDATE SET a = excluded.a + 1"
    );
    p!(
        db,
        "dml.upsert.where",
        "INSERT INTO t3 (a) VALUES (99) ON CONFLICT (a) DO UPDATE SET a = 99 WHERE t3.a < 50"
    );
    p!(
        db,
        "dml.insert.returning",
        "INSERT INTO t1 (a, b, c) VALUES (999, 'r', 9.9) RETURNING a, b"
    );
    p!(db, "dml.update", "UPDATE t1 SET b = 'upd' WHERE a = 1");
    p!(db, "dml.update.expr", "UPDATE t1 SET c = c * 2 WHERE a = 1");
    p!(
        db,
        "dml.update.multiple",
        "UPDATE t1 SET b = 'x', c = 0 WHERE a = 2"
    );
    p!(
        db,
        "dml.update.from",
        "UPDATE t1 SET c = t2x.b FROM t2 AS t2x WHERE t1.a = t2x.a"
    );
    p!(
        db,
        "dml.update.returning",
        "UPDATE t1 SET c = 1 WHERE a = 1 RETURNING a"
    );
    p!(db, "dml.delete", "DELETE FROM t1 WHERE a = 999");
    p!(
        db,
        "dml.delete.returning",
        "DELETE FROM t1 WHERE a = 998 RETURNING a"
    );
    p!(db, "dml.delete.limit", "DELETE FROM t1 WHERE a > 0 LIMIT 2");
    p!(
        db,
        "dml.update.limit",
        "UPDATE t1 SET c = 0 WHERE a > 0 LIMIT 1"
    );

    // ============ SELECT forms ============
    p!(db, "sel.distinct", "SELECT DISTINCT a FROM t1");
    p!(db, "sel.distinct.all", "SELECT ALL a FROM t1");
    p!(
        db,
        "sel.join.inner",
        "SELECT * FROM t1 JOIN t2 ON t1.a = t2.a"
    );
    p!(
        db,
        "sel.join.left",
        "SELECT * FROM t1 LEFT JOIN t2 ON t1.a = t2.a"
    );
    p!(db, "sel.join.cross", "SELECT * FROM t1 CROSS JOIN t2");
    p!(
        db,
        "sel.join.inner.explicit",
        "SELECT * FROM t1 INNER JOIN t2 ON t1.a = t2.a"
    );
    p!(
        db,
        "sel.join.left_outer",
        "SELECT * FROM t1 LEFT OUTER JOIN t2 ON t1.a = t2.a"
    );
    p!(
        db,
        "sel.join.right",
        "SELECT * FROM t1 RIGHT JOIN t2 ON t1.a = t2.a"
    );
    p!(
        db,
        "sel.join.full",
        "SELECT * FROM t1 FULL OUTER JOIN t2 ON t1.a = t2.a"
    );
    p!(db, "sel.join.natural", "SELECT * FROM t1 NATURAL JOIN t2");
    p!(db, "sel.join.usign", "SELECT * FROM t1 JOIN t2 USING (a)");
    p!(
        db,
        "sel.join.comma",
        "SELECT * FROM t1, t2 WHERE t1.a = t2.a"
    );
    p!(
        db,
        "sel.join.self_alias",
        "SELECT * FROM t1 AS x JOIN t1 AS y ON x.a = y.a"
    );
    p!(db, "sel.union", "SELECT a FROM t1 UNION SELECT a FROM t2");
    p!(
        db,
        "sel.union.all",
        "SELECT a FROM t1 UNION ALL SELECT a FROM t2"
    );
    p!(
        db,
        "sel.intersect",
        "SELECT a FROM t1 INTERSECT SELECT a FROM t2"
    );
    p!(db, "sel.except", "SELECT a FROM t1 EXCEPT SELECT a FROM t2");
    p!(
        db,
        "sel.order.nulls",
        "SELECT a FROM t1 ORDER BY a NULLS LAST"
    );
    p!(db, "sel.order.expr", "SELECT a FROM t1 ORDER BY a + 1");
    p!(db, "sel.order.alias", "SELECT a AS x FROM t1 ORDER BY x");
    p!(db, "sel.values", "VALUES (1, 2), (3, 4)");
    p!(db, "sel.cte", "WITH x AS (SELECT 1 AS n) SELECT * FROM x");
    p!(db, "sel.cte.recursive", "WITH RECURSIVE r (n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 5) SELECT n FROM r");
    p!(
        db,
        "sel.cte.multiple",
        "WITH x AS (SELECT 1 AS n), y AS (SELECT 2 AS m) SELECT * FROM x, y"
    );
    p!(
        db,
        "sel.cte.materialized",
        "WITH x AS MATERIALIZED (SELECT 1 AS n) SELECT * FROM x"
    );
    p!(
        db,
        "sel.cte.notmaterialized",
        "WITH x AS NOT MATERIALIZED (SELECT 1 AS n) SELECT * FROM x"
    );
    p!(db, "sel.subquery.scalar", "SELECT (SELECT max(a) FROM t1)");
    p!(
        db,
        "sel.subquery.in",
        "SELECT a FROM t1 WHERE a IN (SELECT a FROM t2)"
    );
    p!(
        db,
        "sel.subquery.exists",
        "SELECT a FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.a = t1.a)"
    );
    p!(
        db,
        "sel.subquery.notin",
        "SELECT a FROM t1 WHERE a NOT IN (SELECT a FROM t2)"
    );
    p!(
        db,
        "sel.subquery.correlated",
        "SELECT a FROM t1 WHERE a > (SELECT avg(a) FROM t2 WHERE t2.a < t1.a)"
    );
    p!(
        db,
        "sel.window",
        "SELECT a, row_number() OVER (ORDER BY a) FROM t1"
    );
    p!(
        db,
        "sel.window.partition",
        "SELECT a, sum(a) OVER (PARTITION BY b) FROM t1"
    );
    p!(
        db,
        "sel.window.frame",
        "SELECT a, sum(a) OVER (ORDER BY a ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t1"
    );
    p!(
        db,
        "sel.window.filter",
        "SELECT count(*) FILTER (WHERE a > 1) FROM t1"
    );
    p!(
        db,
        "sel.group.having",
        "SELECT b, count(*) FROM t1 GROUP BY b HAVING count(*) > 1"
    );
    p!(
        db,
        "sel.group.expr",
        "SELECT a % 2, count(*) FROM t1 GROUP BY a % 2"
    );
    p!(
        db,
        "sel.group.concat",
        "SELECT b, group_concat(a) FROM t1 GROUP BY b"
    );
    p!(db, "sel.limit.offset", "SELECT a FROM t1 LIMIT 2 OFFSET 1");
    p!(db, "sel.limit.comma", "SELECT a FROM t1 LIMIT 1, 2");

    // ============ Expressions ============
    w!(
        db,
        "expr.cast",
        "SELECT CAST ('42' AS INTEGER)",
        &Value::Integer(42)
    );
    w!(
        db,
        "expr.between",
        "SELECT 5 BETWEEN 1 AND 10",
        &Value::Integer(1)
    );
    w!(
        db,
        "expr.in.list",
        "SELECT 2 IN (1, 2, 3)",
        &Value::Integer(1)
    );
    w!(
        db,
        "expr.case",
        "SELECT CASE WHEN 1 THEN 'y' ELSE 'n' END",
        &Value::Text("y".into())
    );
    w!(
        db,
        "expr.case.base",
        "SELECT CASE 2 WHEN 1 THEN 'a' WHEN 2 THEN 'b' END",
        &Value::Text("b".into())
    );
    w!(db, "expr.isnull", "SELECT NULL IS NULL", &Value::Integer(1));
    w!(
        db,
        "expr.isnotnull",
        "SELECT NULL IS NOT NULL",
        &Value::Integer(0)
    );
    w!(db, "expr.is", "SELECT NULL IS NULL", &Value::Integer(1));
    w!(
        db,
        "expr.is_distinct",
        "SELECT 1 IS NOT 2",
        &Value::Integer(1)
    );
    w!(
        db,
        "expr.concat",
        "SELECT 'a' || 'b'",
        &Value::Text("ab".into())
    );
    w!(db, "expr.concat_null", "SELECT 'a' || NULL", &Value::Null);
    w!(
        db,
        "expr.iif",
        "SELECT iif (1, 't', 'f')",
        &Value::Text("t".into())
    );
    w!(db, "expr.nullif", "SELECT nullif (1, 1)", &Value::Null);
    w!(
        db,
        "expr.coalesce",
        "SELECT coalesce (NULL, NULL, 3)",
        &Value::Integer(3)
    );
    w!(
        db,
        "expr.glob",
        "SELECT 'abc' GLOB 'a*'",
        &Value::Integer(1)
    );
    w!(
        db,
        "expr.like.escape",
        "SELECT 'a_c' LIKE 'a\\_c' ESCAPE '\\'",
        &Value::Integer(1)
    );
    w!(
        db,
        "expr.exists",
        "SELECT EXISTS (SELECT 1)",
        &Value::Integer(1)
    );

    // ============ Scalar functions ============
    w!(db, "fn.abs", "SELECT abs (-5)", &Value::Integer(5));
    w!(db, "fn.sign", "SELECT sign (-3)", &Value::Integer(-1));
    w!(
        db,
        "fn.length",
        "SELECT length ('héllo')",
        &Value::Integer(5)
    );
    w!(
        db,
        "fn.length.blob",
        "SELECT length (x'0102')",
        &Value::Integer(2)
    );
    w!(
        db,
        "fn.unicode",
        "SELECT unicode ('A')",
        &Value::Integer(65)
    );
    w!(
        db,
        "fn.char",
        "SELECT char (65, 66)",
        &Value::Text("AB".into())
    );
    w!(
        db,
        "fn.hex",
        "SELECT hex ('abc')",
        &Value::Text("616263".into())
    );
    w!(
        db,
        "fn.unhex",
        "SELECT unhex ('616263')",
        &Value::Blob(vec![97, 98, 99])
    );
    w!(
        db,
        "fn.instr",
        "SELECT instr ('hello', 'l')",
        &Value::Integer(3)
    );
    w!(
        db,
        "fn.substr",
        "SELECT substr ('hello', 2, 3)",
        &Value::Text("ell".into())
    );
    w!(
        db,
        "fn.substr.neg",
        "SELECT substr ('hello', -3)",
        &Value::Text("llo".into())
    );
    w!(
        db,
        "fn.replace",
        "SELECT replace ('abcabc', 'b', 'X')",
        &Value::Text("aXcaXc".into())
    );
    w!(
        db,
        "fn.trim",
        "SELECT trim ('  x  ')",
        &Value::Text("x".into())
    );
    w!(
        db,
        "fn.trim.chars",
        "SELECT trim ('xxhixx', 'x')",
        &Value::Text("hi".into())
    );
    w!(
        db,
        "fn.ltrim",
        "SELECT ltrim ('  x')",
        &Value::Text("x".into())
    );
    w!(
        db,
        "fn.rtrim",
        "SELECT rtrim ('x  ')",
        &Value::Text("x".into())
    );
    w!(
        db,
        "fn.upper",
        "SELECT upper ('ab')",
        &Value::Text("AB".into())
    );
    w!(
        db,
        "fn.lower",
        "SELECT lower ('AB')",
        &Value::Text("ab".into())
    );
    w!(
        db,
        "fn.round",
        "SELECT round (2.567, 2)",
        &Value::Real(2.57)
    );
    w!(
        db,
        "fn.max.multi",
        "SELECT max (1, 5, 3)",
        &Value::Integer(5)
    );
    w!(
        db,
        "fn.min.multi",
        "SELECT min (1, 5, 3)",
        &Value::Integer(1)
    );
    w!(
        db,
        "fn.nullif.eq",
        "SELECT nullif (2, 3)",
        &Value::Integer(2)
    );
    w!(
        db,
        "fn.quote",
        "SELECT quote (x'0102')",
        &Value::Text("X'0102'".into())
    );
    w!(
        db,
        "fn.printf",
        "SELECT printf ('%d-%s', 5, 'x')",
        &Value::Text("5-x".into())
    );
    w!(
        db,
        "fn.format",
        "SELECT format ('%05d', 42)",
        &Value::Text("00042".into())
    );
    w!(
        db,
        "fn.zeroblob",
        "SELECT length (zeroblob (5))",
        &Value::Integer(5)
    );
    w!(
        db,
        "fn.typeof.blob",
        "SELECT typeof (x'01')",
        &Value::Text("blob".into())
    );
    w!(
        db,
        "fn.typeof.null",
        "SELECT typeof (NULL)",
        &Value::Text("null".into())
    );
    w!(db, "fn.likely", "SELECT likely (42)", &Value::Integer(42));
    w!(
        db,
        "fn.unlikely",
        "SELECT unlikely (42)",
        &Value::Integer(42)
    );

    // random is nondeterministic — just check type
    match scalar(&mut db, "SELECT typeof (random ())") {
        Some(Value::Text(t)) if t == "integer" => ok += 1,
        other => {
            println!("FAIL fn.random : {other:?}");
            fail += 1;
        }
    }

    // ============ Date/time ============
    w!(
        db,
        "dt.date",
        "SELECT date ('2024-03-15')",
        &Value::Text("2024-03-15".into())
    );
    w!(
        db,
        "dt.date.mod",
        "SELECT date ('2024-03-15', '+1 day')",
        &Value::Text("2024-03-16".into())
    );
    w!(
        db,
        "dt.time",
        "SELECT time ('12:30:45')",
        &Value::Text("12:30:45".into())
    );
    w!(
        db,
        "dt.datetime",
        "SELECT datetime ('2024-03-15 12:30', '+1 hour')",
        &Value::Text("2024-03-15 13:30:00".into())
    );
    w!(
        db,
        "dt.julianday",
        "SELECT CAST (julianday ('2000-01-01 12:00') AS INTEGER)",
        &Value::Integer(2451545)
    );
    w!(
        db,
        "dt.unixepoch",
        "SELECT unixepoch ('1970-01-01 00:00:00')",
        &Value::Integer(0)
    );
    w!(
        db,
        "dt.strftime",
        "SELECT strftime ('%Y-%m-%d', '2024-03-15')",
        &Value::Text("2024-03-15".into())
    );
    w!(
        db,
        "dt.strftime.weekday",
        "SELECT strftime ('%w', '2024-03-15')",
        &Value::Text("5".into())
    );
    w!(
        db,
        "dt.date.start_of_month",
        "SELECT date ('2024-03-15', 'start of month')",
        &Value::Text("2024-03-01".into())
    );
    w!(
        db,
        "dt.time.localtime_mod",
        "SELECT time ('12:00', 'utc')",
        &Value::Text("12:00:00".into())
    );
    w!(
        db,
        "dt.now",
        "SELECT length (datetime ('now'))",
        &Value::Integer(19)
    );

    // ============ JSON1 ============
    w!(
        db,
        "json.valid",
        "SELECT json_valid ('{\"a\": 1}')",
        &Value::Integer(1)
    );
    w!(
        db,
        "json.array",
        "SELECT json_array (1, 'x', 2.5, null)",
        &Value::Text("[1,\"x\",2.5,null]".into())
    );
    w!(
        db,
        "json.object",
        "SELECT json_object ('a', 1)",
        &Value::Text("{\"a\":1}".into())
    );
    w!(
        db,
        "json.quote",
        "SELECT json_quote ('x')",
        &Value::Text("\"x\"".into())
    );
    w!(
        db,
        "json.extract",
        "SELECT json_extract ('{\"a\": {\"b\": 2}}', '$.a.b')",
        &Value::Integer(2)
    );
    w!(
        db,
        "json.type",
        "SELECT json_type ('[1, 2]')",
        &Value::Text("array".into())
    );
    w!(
        db,
        "json.array_length",
        "SELECT json_array_length ('[1, 2, 3]')",
        &Value::Integer(3)
    );
    w!(
        db,
        "json.set",
        "SELECT json_set ('{\"a\": 1}', '$.b', 2)",
        &Value::Text("{\"a\":1,\"b\":2}".into())
    );
    w!(
        db,
        "json.insert",
        "SELECT json_insert ('{\"a\": 1}', '$.b', 2)",
        &Value::Text("{\"a\":1,\"b\":2}".into())
    );
    w!(
        db,
        "json.replace",
        "SELECT json_replace ('{\"a\": 1}', '$.a', 9)",
        &Value::Text("{\"a\":9}".into())
    );
    w!(
        db,
        "json.remove",
        "SELECT json_remove ('{\"a\": 1, \"b\": 2}', '$.b')",
        &Value::Text("{\"a\":1}".into())
    );
    w!(
        db,
        "json.patch",
        "SELECT json_patch ('{\"a\": 1}', '{\"b\": 2}')",
        &Value::Text("{\"a\":1,\"b\":2}".into())
    );
    w!(
        db,
        "json.minify",
        "SELECT json ('{\"a\": [1, 2]}')",
        &Value::Text("{\"a\":[1,2]}".into())
    );
    p!(db, "json.each", "SELECT value FROM json_each ('[1, 2]')", 2);
    p!(
        db,
        "json.tree",
        "SELECT path FROM json_tree ('{\"a\": 1}')",
        2
    );

    // ============ Math functions ============
    w!(db, "math.pow", "SELECT pow (2, 10)", &Value::Real(1024.0));
    w!(
        db,
        "math.acos",
        "SELECT round (acos (1), 4)",
        &Value::Real(0.0)
    );
    w!(
        db,
        "math.atanh",
        "SELECT round (atanh (0), 4)",
        &Value::Real(0.0)
    );
    w!(db, "math.ceil", "SELECT ceil (1.2)", &Value::Real(2.0));
    w!(db, "math.floor", "SELECT floor (1.8)", &Value::Real(1.0));
    w!(
        db,
        "math.log",
        "SELECT round (log (10), 6)",
        &Value::Real(1.0)
    );
    w!(db, "math.log2", "SELECT log2 (8)", &Value::Real(3.0));
    w!(db, "math.log10", "SELECT log10 (1000)", &Value::Real(3.0));
    w!(db, "math.mod", "SELECT mod (7, 3)", &Value::Integer(1));
    w!(
        db,
        "math.pi",
        "SELECT round (pi (), 5)",
        &Value::Real((std::f64::consts::PI * 1e5).round() / 1e5)
    );
    w!(
        db,
        "math.sin",
        "SELECT round (sin (0), 4)",
        &Value::Real(0.0)
    );
    w!(
        db,
        "math.exp",
        "SELECT round (exp (0), 4)",
        &Value::Real(1.0)
    );
    w!(db, "math.trunc", "SELECT trunc (1.9)", &Value::Real(1.0));
    w!(db, "math.sqrt", "SELECT sqrt (16)", &Value::Real(4.0));
    w!(
        db,
        "math.degrees",
        "SELECT round (degrees (0), 4)",
        &Value::Real(0.0)
    );
    w!(
        db,
        "math.radians",
        "SELECT round (radians (0), 4)",
        &Value::Real(0.0)
    );

    // ============ Aggregates ============
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t1 (a INTEGER PRIMARY KEY, b TEXT, c REAL)",
        [],
    )
    .unwrap();
    p!(db, "agg.setup", "INSERT INTO t1 (a, b, c) VALUES (0, 'g0', 0.5), (10, 'g1', 1.0), (11, 'g1', 2.0), (12, 'g2', 3.0)");
    w!(
        db,
        "agg.sum",
        "SELECT sum (c) FROM t1 WHERE b = 'g1'",
        &Value::Real(3.0)
    );
    w!(
        db,
        "agg.total",
        "SELECT total (c) FROM t1 WHERE b = 'g1'",
        &Value::Real(3.0)
    );
    w!(
        db,
        "agg.avg",
        "SELECT avg (c) FROM t1 WHERE b = 'g1'",
        &Value::Real(1.5)
    );
    w!(
        db,
        "agg.count.star",
        "SELECT count (*) FROM t1",
        &Value::Integer(4)
    );
    w!(
        db,
        "agg.count.distinct",
        "SELECT count (DISTINCT b) FROM t1",
        &Value::Integer(3)
    );
    w!(db, "agg.max", "SELECT max (a) FROM t1", &Value::Integer(12));
    w!(db, "agg.min", "SELECT min (a) FROM t1", &Value::Integer(0));
    w!(
        db,
        "agg.group_concat.sep",
        "SELECT group_concat (a, '-') FROM t1 WHERE b = 'g1'",
        &Value::Text("10-11".into())
    );
    w!(
        db,
        "agg.string_agg",
        "SELECT string_agg (a, '-') FROM t1 WHERE b = 'g1'",
        &Value::Text("10-11".into())
    );
    w!(
        db,
        "agg.sum_empty",
        "SELECT sum (a) FROM t1 WHERE a < 0",
        &Value::Null
    );
    w!(
        db,
        "agg.total_empty",
        "SELECT total (a) FROM t1 WHERE a < 0",
        &Value::Real(0.0)
    );
    w!(
        db,
        "agg.json_group_array",
        "SELECT json_group_array (a) FROM (SELECT 1 AS a)",
        &Value::Text("[1]".into())
    );
    w!(
        db,
        "agg.json_group_object",
        "SELECT json_group_object ('k', 1)",
        &Value::Text("{\"k\":1}".into())
    );

    // ============ Window functions (values!) ============
    {
        let mut db2 = Database::open_in_memory().unwrap();
        db2.execute(
            "CREATE TABLE w (id INTEGER PRIMARY KEY, g TEXT, v INTEGER)",
            [],
        )
        .unwrap();
        db2.execute(
            "INSERT INTO w (g, v) VALUES ('a', 1), ('a', 2), ('a', 3), ('b', 4), ('b', 5)",
            [],
        )
        .unwrap();
        w!(
            db2,
            "win.row_number",
            "SELECT row_number () OVER (ORDER BY id) FROM w LIMIT 1 OFFSET 4",
            &Value::Integer(5)
        );
        w!(
            db2,
            "win.rank",
            "SELECT rank () OVER (ORDER BY v) FROM w LIMIT 1",
            &Value::Integer(1)
        );
        w!(
            db2,
            "win.dense_rank",
            "SELECT dense_rank () OVER (ORDER BY g) FROM w LIMIT 1 OFFSET 4",
            &Value::Integer(2)
        );
        w!(
            db2,
            "win.percent_rank",
            "SELECT round (percent_rank () OVER (ORDER BY v), 4) FROM w LIMIT 1 OFFSET 1",
            &Value::Real(0.25)
        );
        w!(
            db2,
            "win.cume_dist",
            "SELECT round (cume_dist () OVER (ORDER BY v), 4) FROM w LIMIT 1",
            &Value::Real(0.2)
        );
        w!(
            db2,
            "win.ntile",
            "SELECT ntile (2) OVER (ORDER BY v) FROM w LIMIT 1 OFFSET 3",
            &Value::Integer(2)
        );
        w!(
            db2,
            "win.lag",
            "SELECT lag (v) OVER (ORDER BY id) FROM w LIMIT 1 OFFSET 1",
            &Value::Integer(1)
        );
        w!(
            db2,
            "win.lag.default",
            "SELECT lag (v, 1, 0) OVER (ORDER BY id) FROM w LIMIT 1",
            &Value::Integer(0)
        );
        w!(
            db2,
            "win.lead",
            "SELECT lead (v) OVER (ORDER BY id) FROM w LIMIT 1",
            &Value::Integer(2)
        );
        w!(
            db2,
            "win.first_value",
            "SELECT first_value (v) OVER (ORDER BY id) FROM w LIMIT 1 OFFSET 3",
            &Value::Integer(1)
        );
        w!(db2, "win.last_value", "SELECT last_value (v) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM w LIMIT 1", &Value::Integer(5));
        w!(
            db2,
            "win.nth_value",
            "SELECT nth_value (v, 2) OVER (ORDER BY id) FROM w LIMIT 1 OFFSET 1",
            &Value::Integer(2)
        );
        w!(db2, "win.sum.frame", "SELECT sum (v) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w LIMIT 1 OFFSET 1", &Value::Integer(3));
    }

    // ============ Transactions ============
    {
        let mut db2 = Database::open_in_memory().unwrap();
        db2.execute("CREATE TABLE x (a INTEGER PRIMARY KEY)", [])
            .unwrap();
        p!(db2, "tx.begin", "BEGIN");
        p!(db2, "tx.insert", "INSERT INTO x VALUES (1)");
        p!(db2, "tx.rollback", "ROLLBACK");
        w!(
            db2,
            "tx.rollback.effect",
            "SELECT count (*) FROM x",
            &Value::Integer(0)
        );
        p!(db2, "tx.begin.immediate", "BEGIN IMMEDIATE");
        p!(db2, "tx.commit", "COMMIT");
        p!(db2, "tx.begin.exclusive", "BEGIN EXCLUSIVE");
        p!(db2, "tx.commit2", "COMMIT");
        p!(db2, "tx.begin.deferred", "BEGIN DEFERRED");
        p!(db2, "tx.commit3", "COMMIT");
        p!(db2, "tx.savepoint", "SAVEPOINT sp1");
        p!(db2, "tx.sp.insert", "INSERT INTO x VALUES (5)");
        p!(db2, "tx.sp.rollback_to", "ROLLBACK TO sp1");
        w!(
            db2,
            "tx.sp.effect",
            "SELECT count (*) FROM x",
            &Value::Integer(0)
        );
        p!(db2, "tx.sp.release", "RELEASE sp1");
        // SQLite: RELEASE of the outermost savepoint (the one that
        // started the implicit transaction) ENDS that transaction — a
        // following BEGIN is legal, not an error.
        p!(db2, "tx.begin.after.release", "BEGIN");
        p!(db2, "tx.commit.after.release", "COMMIT");
    }

    // ============ Pragmas ============
    {
        let mut db2 = Database::open_in_memory().unwrap();
        db2.execute(
            "CREATE TABLE p (a INTEGER PRIMARY KEY, b TEXT COLLATE NOCASE)",
            [],
        )
        .unwrap();
        db2.execute("CREATE INDEX pi ON p (b)", []).unwrap();
        p!(db2, "prag.table_info", "PRAGMA table_info (p)");
        w!(
            db2,
            "prag.table_info.count",
            "SELECT count (*) FROM pragma_table_info ('p')",
            &Value::Integer(2)
        );
        p!(db2, "prag.foreign_key_list", "PRAGMA foreign_key_list (p)");
        p!(db2, "prag.index_list", "PRAGMA index_list (p)");
        p!(db2, "prag.index_info", "PRAGMA index_info (pi)");
        p!(db2, "prag.index_xinfo", "PRAGMA index_xinfo (pi)");
        w!(
            db2,
            "prag.encoding",
            "PRAGMA encoding",
            &Value::Text("UTF-8".into())
        );
        w!(
            db2,
            "prag.page_size",
            "PRAGMA page_size",
            &Value::Integer(4096)
        );
        p!(db2, "prag.journal_mode", "PRAGMA journal_mode = WAL");
        p!(db2, "prag.foreign_keys", "PRAGMA foreign_keys = ON");
        p!(db2, "prag.user_version.set", "PRAGMA user_version = 7");
        w!(
            db2,
            "prag.user_version.get",
            "PRAGMA user_version",
            &Value::Integer(7)
        );
        p!(db2, "prag.application_id", "PRAGMA application_id = 5");
        p!(db2, "prag.busy_timeout", "PRAGMA busy_timeout = 5000");
        p!(db2, "prag.cache_size", "PRAGMA cache_size = -2000");
        p!(db2, "prag.synchronous", "PRAGMA synchronous = NORMAL");
        p!(db2, "prag.temp_store", "PRAGMA temp_store = MEMORY");
        p!(db2, "prag.secure_delete", "PRAGMA secure_delete = ON");
        p!(db2, "prag.integrity_check", "PRAGMA integrity_check");
        p!(db2, "prag.quick_check", "PRAGMA quick_check");
        p!(db2, "prag.database_list", "PRAGMA database_list");
        p!(db2, "prag.collation_list", "PRAGMA collation_list");
        p!(db2, "prag.function_list", "PRAGMA function_list");
        p!(db2, "prag.module_list", "PRAGMA module_list");
        p!(db2, "prag.compile_options", "PRAGMA compile_options");
        p!(
            db2,
            "prag.max_page_count",
            "PRAGMA max_page_count = 1000000"
        );
        p!(db2, "prag.auto_vacuum", "PRAGMA auto_vacuum = NONE");
        p!(db2, "prag.count_changes", "PRAGMA count_changes = ON");
        p!(
            db2,
            "prag.recursive_triggers",
            "PRAGMA recursive_triggers = ON"
        );
        p!(
            db2,
            "prag.defer_foreign_keys",
            "PRAGMA defer_foreign_keys = ON"
        );
        p!(db2, "prag.locking_mode", "PRAGMA locking_mode = NORMAL");
        p!(db2, "prag.schema_version", "PRAGMA schema_version");
    }

    // ============ Foreign keys ============
    {
        let mut db2 = Database::open_in_memory().unwrap();
        db2.execute("PRAGMA foreign_keys = ON", []).unwrap();
        db2.execute(
            "CREATE TABLE parent (id INTEGER PRIMARY KEY, v TEXT UNIQUE)",
            [],
        )
        .unwrap();
        p!(
            db2,
            "fk.create",
            "CREATE TABLE child (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES parent (id))"
        );
        p!(
            db2,
            "fk.insert.ok",
            "INSERT INTO parent VALUES (1, 'x'), (2, 'y')"
        );
        e!(
            db2,
            "fk.violate.insert",
            "INSERT INTO child (pid) VALUES (99)",
            "foreign"
        );
        p!(
            db2,
            "fk.violate.update.prep",
            "INSERT INTO child (pid) VALUES (1)"
        );
        e!(
            db2,
            "fk.violate.update",
            "UPDATE child SET pid = 99",
            "foreign"
        );
        e!(
            db2,
            "fk.violate.delete",
            "DELETE FROM parent WHERE id = 1",
            "foreign"
        );
        p!(db2, "fk.cascade", "CREATE TABLE c2 (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES parent (id) ON DELETE CASCADE)");
        p!(db2, "fk.cascade.w", "PRAGMA foreign_keys = ON");
        p!(db2, "fk.setnull", "CREATE TABLE c3 (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES parent (id) ON DELETE SET NULL)");
        p!(
            db2,
            "fk.setdefault",
            "CREATE TABLE c4 (pid INTEGER DEFAULT 1 REFERENCES parent (id) ON DELETE SET DEFAULT)"
        );
        p!(
            db2,
            "fk.restrict",
            "CREATE TABLE c5 (pid INTEGER REFERENCES parent (id) ON DELETE RESTRICT)"
        );
        p!(
            db2,
            "fk.match",
            "CREATE TABLE c6 (pid INTEGER REFERENCES parent (id) MATCH SIMPLE)"
        );
        p!(
            db2,
            "fk.deferable",
            "CREATE TABLE c7 (pid INTEGER REFERENCES parent (id) DEFERRABLE INITIALLY DEFERRED)"
        );
    }

    // ============ Misc ============
    p!(db, "misc.attach", "ATTACH ':memory:' AS aux1");
    p!(db, "misc.detach", "DETACH aux1");
    p!(db, "misc.vacuum", "VACUUM");
    p!(db, "misc.explain", "EXPLAIN SELECT a FROM t1");
    p!(
        db,
        "misc.eqp",
        "EXPLAIN QUERY PLAN SELECT a FROM t1 WHERE a = 1"
    );
    p!(db, "misc.analyze", "ANALYZE");
    p!(db, "misc.reindex", "REINDEX");
    w!(
        db,
        "misc.last_insert_rowid",
        "SELECT last_insert_rowid ()",
        &Value::Integer(12)
    );
    w!(db, "misc.changes", "SELECT changes ()", &Value::Integer(0));
    p!(
        db,
        "misc.sqlite_master",
        "SELECT name FROM sqlite_master WHERE type = 'table' LIMIT 1"
    );
    p!(
        db,
        "misc.sqlite_schema",
        "SELECT name FROM sqlite_schema WHERE type = 'table' LIMIT 1"
    );
    w!(
        db,
        "misc.sqlite_version",
        "SELECT substr (sqlite_version (), 1, 1)",
        &Value::Text("3".into())
    );
    p!(db, "misc.comment", "SELECT 1 -- trailing comment");
    p!(db, "misc.comment.block", "SELECT /* block */ 1");
    w!(
        db,
        "misc.substring",
        "SELECT substring ('hello', 2)",
        &Value::Text("ello".into())
    );

    println!("\n== parity audit: {ok} PASS / {fail} FAIL ==");
}
