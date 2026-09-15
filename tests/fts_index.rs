//! End-to-end tests for the GIN-style inverted index (`CREATE INDEX ...
//! USING gin(...)`) — the PostgreSQL access-method borrow that
//! accelerates `tsvector @@ tsquery` from full scans to postings
//! lookups.
//!
//! Coverage strategy: every indexed query is also run against an
//! UNINDEXED twin table (same data) and the row sets must match
//! exactly — the inverted index may only narrow candidates, never
//! change semantics (the `@@` conjunct stays in the residual).

use rustqlite::{Database, Value};

fn db() -> Database {
    Database::open_in_memory().unwrap()
}

fn ids(db: &Database, sql: &str) -> Vec<i64> {
    let rows = db.query(sql, []).unwrap();
    rows.iter()
        .map(|r| match r.first() {
            Some(Value::Integer(i)) => *i,
            v => panic!("expected INTEGER id, got {v:?}"),
        })
        .collect()
}

fn ids_sorted(db: &Database, sql: &str) -> Vec<i64> {
    let mut v = ids(db, sql);
    v.sort_unstable();
    v
}

/// Fixture: docs with body text; twin tables — one gin-indexed, one not.
fn doc_fixture() -> (Database, Database) {
    let mut indexed = db();
    indexed
        .execute("CREATE TABLE docs(id INTEGER PRIMARY KEY, body TEXT)", [])
        .unwrap();
    indexed
        .execute(
            "INSERT INTO docs VALUES
                (1, 'the quick brown fox jumps over the lazy dog'),
                (2, 'postgresql is a powerful open source database'),
                (3, 'mysql is also an open source database'),
                (4, 'the quick fox returns to the forest'),
                (5, 'json and jsonb are postgres native types'),
                (6, NULL)",
            [],
        )
        .unwrap();
    indexed
        .execute(
            "CREATE INDEX docs_fts ON docs USING gin(to_tsvector('english', body))",
            [],
        )
        .unwrap();

    let mut plain = db();
    plain
        .execute("CREATE TABLE docs(id INTEGER PRIMARY KEY, body TEXT)", [])
        .unwrap();
    plain
        .execute(
            "INSERT INTO docs VALUES
                (1, 'the quick brown fox jumps over the lazy dog'),
                (2, 'postgresql is a powerful open source database'),
                (3, 'mysql is also an open source database'),
                (4, 'the quick fox returns to the forest'),
                (5, 'json and jsonb are postgres native types'),
                (6, NULL)",
            [],
        )
        .unwrap();
    (indexed, plain)
}

/// One query, both tables, identical expected rows.
fn assert_parity(indexed: &Database, plain: &Database, sql: &str, expected: &[i64]) {
    let a = ids_sorted(indexed, sql);
    let b = ids_sorted(plain, sql);
    assert_eq!(
        a, b,
        "indexed and unindexed disagree on `{sql}`: {a:?} vs {b:?}"
    );
    assert_eq!(a, expected.to_vec(), "wrong rows for `{sql}`: {a:?}");
}

#[test]
fn gin_single_term_matches_full_scan() {
    let (indexed, plain) = doc_fixture();
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'database')",
        &[2, 3],
    );
}

#[test]
fn gin_and_intersection() {
    let (indexed, plain) = doc_fixture();
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'database & open')",
        &[2, 3],
    );
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'database & mysql')",
        &[3],
    );
    // A narrower AND: the postings intersection should be exact.
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'quick & fox')",
        &[1, 4],
    );
}

#[test]
fn gin_or_union() {
    let (indexed, plain) = doc_fixture();
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'postgresql | mysql')",
        &[2, 3],
    );
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'quick | json')",
        &[1, 4, 5],
    );
}

#[test]
fn gin_prefix_terms() {
    let (indexed, plain) = doc_fixture();
    // postg:* matches postgres AND postgresql (both stem to 'postg').
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'postg:*')",
        &[2, 5],
    );
    // dat:* matches database.
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'dat:*')",
        &[2, 3],
    );
    // Prefix AND exact.
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'postg:* & open')",
        &[2],
    );
}

#[test]
fn gin_not_conjunction_and_fallback() {
    let (indexed, plain) = doc_fixture();
    // a & !b: candidates = postings(a), residual refines — sound.
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'open & !mysql')",
        &[2],
    );
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'database & !mysql')",
        &[2],
    );
    // Pure negation / unsatisfiable-positive shapes fall back to a full
    // scan (the `All` propagation) — still exact. NULL-body rows never
    // match (`@@` is NULL).
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', '!database')",
        &[1, 4, 5],
    );
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', '!database | mysql')",
        &[1, 3, 4, 5],
    );
}

#[test]
fn gin_phrase_queries() {
    let (indexed, plain) = doc_fixture();
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'open <-> source')",
        &[2, 3],
    );
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'source <-> open')",
        &[],
    );
}

#[test]
fn gin_plainto_websearch_and_ts_match_forms() {
    let (indexed, plain) = doc_fixture();
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ plainto_tsquery('english', 'open source database')",
        &[2, 3],
    );
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ websearch_to_tsquery('english', 'postgresql OR mysql')",
        &[2, 3],
    );
    // Function form of @@.
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE ts_match(to_tsvector('english', body), to_tsquery('english', 'quick'))",
        &[1, 4],
    );
}

#[test]
fn gin_null_rows_and_null_query() {
    let (indexed, plain) = doc_fixture();
    // Row 6 has NULL body: @@ is NULL — never matches, on either plan.
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'quick')",
        &[1, 4],
    );
    // NULL query: no rows match.
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ NULL",
        &[],
    );
}

#[test]
fn gin_parameterized_query() {
    let (indexed, _) = doc_fixture();
    let out = indexed
        .query(
            "SELECT id FROM docs WHERE to_tsvector('english', body) @@ ? ORDER BY id",
            [Value::Text("'quick'".into())],
        )
        .unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].first(), Some(&Value::Integer(1)));
    assert_eq!(out[1].first(), Some(&Value::Integer(4)));

    // NULL parameter.
    let out = indexed
        .query(
            "SELECT id FROM docs WHERE to_tsvector('english', body) @@ ?",
            [Value::Null],
        )
        .unwrap();
    assert!(out.is_empty());
}

#[test]
fn gin_combined_with_other_predicates() {
    let (indexed, plain) = doc_fixture();
    // The @@ drives the index; id < 5 rides the residual.
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'database') AND id < 3",
        &[2],
    );
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'open') AND id % 2 = 0",
        &[2],
    );
}

#[test]
fn gin_write_maintenance_update_delete_insert() {
    let (mut indexed, mut plain) = doc_fixture();

    // UPDATE: move a term from doc 2 to a fresh wording.
    indexed
        .execute(
            "UPDATE docs SET body = 'sqlite is a lightweight database' WHERE id = 2",
            [],
        )
        .unwrap();
    plain
        .execute(
            "UPDATE docs SET body = 'sqlite is a lightweight database' WHERE id = 2",
            [],
        )
        .unwrap();
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'sqlite')",
        &[2],
    );
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'postgresql')",
        &[],
    );
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'database')",
        &[2, 3],
    );

    // DELETE removes postings.
    indexed
        .execute("DELETE FROM docs WHERE id = 3", [])
        .unwrap();
    plain.execute("DELETE FROM docs WHERE id = 3", []).unwrap();
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'database')",
        &[2],
    );

    // INSERT adds postings (including into terms that existed).
    indexed
        .execute("INSERT INTO docs VALUES (7, 'another quick fox story')", [])
        .unwrap();
    plain
        .execute("INSERT INTO docs VALUES (7, 'another quick fox story')", [])
        .unwrap();
    assert_parity(
        &indexed,
        &plain,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'quick & fox')",
        &[1, 4, 7],
    );
}

#[test]
fn gin_backfill_over_existing_rows() {
    // CREATE INDEX after the data exists: every row must be visible to
    // the inverted scan (the backfill recomputes per-lexeme entries).
    let mut d = db();
    d.execute("CREATE TABLE docs(id INTEGER PRIMARY KEY, body TEXT)", [])
        .unwrap();
    d.execute(
        "INSERT INTO docs VALUES (1,'alpha beta'), (2,'beta gamma'), (3,'delta')",
        [],
    )
    .unwrap();
    // Before the index: full scan matches.
    let before = ids_sorted(
        &d,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'beta')",
    );
    assert_eq!(before, vec![1, 2]);
    d.execute(
        "CREATE INDEX late_fts ON docs USING gin(to_tsvector('english', body))",
        [],
    )
    .unwrap();
    let after = ids_sorted(
        &d,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'beta')",
    );
    assert_eq!(after, vec![1, 2]);
    let after2 = ids_sorted(
        &d,
        "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'delta')",
    );
    assert_eq!(after2, vec![3]);
}

#[test]
fn gin_stored_tsvector_column_form() {
    // The generated-column pattern from the fts docs, with a gin index
    // over the STORED tsvector column.
    let mut d = db();
    d.execute(
        "CREATE TABLE docs(id INTEGER PRIMARY KEY, body TEXT, tsv TEXT GENERATED ALWAYS AS (to_tsvector('english', body)))",
        [],
    )
    .unwrap();
    d.execute(
        "INSERT INTO docs(id, body) VALUES (1,'hello world'), (2,'goodbye world'), (3,'hello again')",
        [],
    )
    .unwrap();
    d.execute("CREATE INDEX docs_tsv ON docs USING gin(tsv)", [])
        .unwrap();

    assert_eq!(
        ids_sorted(
            &d,
            "SELECT id FROM docs WHERE tsv @@ to_tsquery('english', 'hello')"
        ),
        vec![1, 3]
    );
    assert_eq!(
        ids_sorted(
            &d,
            "SELECT id FROM docs WHERE tsv @@ to_tsquery('english', 'hello & world')"
        ),
        vec![1]
    );
    assert_eq!(
        ids_sorted(
            &d,
            "SELECT id FROM docs WHERE tsv @@ to_tsquery('english', 'hello | goodbye')"
        ),
        vec![1, 2, 3]
    );
    // Ranked output over the inverted scan: doc 3 ('hello' only, 1
    // position) length-normalizes ABOVE doc 1 ('hello world', 2
    // positions) — the shorter, fully-matching document wins.
    let ranked = ids(&d,
        "SELECT id FROM docs WHERE tsv @@ to_tsquery('english', 'hello') ORDER BY ts_rank(tsv, to_tsquery('english', 'hello')) DESC, id");
    assert_eq!(ranked, vec![3, 1]);
}

#[test]
fn gin_persistence_round_trip() {
    let path = std::env::temp_dir().join("rustqlite_gin_test.db");
    let _ = std::fs::remove_file(&path);
    {
        let mut d = Database::open(&path).unwrap();
        d.execute("CREATE TABLE docs(id INTEGER PRIMARY KEY, body TEXT)", [])
            .unwrap();
        d.execute(
            "INSERT INTO docs VALUES (1,'alpha beta'), (2,'gamma delta')",
            [],
        )
        .unwrap();
        d.execute(
            "CREATE INDEX di ON docs USING gin(to_tsvector('english', body))",
            [],
        )
        .unwrap();
    }
    {
        let d = Database::open(&path).unwrap();
        assert_eq!(
            ids_sorted(&d, "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'alpha')"),
            vec![1]
        );
        // Writes maintain the reopened index.
        let mut d2 = Database::open(&path).unwrap();
        d2.execute("INSERT INTO docs VALUES (3, 'alpha epsilon')", [])
            .unwrap();
        assert_eq!(
            ids_sorted(&d2, "SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'alpha')"),
            vec![1, 3]
        );
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn gin_explain_shows_inverted_scan() {
    let (d, _) = doc_fixture();
    let rows = d
        .query(
            "EXPLAIN SELECT id FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'quick')",
            [],
        )
        .unwrap();
    let plan = format!("{:?}", rows);
    assert!(
        plan.contains("INVERTED"),
        "EXPLAIN must advertise the inverted scan, got: {plan}"
    );
    assert!(
        plan.contains("docs_fts"),
        "EXPLAIN must name the gin index, got: {plan}"
    );
}

#[test]
fn gin_validation_and_errors() {
    let mut d = db();
    d.execute("CREATE TABLE t(a TEXT, b TEXT)", []).unwrap();
    // UNIQUE is meaningless on an inverted index.
    assert!(d
        .execute("CREATE UNIQUE INDEX ix ON t USING gin(to_tsvector(a))", [])
        .is_err());
    // Non-tsvector expressions rejected.
    assert!(d
        .execute("CREATE INDEX ix ON t USING gin(lower(a))", [])
        .is_err());
    assert!(d
        .execute("CREATE INDEX ix ON t USING gin(a, b)", [])
        .is_err());
    // Unknown access method is a parse error.
    assert!(d.execute("CREATE INDEX ix ON t USING brin(a)", []).is_err());
    // Plain columns holding garbage fail the write (PG's GIN strictness).
    d.execute("CREATE INDEX ix ON t USING gin(a)", []).unwrap();
    let err = d.execute("INSERT INTO t VALUES ('not a tsvector', NULL)", []);
    assert!(err.is_err(), "garbage tsvector must fail the insert");
    d.execute("INSERT INTO t VALUES (NULL, NULL)", []).unwrap();
    // NULLs are exempt: no entries, no matches.
    assert_eq!(
        ids_sorted(
            &d,
            "SELECT rowid FROM t WHERE a @@ to_tsquery('english', 'x')"
        ),
        Vec::<i64>::new()
    );
}

#[test]
fn gin_no_hijack_of_unrelated_queries() {
    // An eq predicate on a plain btree index still wins when no @@ is
    // present; the gin index must not intercept anything.
    let (d, plain) = doc_fixture();
    assert_parity(&d, &plain, "SELECT id FROM docs WHERE id >= 4", &[4, 5, 6]);
    assert_parity(&d, &plain, "SELECT id FROM docs WHERE body IS NULL", &[6]);
}
