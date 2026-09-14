//! End-to-end full-text search (PostgreSQL-style tsvector/tsquery).
//!
//! Covers the SQL surface of `src/executor/fts.rs`: the `to_tsvector` /
//! `to_tsquery` / `plainto_tsquery` / `phraseto_tsquery` /
//! `websearch_to_tsquery` function family, the `@@` match operator in
//! WHERE clauses, `ts_rank` ordering, `ts_headline`, tsvector `||`
//! concatenation via `tsvector_concat`, and the generated-column
//! indexing pattern recommended in the module docs.

use rustqlite::{Database, Value};

fn db() -> Database {
    Database::open_in_memory().unwrap()
}

fn one(db: &Database, sql: &str) -> Value {
    let rows = db.query(sql, []).unwrap();
    rows.first()
        .and_then(|r| r.first().cloned())
        .unwrap_or(Value::Null)
}

fn text(db: &Database, sql: &str) -> String {
    match one(db, sql) {
        Value::Text(s) => s.to_string(),
        v => panic!("expected TEXT from `{sql}`, got {v:?}"),
    }
}

fn int(db: &Database, sql: &str) -> i64 {
    match one(db, sql) {
        Value::Integer(i) => i,
        v => panic!("expected INTEGER from `{sql}`, got {v:?}"),
    }
}

fn real(db: &Database, sql: &str) -> f64 {
    match one(db, sql) {
        Value::Real(f) => f,
        v => panic!("expected REAL from `{sql}`, got {v:?}"),
    }
}

#[test]
fn to_tsvector_canonical_form_and_config() {
    let d = db();
    // positions count stop words; stems applied; lexemes sorted.
    assert_eq!(
        text(
            &d,
            "SELECT to_tsvector('english', 'The quick brown foxes jumped over the lazy dogs')"
        ),
        "'brown':3 'dog':9 'fox':4 'jump':5 'lazi':8 'quick':2"
    );
    // simple: lowercase only, stop words kept
    assert_eq!(
        text(&d, "SELECT to_tsvector('simple', 'The Cats')"),
        "'cats':2 'the':1"
    );
    // one-arg form defaults to english
    assert_eq!(text(&d, "SELECT to_tsvector('Cats')"), "'cat':1");
    // pg_catalog-qualified config name
    assert_eq!(
        text(&d, "SELECT to_tsvector('pg_catalog.simple', 'The')"),
        "'the':1"
    );
    // NULL in, NULL out
    assert_eq!(one(&d, "SELECT to_tsvector(NULL)"), Value::Null);
    // unknown config errors
    assert!(d.query("SELECT to_tsvector('german', 'x')", []).is_err());
}

#[test]
fn to_tsquery_forms() {
    let d = db();
    assert_eq!(
        text(&d, "SELECT to_tsquery('english', 'cat & dog')"),
        "'cat' & 'dog'"
    );
    assert_eq!(
        text(&d, "SELECT to_tsquery('english', 'cat | dog')"),
        "'cat' | 'dog'"
    );
    assert_eq!(text(&d, "SELECT to_tsquery('english', '!cat')"), "!'cat'");
    assert_eq!(text(&d, "SELECT to_tsquery('english', 'run:*')"), "'run':*");
    assert_eq!(
        text(&d, "SELECT to_tsquery('english', 'cat <-> dog')"),
        "'cat' <-> 'dog'"
    );
    // stop word dropped from a mixed query
    assert_eq!(
        text(&d, "SELECT to_tsquery('english', 'cat & the')"),
        "'cat'"
    );
    // only-stop-word query errors like PostgreSQL
    assert!(d
        .query("SELECT to_tsquery('english', 'the & was')", [])
        .is_err());
    // precedence: & binds tighter than |
    assert_eq!(
        text(&d, "SELECT to_tsquery('english', 'cat & dog | pig' )"),
        "'cat' & 'dog' | 'pig'"
    );
    // parentheses change the shape
    assert_eq!(
        text(&d, "SELECT to_tsquery('english', 'cat & (dog | pig)')"),
        "'cat' & ('dog' | 'pig')"
    );
}

#[test]
fn plainto_phraseto_websearch() {
    let d = db();
    assert_eq!(
        text(&d, "SELECT plainto_tsquery('english', 'The fat cats')"),
        "'fat' & 'cat'"
    );
    assert_eq!(
        text(&d, "SELECT phraseto_tsquery('english', 'The fat cats')"),
        "'fat' <-> 'cat'"
    );
    assert_eq!(
        text(
            &d,
            "SELECT websearch_to_tsquery('english', 'fat cats mouse')"
        ),
        "'fat' & 'cat' & 'mouse'"
    );
    assert_eq!(
        text(&d, "SELECT websearch_to_tsquery('english', 'fat OR cats')"),
        "'fat' | 'cat'"
    );
    assert_eq!(
        text(
            &d,
            "SELECT websearch_to_tsquery('english', '\"fat cats\" -mouse')"
        ),
        "'fat' <-> 'cat' & !'mouse'"
    );
    // trailing OR with no operand is dropped
    assert_eq!(
        text(&d, "SELECT websearch_to_tsquery('english', 'trailing OR')"),
        "'trail'"
    );
}

#[test]
fn match_operator_in_where() {
    let mut d = db();
    d.execute(
        "CREATE TABLE docs(id INTEGER PRIMARY KEY, body TEXT);
         INSERT INTO docs VALUES
           (1, 'PostgreSQL is a powerful open source database engine'),
           (2, 'MySQL is also an open source database'),
           (3, 'MongoDB stores documents as JSON')",
        [],
    )
    .unwrap();
    // raw @@ with hand-written stored forms
    let n = int(
        &d,
        "SELECT COUNT(*) FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'database')",
    );
    assert_eq!(n, 2);

    let n = int(
        &d,
        "SELECT COUNT(*) FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'database & open')",
    );
    assert_eq!(n, 2);

    let n = int(
        &d,
        "SELECT COUNT(*) FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'database & mysql')",
    );
    assert_eq!(n, 1);

    let n = int(
        &d,
        "SELECT COUNT(*) FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'json')",
    );
    assert_eq!(n, 1);

    let n = int(
        &d,
        "SELECT COUNT(*) FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', '!database')",
    );
    assert_eq!(n, 1);

    // prefix match: 'postg:*'
    let n = int(
        &d,
        "SELECT COUNT(*) FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'postg:*')",
    );
    assert_eq!(n, 1);

    // phrase: 'open <-> source' (our stemmer keeps 'source' whole —
    // documented divergence from Snowball's 'sourc')
    let n = int(
        &d,
        "SELECT COUNT(*) FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'open <-> source')",
    );
    assert_eq!(n, 2);
    // reversed order phrase must not match
    let n = int(
        &d,
        "SELECT COUNT(*) FROM docs WHERE to_tsvector('english', body) @@ to_tsquery('english', 'source <-> open')",
    );
    assert_eq!(n, 0);
}

#[test]
fn generated_column_pattern() {
    // The indexing pattern recommended in fts.rs docs: a GENERATED ALWAYS
    // column keeps the tsvector in sync, and queries hit it via @@.
    let mut d = db();
    d.execute(
        "CREATE TABLE docs(id INTEGER PRIMARY KEY, body TEXT,
            tsv TEXT GENERATED ALWAYS AS (to_tsvector('english', body)))",
        [],
    )
    .unwrap();
    d.execute(
        "INSERT INTO docs(id, body) VALUES (1, 'quick brown fox'), (2, 'lazy dog')",
        [],
    )
    .unwrap();
    let rows = d
        .query(
            "SELECT id FROM docs WHERE tsv @@ to_tsquery('english', 'fox') ORDER BY id",
            [],
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Integer(1));

    // the stored generated value is the canonical tsvector text
    assert_eq!(
        text(&d, "SELECT tsv FROM docs WHERE id = 1"),
        "'brown':2 'fox':3 'quick':1"
    );
    // and it updates when the source column changes
    d.execute("UPDATE docs SET body = 'sleepy cat' WHERE id = 1", [])
        .unwrap();
    assert_eq!(
        text(&d, "SELECT tsv FROM docs WHERE id = 1"),
        "'cat':2 'sleepi':1"
    );
}

#[test]
fn ts_rank_orders_documents() {
    let mut d = db();
    d.execute(
        "CREATE TABLE docs(id INTEGER PRIMARY KEY, body TEXT);
         INSERT INTO docs VALUES
           (1, 'postgres appears once'),
           (2, 'postgres postgres postgres everywhere')",
        [],
    )
    .unwrap();
    // more occurrences of the term => higher rank
    let ranked = d
        .query(
            "SELECT id FROM docs
             WHERE to_tsvector('simple', body) @@ 'postgres'
             ORDER BY ts_rank(to_tsvector('simple', body), 'postgres') DESC",
            [],
        )
        .unwrap();
    assert_eq!(ranked[0][0], Value::Integer(2));
    assert_eq!(ranked[1][0], Value::Integer(1));
    // rank values are positive REALs
    assert!(
        real(
            &d,
            "SELECT ts_rank(to_tsvector('simple', 'postgres postgres'), 'postgres')"
        ) > 0.0
    );
    // ts_rank_cd is accepted as an alias
    assert!(
        real(
            &d,
            "SELECT ts_rank_cd(to_tsvector('simple', 'postgres postgres'), 'postgres')"
        ) > 0.0
    );
    // 3-arg with weights array literal
    assert!(
        real(
            &d,
            "SELECT ts_rank('{0.1,0.2,0.4,1.0}', to_tsvector('simple', 'postgres'), 'postgres')"
        ) > 0.0
    );
    // NULL tsvector ranks NULL
    assert_eq!(one(&d, "SELECT ts_rank(NULL, 'postgres')"), Value::Null);
}

#[test]
fn headline_and_utilities() {
    let d = db();
    assert_eq!(
        text(
            &d,
            "SELECT ts_headline('english', 'The quick brown fox', 'quick & fox')"
        ),
        "The <b>quick</b> brown <b>fox</b>"
    );
    // strip() drops positions
    assert_eq!(
        text(&d, "SELECT strip(to_tsvector('english', 'cats and dogs'))"),
        "'cat' 'dog'"
    );
    // numnode counts query nodes
    assert_eq!(
        int(&d, "SELECT numnode(to_tsquery('simple', 'a & b | !c'))"),
        6
    );
    // tsvector_concat = Postgres || for tsvectors ('simple' config: no
    // stemming, so 'apples' keeps its plural)
    assert_eq!(
        text(
            &d,
            "SELECT tsvector_concat(to_tsvector('simple', 'red apples'), to_tsvector('simple', 'green apples'))"
        ),
        "'apples':2 'green':1 'red':1"
    );
}

#[test]
fn match_operator_null_and_errors() {
    let d = db();
    // NULL operands propagate to NULL
    assert_eq!(one(&d, "SELECT NULL @@ 'cat'"), Value::Null);
    assert_eq!(
        one(&d, "SELECT to_tsvector('simple', 'cat') @@ NULL"),
        Value::Null
    );
    // malformed stored tsvector raises
    assert!(d.query("SELECT 'garbage' @@ 'cat'", []).is_err());
    // malformed stored tsquery raises
    assert!(d
        .query("SELECT to_tsvector('simple', 'cat') @@ '&&'", [])
        .is_err());
}

#[test]
fn fts_index_scan_still_correct() {
    // A real index on the tsvector column must not change results
    // (regression lock: @@ is not pushed into a compiled fallback that
    // silently yields NULL — see apply_binary's FtsMatch arm).
    let mut d = db();
    d.execute(
        "CREATE TABLE docs(id INTEGER PRIMARY KEY, body TEXT,
            tsv TEXT GENERATED ALWAYS AS (to_tsvector('english', body)));
         CREATE INDEX docs_tsv ON docs(tsv);
         INSERT INTO docs(id, body) VALUES
           (1, 'quick brown fox'), (2, 'lazy dog'), (3, 'quick quick dog')",
        [],
    )
    .unwrap();
    let n = int(&d, "SELECT COUNT(*) FROM docs WHERE tsv @@ 'quick'");
    assert_eq!(n, 2);
    // stored-form semantics: the generated tsv holds the STEM 'lazi'
    // (lazy → lazi), so the stored-form query must use the stem too —
    // exactly like PostgreSQL, where tsv @@ 'lazy' does not match a
    // stemmed lexeme 'lazi' but tsv @@ to_tsquery('lazy') does.
    let n = int(
        &d,
        "SELECT COUNT(*) FROM docs WHERE tsv @@ 'dog' AND NOT tsv @@ 'lazi'",
    );
    assert_eq!(n, 1);
    // ...and dictionary-processed queries match through stemming:
    let n = int(
        &d,
        "SELECT COUNT(*) FROM docs WHERE tsv @@ to_tsquery('english', 'lazy')",
    );
    assert_eq!(n, 1);
}
