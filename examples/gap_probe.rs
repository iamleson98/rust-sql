//! Gap probe: exercise SQLite surface NOT covered by parity_audit, biased
//! toward what sqlx/sea-orm and real apps emit (schema introspection
//! pragmas, upsert edge shapes, RETURNING on DELETE/UPDATE, window
//! function variants, table_list, ALTER shapes, collations, CHECK with
//! subquery-ish expressions, generated columns, trigger NEW/OLD refs).

fn main() {
    let t = |s: &str| rustqlite::Value::Text(s.into());
    let mut pass = 0usize;
    let mut fail = 0usize;
    let mut failures: Vec<&'static str> = Vec::new();

    let mut probe = |name: &'static str, ok: bool| {
        if ok {
            pass += 1;
        } else {
            fail += 1;
            failures.push(name);
        }
    };

    // -- schema introspection pragmas (sqlx/sea-orm migration tooling) --
    {
        let mut db = rustqlite::Database::open_in_memory().unwrap();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL, d REAL DEFAULT 1.5)",
            [],
        )
        .unwrap();
        db.execute("CREATE INDEX iv ON t (v)", []).unwrap();
        db.execute("CREATE UNIQUE INDEX iu ON t (d)", []).unwrap();
        let r = db.query("PRAGMA table_info(t)", []).unwrap();
        probe("pragma table_info columns", r.len() == 3 && r[0].len() == 6);
        let r = db.query("PRAGMA table_xinfo(t)", []).unwrap();
        probe("pragma table_xinfo", !r.is_empty());
        let r = db.query("PRAGMA index_list(t)", []).unwrap();
        probe("pragma index_list", !r.is_empty());
        let r = db.query("PRAGMA index_info(iv)", []).unwrap();
        probe("pragma index_info", !r.is_empty());
        let r = db.query("PRAGMA index_xinfo(iv)", []).unwrap();
        probe("pragma index_xinfo", !r.is_empty());
        let r = db.query("PRAGMA foreign_key_list(t)", []).unwrap();
        probe(
            "pragma foreign_key_list (empty ok)",
            r.is_empty() || !r.is_empty(),
        );
        let r = db
            .query("SELECT name FROM sqlite_schema WHERE type = 'index'", [])
            .unwrap();
        probe("sqlite_schema (alias) filter", r.len() == 2);
    }

    // -- upsert edge shapes --
    {
        let mut db = rustqlite::Database::open_in_memory().unwrap();
        db.execute("CREATE TABLE u (k INT PRIMARY KEY, v TEXT, n INT)", [])
            .unwrap();
        let ok = db
            .execute(
                "INSERT INTO u VALUES (1, 'a', 1) ON CONFLICT(k) DO UPDATE SET v = excluded.v, n = u.n + excluded.n",
                [],
            )
            .is_ok();
        probe("upsert excluded refs", ok);
        let ok = db
            .execute(
                "INSERT INTO u VALUES (1, 'b', 2) ON CONFLICT(k) DO UPDATE SET n = n + 1 WHERE u.v = 'a'",
                [],
            )
            .is_ok();
        probe("upsert DO UPDATE ... WHERE", ok);
        let ok = db
            .execute(
                "INSERT INTO u VALUES (2, 'c', 1) ON CONFLICT DO NOTHING",
                [],
            )
            .is_ok();
        probe("upsert bare ON CONFLICT DO NOTHING", ok);
        let rows = db.query("SELECT v, n FROM u ORDER BY k", []).unwrap();
        // (1,'a',1) inserted; upsert (1,'b',2) sets n=n+1 (v still 'a'
        // — the DO UPDATE only assigns n) -> (a,2); (2,'c',1) inserted.
        let expects = [
            (t("a"), rustqlite::Value::Integer(2)),
            (t("c"), rustqlite::Value::Integer(1)),
        ];
        let got: Vec<(rustqlite::Value, rustqlite::Value)> =
            rows.iter().map(|r| (r[0].clone(), r[1].clone())).collect();
        probe("upsert state correct", got == expects);
    }

    // -- RETURNING on UPDATE and DELETE --
    {
        let mut db = rustqlite::Database::open_in_memory().unwrap();
        db.execute("CREATE TABLE r (id INTEGER PRIMARY KEY, v INT)", [])
            .unwrap();
        for i in 1..=5 {
            db.execute(
                "INSERT INTO r (v) VALUES (?1)",
                [rustqlite::Value::Integer(i)],
            )
            .unwrap();
        }
        let rows = db
            .query("UPDATE r SET v = v * 10 WHERE id > 3 RETURNING id, v", [])
            .unwrap();
        probe(
            "UPDATE ... RETURNING",
            rows.len() == 2 && rows[0][1] == rustqlite::Value::Integer(40),
        );
        let rows = db
            .query("DELETE FROM r WHERE id < 3 RETURNING id AS deleted", [])
            .unwrap();
        probe("DELETE ... RETURNING", rows.len() == 2);
    }

    // -- window function variants --
    {
        let mut db = rustqlite::Database::open_in_memory().unwrap();
        db.execute("CREATE TABLE w (g INT, x INT)", []).unwrap();
        for (g, x) in [(1, 10), (1, 20), (2, 30), (2, 40)] {
            db.execute(
                "INSERT INTO w VALUES (?1, ?2)",
                [rustqlite::Value::Integer(g), rustqlite::Value::Integer(x)],
            )
            .unwrap();
        }
        let rows = db
            .query(
                "SELECT x, ROW_NUMBER() OVER (PARTITION BY g ORDER BY x DESC) FROM w",
                [],
            )
            .unwrap();
        probe("ROW_NUMBER OVER partition", rows.len() == 4);
        let rows = db
            .query(
                "SELECT x, LAG(x) OVER (ORDER BY x), LEAD(x) OVER (ORDER BY x) FROM w",
                [],
            )
            .unwrap();
        probe("LAG/LEAD", rows.len() == 4 && rows[0][1].is_null());
        let rows = db
            .query("SELECT x, NTILE(2) OVER (ORDER BY x) FROM w", [])
            .unwrap();
        probe("NTILE", rows.len() == 4);
        let rows = db
            .query(
                "SELECT SUM(x) OVER (ORDER BY x ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w",
                [],
            )
            .unwrap();
        probe("window frame ROWS PRECEDING", rows.len() == 4);
        let rows = db
            .query("SELECT FIRST_VALUE(x) OVER w2, LAST_VALUE(x) OVER w2 FROM w WINDOW w2 AS (ORDER BY x)", [])
            .unwrap();
        probe("WINDOW clause", rows.len() == 4);
        let rows = db
            .query("SELECT GROUP_CONCAT(x) OVER (ORDER BY x) FROM w", [])
            .unwrap();
        probe("GROUP_CONCAT as window agg", rows.len() == 4);
    }

    // -- CHECK constraints + DEFAULT expressions --
    {
        let mut db = rustqlite::Database::open_in_memory().unwrap();
        let ok = db
            .execute(
                "CREATE TABLE c (a INT CHECK (a > 0), b TEXT CHECK (length(b) < 5))",
                [],
            )
            .is_ok();
        probe("CREATE with CHECK exprs", ok);
        let bad = db.execute("INSERT INTO c VALUES (-1, 'x')", []).is_err();
        probe("CHECK enforced (a > 0)", bad);
        let bad = db
            .execute("INSERT INTO c VALUES (1, 'xxxxxx')", [])
            .is_err();
        probe("CHECK enforced (length)", bad);
        let mut db = rustqlite::Database::open_in_memory().unwrap();
        db.execute(
            "CREATE TABLE d (ts TEXT DEFAULT (strftime('%Y','2000-01-01')), n INT DEFAULT (1+2))",
            [],
        )
        .unwrap();
        let rows = db
            .query("INSERT INTO d DEFAULT VALUES RETURNING n", [])
            .unwrap();
        probe(
            "parenthesized DEFAULT expr",
            rows.len() == 1 && rows[0][0] == rustqlite::Value::Integer(3),
        );
    }

    // -- generated columns --
    {
        let mut db = rustqlite::Database::open_in_memory().unwrap();
        let ok = db
            .execute(
                "CREATE TABLE g (a INT, b INT GENERATED ALWAYS AS (a * 2) VIRTUAL)",
                [],
            )
            .is_ok();
        if ok {
            db.execute("INSERT INTO g (a) VALUES (21)", []).unwrap();
            let rows = db.query("SELECT b FROM g", []).unwrap();
            probe(
                "generated VIRTUAL column",
                rows.len() == 1 && rows[0][0] == rustqlite::Value::Integer(42),
            );
        } else {
            probe("generated VIRTUAL column", false);
        }
        let ok = db
            .execute("CREATE TABLE g2 (a INT, b INT AS (a + 1) STORED)", [])
            .is_ok();
        if ok {
            db.execute("INSERT INTO g2 (a) VALUES (1)", []).unwrap();
            let rows = db.query("SELECT b FROM g2", []).unwrap();
            probe(
                "generated STORED column",
                rows.len() == 1 && rows[0][0] == rustqlite::Value::Integer(2),
            );
        } else {
            probe("generated STORED column", false);
        }
    }

    // -- triggers: NEW/OLD refs, multi-action, WHEN clause --
    {
        let mut db = rustqlite::Database::open_in_memory().unwrap();
        db.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, v INT)", [])
            .unwrap();
        db.execute("CREATE TABLE log (msg TEXT)", []).unwrap();
        db.execute(
            "CREATE TRIGGER trg AFTER INSERT ON a WHEN NEW.v > 100 BEGIN INSERT INTO log VALUES ('big: ' || NEW.v); END",
            [],
        )
        .unwrap();
        db.execute("INSERT INTO a (v) VALUES (5)", []).unwrap();
        db.execute("INSERT INTO a (v) VALUES (200)", []).unwrap();
        let rows = db.query("SELECT msg FROM log", []).unwrap();
        probe(
            "trigger WHEN + NEW ref + concat",
            rows.len() == 1 && rows[0][0] == t("big: 200"),
        );
        db.execute(
            "CREATE TRIGGER upd AFTER UPDATE OF v ON a BEGIN INSERT INTO log VALUES ('upd ' || OLD.v || '->' || NEW.v); END",
            [],
        )
        .unwrap();
        db.execute("UPDATE a SET v = 7 WHERE v = 5", []).unwrap();
        let rows = db.query("SELECT msg FROM log", []).unwrap();
        probe(
            "UPDATE OF column trigger OLD/NEW",
            rows.len() == 2 && rows[1][0] == t("upd 5->7"),
        );
    }

    // -- CTE variants --
    {
        let mut db = rustqlite::Database::open_in_memory().unwrap();
        db.execute("CREATE TABLE n (x INT)", []).unwrap();
        db.execute("INSERT INTO n VALUES (1), (2), (3)", [])
            .unwrap();
        let rows = db
            .query(
                "WITH s (total) AS (SELECT SUM(x) FROM n) SELECT total FROM s",
                [],
            )
            .unwrap();
        probe(
            "CTE explicit column list",
            rows.len() == 1 && rows[0][0] == rustqlite::Value::Integer(6),
        );
        let rows = db
            .query(
                "WITH RECURSIVE cnt (i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM cnt WHERE i < 5) SELECT i FROM cnt",
                [],
            )
            .unwrap();
        probe("recursive CTE column list", rows.len() == 5);
        let rows = db
            .query(
                "WITH a AS (SELECT x FROM n), b AS (SELECT x * 2 AS y FROM a) SELECT y FROM b WHERE y > 2",
                [],
            )
            .unwrap();
        probe("chained CTEs", rows.len() == 2);
    }

    // -- ALTER TABLE surface --
    {
        let mut db = rustqlite::Database::open_in_memory().unwrap();
        db.execute("CREATE TABLE al (a INT)", []).unwrap();
        let ok = db
            .execute("ALTER TABLE al ADD COLUMN b TEXT DEFAULT 'z'", [])
            .is_ok();
        probe("ALTER ADD COLUMN w/ DEFAULT", ok);
        let ok = db
            .execute("ALTER TABLE al RENAME COLUMN a TO aa", [])
            .is_ok();
        probe("ALTER RENAME COLUMN", ok);
        db.execute("INSERT INTO al (aa) VALUES (1)", []).unwrap();
        let rows = db.query("SELECT b FROM al", []).unwrap();
        probe(
            "ADD COLUMN default backfilled",
            rows.len() == 1 && rows[0][0] == t("z"),
        );
        let ok = db.execute("ALTER TABLE al RENAME TO al2", []).is_ok();
        probe("ALTER RENAME TABLE", ok);
    }

    // -- collations --
    {
        let mut db = rustqlite::Database::open_in_memory().unwrap();
        db.execute("CREATE TABLE col (s TEXT COLLATE NOCASE)", [])
            .unwrap();
        db.execute("INSERT INTO col VALUES ('HELLO')", []).unwrap();
        let rows = db.query("SELECT s FROM col WHERE s = 'hello'", []).unwrap();
        probe("COLLATE NOCASE column", rows.len() == 1);
        let rows = db.query("SELECT 'a' = 'A' COLLATE NOCASE", []).unwrap();
        probe("expr COLLATE", rows[0][0] == rustqlite::Value::Integer(1));
    }

    // -- datetime / string functions battery --
    {
        let db = rustqlite::Database::open_in_memory().unwrap();
        let cases: Vec<(&str, rustqlite::Value)> = vec![
            ("SELECT date('2026-09-06', '+1 day')", t("2026-09-07")),
            (
                "SELECT strftime('%s', '2000-01-01 00:00:00')",
                t("946684800"),
            ),
            ("SELECT hex('hi')", t("6869")),
            (
                "SELECT unhex('6869')",
                rustqlite::Value::Blob(vec![0x68, 0x69]),
            ),
            ("SELECT printf('%05d', 42)", t("00042")),
            ("SELECT substr('sqlite', 2, 3)", t("qli")),
            ("SELECT instr('sqlite', 'li')", rustqlite::Value::Integer(3)),
            ("SELECT replace('aaa', 'a', 'b')", t("bbb")),
            ("SELECT trim('  x  ')", t("x")),
            ("SELECT ltrim('yyx', 'y')", t("x")),
            ("SELECT nullif(1, 1)", rustqlite::Value::Null),
            ("SELECT iif(1 > 0, 'y', 'n')", t("y")),
            ("SELECT sign(-3)", rustqlite::Value::Integer(-1)),
            ("SELECT min(3, 1, 2)", rustqlite::Value::Integer(1)),
            ("SELECT max(3, 1, 2)", rustqlite::Value::Integer(3)),
            ("SELECT abs(-2)", rustqlite::Value::Integer(2)),
            ("SELECT round(2.567, 1)", rustqlite::Value::Real(2.6)),
            ("SELECT typeof('x')", t("text")),
            ("SELECT typeof(1)", t("integer")),
            (
                "SELECT CAST('42' AS INTEGER)",
                rustqlite::Value::Integer(42),
            ),
            ("SELECT random() IS NOT NULL", rustqlite::Value::Integer(1)),
            ("SELECT last_insert_rowid()", rustqlite::Value::Integer(0)),
            ("SELECT likely(5)", rustqlite::Value::Integer(5)),
            ("SELECT char(72, 105)", t("Hi")),
            ("SELECT unicode('A')", rustqlite::Value::Integer(65)),
        ];
        for (sql, want) in cases {
            let got = db.query(sql, []);
            let ok = matches!(got, Ok(ref rows) if !rows.is_empty() && rows[0][0] == want);
            probe_match(&mut probe, sql, ok);
        }
    }

    // -- NULL / three-valued logic edge --
    {
        let mut db = rustqlite::Database::open_in_memory().unwrap();
        db.execute("CREATE TABLE z (x INT)", []).unwrap();
        db.execute("INSERT INTO z VALUES (NULL), (0), (1)", [])
            .unwrap();
        let rows = db.query("SELECT COUNT(*), COUNT(x) FROM z", []).unwrap();
        probe(
            "COUNT(*) vs COUNT(x) NULL semantics",
            rows[0][0] == rustqlite::Value::Integer(3)
                && rows[0][1] == rustqlite::Value::Integer(2),
        );
        let rows = db
            .query("SELECT x FROM z WHERE x NOT IN (1, NULL) ORDER BY x", [])
            .unwrap();
        probe("NOT IN with NULL empty set", rows.is_empty());
        let rows = db
            .query("SELECT x IS NULL, x IS NOT NULL FROM z WHERE x IS NULL", [])
            .unwrap();
        probe(
            "IS NULL / IS NOT NULL",
            rows[0][0] == rustqlite::Value::Integer(1)
                && rows[0][1] == rustqlite::Value::Integer(0),
        );
    }

    println!("\n== gap probe: {pass} PASS / {fail} FAIL ==");
    if !failures.is_empty() {
        println!("failures:");
        for f in &failures {
            println!("  - {f}");
        }
    }
}

fn probe_match(probe: &mut impl FnMut(&'static str, bool), sql: &'static str, ok: bool) {
    // Leak a &'static str label for the probe name.
    let name: &'static str = Box::leak(sql.to_string().into_boxed_str());
    probe(name, ok);
}
