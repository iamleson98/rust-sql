//! FTS5 tests: create/insert/MATCH surface, query grammar, rank/bm25/
//! highlight/snippet, DML, persistence (reopen), rollback resync,
//! contentless + external-content tables, tokenizers — differential
//! against real SQLite (rusqlite bundled) wherever the oracle applies.

use rustqlite::types::Value;
use rustqlite::Database;
use std::io::Write;

fn lite() -> rusqlite::Connection {
    rusqlite::Connection::open_in_memory().unwrap()
}

// ---------------------------------------------------------------------------
// Basic surface
// ---------------------------------------------------------------------------

#[test]
fn fts5_create_insert_match() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE VIRTUAL TABLE t USING fts5(a, b)", [])
        .unwrap();
    db.execute(
        "INSERT INTO t VALUES ('the quick brown fox', 'jumps over the lazy dog')",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO t VALUES ('quick quick fox', 'brown dogs')", [])
        .unwrap();
    // Bare term.
    let rows = db
        .query("SELECT rowid FROM t WHERE t MATCH 'quick'", [])
        .unwrap();
    assert_eq!(rows.len(), 2);
    // Phrase.
    let rows = db
        .query("SELECT rowid FROM t WHERE t MATCH '\"brown fox\"'", [])
        .unwrap();
    assert_eq!(rows.len(), 1);
    // Column filter.
    let rows = db
        .query("SELECT rowid FROM t WHERE t MATCH 'b:quick'", [])
        .unwrap();
    assert_eq!(rows.len(), 0);
    // AND.
    let rows = db
        .query("SELECT rowid FROM t WHERE t MATCH 'quick AND brown'", [])
        .unwrap();
    assert_eq!(rows.len(), 2);
    // OR.
    let rows = db
        .query("SELECT rowid FROM t WHERE t MATCH 'dog OR plugh'", [])
        .unwrap();
    assert_eq!(rows.len(), 1);
    // NOT.
    let rows = db
        .query("SELECT rowid FROM t WHERE t MATCH 'quick NOT dog'", [])
        .unwrap();
    assert_eq!(rows.len(), 1);
    // Prefix.
    let rows = db
        .query("SELECT rowid FROM t WHERE t MATCH 'quic*'", [])
        .unwrap();
    assert_eq!(rows.len(), 2);
    // Parens.
    let rows = db
        .query(
            "SELECT rowid FROM t WHERE t MATCH '(quick OR dog) AND brown'",
            [],
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
    // Two MATCH constraints AND (SQLite behavior).
    let rows = db
        .query(
            "SELECT rowid FROM t WHERE t MATCH 'quick' AND t MATCH 'dog'",
            [],
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn fts5_near_and_anchor() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE VIRTUAL TABLE t USING fts5(a)", [])
        .unwrap();
    db.execute(
        "INSERT INTO t VALUES ('alpha beta gamma delta epsilon')",
        [],
    )
    .unwrap();
    // NEAR within 2 tokens (beta ... delta are 2 apart).
    let rows = db
        .query(
            "SELECT rowid FROM t WHERE t MATCH 'NEAR(beta delta, 2)'",
            [],
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    let rows = db
        .query(
            "SELECT rowid FROM t WHERE t MATCH 'NEAR(beta delta, 1)'",
            [],
        )
        .unwrap();
    assert_eq!(rows.len(), 0);
    // NEAR is column-scoped.
    db.execute(
        "CREATE VIRTUAL TABLE u USING fts5(a, b);
         INSERT INTO u VALUES ('quick here', 'dog there')",
        [],
    )
    .unwrap();
    let rows = db
        .query(
            "SELECT rowid FROM u WHERE u MATCH 'NEAR(quick dog, 10)'",
            [],
        )
        .unwrap();
    assert_eq!(rows.len(), 0);
    // ^ anchor.
    let rows = db
        .query("SELECT rowid FROM t WHERE t MATCH '^alpha'", [])
        .unwrap();
    assert_eq!(rows.len(), 1);
    let rows = db
        .query("SELECT rowid FROM t WHERE t MATCH '^beta'", [])
        .unwrap();
    assert_eq!(rows.len(), 0);
}

#[test]
fn fts5_module_list_and_info() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE VIRTUAL TABLE t USING fts5(a, b)", [])
        .unwrap();
    let rows = db.query("PRAGMA module_list", []).unwrap();
    let names: Vec<String> = rows.iter().map(|r| r[0].as_text().to_string()).collect();
    assert!(names.iter().any(|n| n == "fts5"), "{names:?}");
    // table_info: user columns only.
    let rows = db.query("PRAGMA table_info(t)", []).unwrap();
    assert_eq!(rows.len(), 2);
    // table_xinfo: + the self column and rank, hidden=1.
    let rows = db.query("PRAGMA table_xinfo(t)", []).unwrap();
    let mut hidden_count = 0;
    for r in &rows {
        if r.last().unwrap().as_integer() == 1 {
            hidden_count += 1;
        }
    }
    assert_eq!(hidden_count, 2, "{:?}", rows);
}

// ---------------------------------------------------------------------------
// DML + persistence
// ---------------------------------------------------------------------------

#[test]
fn fts5_update_delete() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE VIRTUAL TABLE t USING fts5(a)", [])
        .unwrap();
    db.execute("INSERT INTO t VALUES ('one two three')", [])
        .unwrap();
    db.execute("INSERT INTO t VALUES ('four five six')", [])
        .unwrap();
    db.execute("UPDATE t SET a = 'seven eight' WHERE a MATCH 'four'", [])
        .unwrap();
    let rows = db
        .query("SELECT a FROM t WHERE t MATCH 'seven'", [])
        .unwrap();
    assert_eq!(rows.len(), 1);
    let rows = db
        .query("SELECT a FROM t WHERE t MATCH 'four'", [])
        .unwrap();
    assert_eq!(rows.len(), 0);
    db.execute("DELETE FROM t WHERE t MATCH 'seven'", [])
        .unwrap();
    let rows = db.query("SELECT count(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 1);
}

#[test]
fn fts5_reopen_persistence() {
    let dir = std::env::temp_dir().join(format!(
        "fts5_reopen_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.db");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE VIRTUAL TABLE t USING fts5(a, b)", [])
            .unwrap();
        db.execute(
            "INSERT INTO t VALUES ('hello world', 'second column');
             INSERT INTO t VALUES ('world peace', 'another one')",
            [],
        )
        .unwrap();
    }
    let mut db = Database::open(&path).unwrap();
    let rows = db
        .query("SELECT rowid FROM t WHERE t MATCH 'world'", [])
        .unwrap();
    assert_eq!(rows.len(), 2, "index rebuilt from the shadow on reopen");
    let rows = db
        .query("SELECT a, b FROM t WHERE t MATCH 'peace'", [])
        .unwrap();
    assert_eq!(rows[0][0].as_text(), "world peace");
    // Post-reopen DML keeps working.
    db.execute("INSERT INTO t(a) VALUES ('brand new row')", [])
        .unwrap();
    let rows = db
        .query("SELECT count(*) FROM t WHERE t MATCH 'brand'", [])
        .unwrap();
    assert_eq!(rows[0][0].as_integer(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fts5_rollback_resync() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE VIRTUAL TABLE t USING fts5(a)", [])
        .unwrap();
    db.execute("INSERT INTO t VALUES ('base row')", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    db.execute("INSERT INTO t VALUES ('transient row')", [])
        .unwrap();
    db.execute("ROLLBACK", []).unwrap();
    let rows = db.query("SELECT count(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 1);
    // The module's index must not see the rolled-back row.
    let rows = db
        .query("SELECT count(*) FROM t WHERE t MATCH 'transient'", [])
        .unwrap();
    assert_eq!(rows[0][0].as_integer(), 0);
    let rows = db
        .query("SELECT count(*) FROM t WHERE t MATCH 'base'", [])
        .unwrap();
    assert_eq!(rows[0][0].as_integer(), 1);
}

#[test]
fn fts5_drop_table() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE VIRTUAL TABLE t USING fts5(a)", [])
        .unwrap();
    db.execute("INSERT INTO t VALUES ('x')", []).unwrap();
    db.execute("DROP TABLE t", []).unwrap();
    // The shadow table dropped with it.
    let rows = db
        .query(
            "SELECT count(*) FROM sqlite_schema WHERE name = 't_content'",
            [],
        )
        .unwrap();
    assert_eq!(rows[0][0].as_integer(), 0);
    // And the name is reusable.
    db.execute("CREATE VIRTUAL TABLE t USING fts5(a)", [])
        .unwrap();
    db.execute("INSERT INTO t VALUES ('fresh')", []).unwrap();
    let rows = db
        .query("SELECT count(*) FROM t WHERE t MATCH 'fresh'", [])
        .unwrap();
    assert_eq!(rows[0][0].as_integer(), 1);
}

#[test]
fn fts5_explicit_rowid() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE VIRTUAL TABLE t USING fts5(a)", [])
        .unwrap();
    db.execute("INSERT INTO t(rowid, a) VALUES (10, 'xyzzy')", [])
        .unwrap();
    let rows = db.query("SELECT rowid, a FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 10);
    assert_eq!(rows[0][1].as_text(), "xyzzy");
}

// ---------------------------------------------------------------------------
// rank / bm25 / highlight / snippet (differential vs real SQLite)
// ---------------------------------------------------------------------------

fn corpus() -> Vec<(&'static str, &'static str)> {
    vec![
        ("the quick brown fox", "jumps over the lazy dog"),
        ("quick quick fox", "brown dogs everywhere"),
        ("slow green turtle", "the quiet swamp"),
        ("quick brown dogs", "leap over lazy foxes"),
    ]
}

#[test]
fn fts5_bm25_rank_differential() {
    let mut ours = Database::open_in_memory().unwrap();
    let oracle = lite();
    ours.execute("CREATE VIRTUAL TABLE t USING fts5(a, b)", [])
        .unwrap();
    oracle
        .execute_batch("CREATE VIRTUAL TABLE t USING fts5(a, b);")
        .unwrap();
    for (a, b) in corpus() {
        ours.execute(
            "INSERT INTO t VALUES (?, ?)",
            [Value::Text(a.into()), Value::Text(b.into())],
        )
        .unwrap();
        oracle
            .execute("INSERT INTO t VALUES (?, ?)", rusqlite::params![a, b])
            .unwrap();
    }
    for q in ["quick", "quick OR turtle", "dogs", "\"lazy fox\"", "over"] {
        // ORDER BY rank must produce the same rowid sequence.
        let ours_rows = ours
            .query(
                "SELECT rowid FROM t WHERE t MATCH ? ORDER BY rank",
                [Value::Text(q.into())],
            )
            .unwrap();
        let ours_ids: Vec<i64> = ours_rows.iter().map(|r| r[0].as_integer()).collect();
        let mut stmt = oracle
            .prepare("SELECT rowid FROM t WHERE t MATCH ? ORDER BY rank")
            .unwrap();
        let oracle_ids: Vec<i64> = stmt
            .query_map(rusqlite::params![q], |r| r.get::<_, i64>(0))
            .unwrap()
            .map(|x| x.unwrap())
            .collect();
        assert_eq!(ours_ids, oracle_ids, "query {q}: rank ordering diverged");
        // bm25() values within a tight epsilon.
        let ours_b = ours
            .query(
                "SELECT rowid, bm25(t) FROM t WHERE t MATCH ? ORDER BY rowid",
                [Value::Text(q.into())],
            )
            .unwrap();
        let mut stmt = oracle
            .prepare("SELECT rowid, bm25(t) FROM t WHERE t MATCH ? ORDER BY rowid")
            .unwrap();
        let oracle_b: Vec<(i64, f64)> = stmt
            .query_map(rusqlite::params![q], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
            })
            .unwrap()
            .map(|x| x.unwrap())
            .collect();
        assert_eq!(ours_b.len(), oracle_b.len(), "query {q}");
        for (i, (rid, b)) in oracle_b.iter().enumerate() {
            let ours_rid = ours_b[i][0].as_integer();
            let ours_val = ours_b[i][1].as_real();
            assert_eq!(ours_rid, *rid, "query {q} row {i}");
            assert!(
                (ours_val - b).abs() < 1e-9 * b.abs().max(1.0),
                "query {q} row {i}: bm25 {ours_val} vs {b}"
            );
        }
    }
}

#[test]
fn fts5_highlight_snippet_differential() {
    let mut ours = Database::open_in_memory().unwrap();
    let oracle = lite();
    ours.execute("CREATE VIRTUAL TABLE t USING fts5(a, b)", [])
        .unwrap();
    oracle
        .execute_batch("CREATE VIRTUAL TABLE t USING fts5(a, b);")
        .unwrap();
    for (a, b) in corpus() {
        ours.execute(
            "INSERT INTO t VALUES (?, ?)",
            [Value::Text(a.into()), Value::Text(b.into())],
        )
        .unwrap();
        oracle
            .execute("INSERT INTO t VALUES (?, ?)", rusqlite::params![a, b])
            .unwrap();
    }
    let queries = [
        (
            "SELECT highlight(t, 0, '<', '>') FROM t WHERE t MATCH 'quick'",
            "quick",
        ),
        (
            "SELECT highlight(t, 1, '[', ']') FROM t WHERE t MATCH 'lazy'",
            "lazy",
        ),
        (
            "SELECT highlight(t, 0, '(', ')') FROM t WHERE t MATCH '\"quick brown\"'",
            "qb",
        ),
        (
            "SELECT snippet(t, 0, '<', '>', '...', 2) FROM t WHERE t MATCH 'lazy'",
            "snip-lazy",
        ),
        (
            "SELECT snippet(t, 1, '<', '>', '...', 4) FROM t WHERE t MATCH 'dog'",
            "snip-dog",
        ),
        (
            "SELECT snippet(t, -1, '<', '>', '...', 3) FROM t WHERE t MATCH 'over'",
            "auto-col",
        ),
    ];
    for (sql, tag) in queries {
        let ours_rows = ours.query(sql, []).unwrap();
        let mut stmt = oracle.prepare(sql).unwrap();
        let oracle_rows: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|x| x.unwrap())
            .collect();
        assert_eq!(ours_rows.len(), oracle_rows.len(), "{tag}");
        for (i, o) in oracle_rows.iter().enumerate() {
            assert_eq!(&ours_rows[i][0].as_text(), o, "{tag} row {i}");
        }
    }
}

// ---------------------------------------------------------------------------
// Contentless + external content
// ---------------------------------------------------------------------------

#[test]
fn fts5_contentless() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE VIRTUAL TABLE cc USING fts5(a, b, content='')", [])
        .unwrap();
    db.execute(
        "INSERT INTO cc(rowid, a, b) VALUES (1, 'alpha', 'beta')",
        [],
    )
    .unwrap();
    // Content reads return NULL; the index works.
    let rows = db
        .query("SELECT a, rowid FROM cc WHERE cc MATCH 'alpha'", [])
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0][0].is_null());
    assert_eq!(rows[0][1].as_integer(), 1);
    // DELETE works without the 'delete' command (the module keeps its
    // own doc map).
    db.execute("DELETE FROM cc WHERE cc MATCH 'alpha'", [])
        .unwrap();
    let rows = db
        .query("SELECT count(*) FROM cc WHERE cc MATCH 'alpha'", [])
        .unwrap();
    assert_eq!(rows[0][0].as_integer(), 0);
}

#[test]
fn fts5_external_content_trigger_pattern() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE stuff(id INTEGER PRIMARY KEY, title TEXT, body TEXT);
         CREATE VIRTUAL TABLE ftsx USING fts5(title, body, content='stuff', content_rowid='id');
         CREATE TRIGGER stuff_ai AFTER INSERT ON stuff BEGIN
           INSERT INTO ftsx(rowid, title, body) VALUES (new.id, new.title, new.body);
         END;
         CREATE TRIGGER stuff_ad AFTER DELETE ON stuff BEGIN
           INSERT INTO ftsx(ftsx, rowid, title, body) VALUES('delete', old.id, old.title, old.body);
         END;",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO stuff(title, body) VALUES ('first post', 'hello world');
         INSERT INTO stuff(title, body) VALUES ('second post', 'quick brown fox')",
        [],
    )
    .unwrap();
    // Reads serve the CONTENT table's values.
    let rows = db
        .query("SELECT title, body FROM ftsx WHERE ftsx MATCH 'hello'", [])
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].as_text(), "first post");
    assert_eq!(rows[0][1].as_text(), "hello world");
    // The canonical delete path: trigger → 'delete' command.
    db.execute("DELETE FROM stuff WHERE id = 1", []).unwrap();
    let rows = db
        .query("SELECT count(*) FROM ftsx WHERE ftsx MATCH 'hello'", [])
        .unwrap();
    assert_eq!(rows[0][0].as_integer(), 0);
    let rows = db
        .query("SELECT count(*) FROM ftsx WHERE ftsx MATCH 'quick'", [])
        .unwrap();
    assert_eq!(rows[0][0].as_integer(), 1);
}

// ---------------------------------------------------------------------------
// Tokenizers
// ---------------------------------------------------------------------------

#[test]
fn fts5_porter_tokenizer() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE VIRTUAL TABLE p USING fts5(a, tokenize='porter')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO p VALUES ('running quickly'); INSERT INTO p VALUES ('walked away')",
        [],
    )
    .unwrap();
    // Stems: 'running' → 'run', so 'run' matches.
    let rows = db
        .query("SELECT rowid FROM p WHERE p MATCH 'run'", [])
        .unwrap();
    assert_eq!(rows.len(), 1);
    let rows = db
        .query("SELECT rowid FROM p WHERE p MATCH 'quickly'", [])
        .unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn fts5_trigram_tokenizer() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE VIRTUAL TABLE g USING fts5(a, tokenize='trigram')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO g VALUES ('alphabet soup'); INSERT INTO g VALUES ('gamma rays')",
        [],
    )
    .unwrap();
    // Substring MATCH.
    let rows = db
        .query("SELECT rowid FROM g WHERE g MATCH 'lphab'", [])
        .unwrap();
    assert_eq!(rows.len(), 1);
    // %...% LIKE acceleration.
    let rows = db
        .query("SELECT rowid FROM g WHERE a LIKE '%amma%'", [])
        .unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn fts5_unicode_diacritics() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE VIRTUAL TABLE d USING fts5(a)", [])
        .unwrap();
    db.execute("INSERT INTO d VALUES ('café naïve')", [])
        .unwrap();
    // The default unicode61 folds diacritics away.
    let rows = db
        .query("SELECT rowid FROM d WHERE d MATCH 'cafe'", [])
        .unwrap();
    assert_eq!(rows.len(), 1);
    let rows = db
        .query("SELECT rowid FROM d WHERE d MATCH 'naive'", [])
        .unwrap();
    assert_eq!(rows.len(), 1);
}

// ---------------------------------------------------------------------------
// Stress + battle testing
// ---------------------------------------------------------------------------

#[test]
fn fts5_randomized_differential() {
    let mut ours = Database::open_in_memory().unwrap();
    let oracle = lite();
    ours.execute("CREATE VIRTUAL TABLE t USING fts5(a, b)", [])
        .unwrap();
    oracle
        .execute_batch("CREATE VIRTUAL TABLE t USING fts5(a, b);")
        .unwrap();
    let words = [
        "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta",
    ];
    let mut lcg: u64 = 0x1234_5678_9abc_def0;
    let mut next = move || {
        lcg = lcg
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (lcg >> 33) as usize
    };
    for round in 0..120 {
        let mut pick = |n: usize| words[next() % n];
        let doc = format!("{} {} {}", pick(8), pick(5), pick(8));
        let doc2 = format!("{} {}", pick(8), pick(3));
        ours.execute(
            "INSERT INTO t VALUES (?, ?)",
            [
                Value::Text(doc.as_str().into()),
                Value::Text(doc2.as_str().into()),
            ],
        )
        .unwrap();
        oracle
            .execute("INSERT INTO t VALUES (?, ?)", rusqlite::params![doc, doc2])
            .unwrap();
        if round % 7 == 3 {
            let q = pick(8);
            let ours_ids: Vec<i64> = ours
                .query(
                    "SELECT rowid FROM t WHERE t MATCH ?",
                    [Value::Text(q.into())],
                )
                .unwrap()
                .iter()
                .map(|r| r[0].as_integer())
                .collect();
            let mut stmt = oracle
                .prepare("SELECT rowid FROM t WHERE t MATCH ?")
                .unwrap();
            let oracle_ids: Vec<i64> = stmt
                .query_map(rusqlite::params![q], |r| r.get::<_, i64>(0))
                .unwrap()
                .map(|x| x.unwrap())
                .collect();
            assert_eq!(ours_ids, oracle_ids, "round {round} query {q}");
        }
    }
}

#[test]
fn fts5_concurrent_ish_stress() {
    // Interleaved multi-connection traffic on one file: reader queries
    // while a writer churns (the reindex machinery must stay consistent).
    let dir = std::env::temp_dir().join(format!(
        "fts5_stress_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.db");
    let mut writer = Database::open(&path).unwrap();
    writer
        .execute("CREATE VIRTUAL TABLE t USING fts5(a)", [])
        .unwrap();
    for i in 0..50 {
        writer
            .execute(
                "INSERT INTO t VALUES (?)",
                [Value::Text(format!("word{i} filler text {i}").into())],
            )
            .unwrap();
        if i % 10 == 5 {
            let reader = Database::open(&path).unwrap();
            let rows = reader
                .query("SELECT count(*) FROM t WHERE t MATCH 'filler'", [])
                .unwrap();
            assert_eq!(rows[0][0].as_integer(), (i + 1) as i64);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// Silence unused warnings for helpers the differential tests share.
#[allow(dead_code)]
fn _write_stdout(s: &str) {
    let _ = std::io::stdout().write_all(s.as_bytes());
}
