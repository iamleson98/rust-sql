//! JSON1 function coverage: parsing, path resolution, extraction, and the
//! mutation/constructor functions.
use rustqlite::{Database, Value};

fn q(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql, []).unwrap()
}

#[test]
fn json_extract_scalars_and_nesting() {
    let db = Database::open_in_memory().unwrap();
    assert_eq!(
        q(&db, "SELECT json_extract('{\"a\": 5}', '$.a')"),
        vec![vec![Value::Integer(5)]]
    );
    assert_eq!(
        q(&db, "SELECT json_extract('{\"a\": {\"b\": [1, 2, 3]}}', '$.a.b[2]')"),
        vec![vec![Value::Integer(3)]]
    );
    // Strings come back unquoted.
    assert_eq!(
        q(&db, "SELECT json_extract('{\"a\": \"hi\"}', '$.a')"),
        vec![vec![Value::Text("hi".into())]]
    );
    // Negative index from the end.
    assert_eq!(
        q(&db, "SELECT json_extract('[10, 20, 30]', '$[#-1]')"),
        vec![vec![Value::Integer(30)]]
    );
    // Top-level array index.
    assert_eq!(
        q(&db, "SELECT json_extract('[10, 20]', '$[1]')"),
        vec![vec![Value::Integer(20)]]
    );
    // Missing path → NULL.
    assert_eq!(
        q(&db, "SELECT json_extract('{\"a\": 5}', '$.missing')"),
        vec![vec![Value::Null]]
    );
    // Whole-document path.
    assert_eq!(
        q(&db, "SELECT json_extract('{\"a\": 5}', '$')"),
        vec![vec![Value::Text("{\"a\":5}".into())]]
    );
}

#[test]
fn json_valid_and_type() {
    let db = Database::open_in_memory().unwrap();
    assert_eq!(q(&db, "SELECT json_valid('{\"a\": 1}')"), vec![vec![Value::Integer(1)]]);
    assert_eq!(q(&db, "SELECT json_valid('not json')"), vec![vec![Value::Integer(0)]]);
    assert_eq!(q(&db, "SELECT json_valid('  [1, 2]  ')"), vec![vec![Value::Integer(1)]]);
    // Trailing garbage is invalid.
    assert_eq!(q(&db, "SELECT json_valid('{} x')"), vec![vec![Value::Integer(0)]]);
    assert_eq!(
        q(&db, "SELECT json_type('{\"a\": 1}')"),
        vec![vec![Value::Text("object".into())]]
    );
    assert_eq!(
        q(&db, "SELECT json_type('[1]', '$[0]')"),
        vec![vec![Value::Text("integer".into())]]
    );
    assert_eq!(
        q(&db, "SELECT json_type('{\"a\": null}', '$.a')"),
        vec![vec![Value::Text("null".into())]]
    );
}

#[test]
fn json_constructors() {
    let db = Database::open_in_memory().unwrap();
    assert_eq!(
        q(&db, "SELECT json_array(1, 'two', 3.5, NULL)"),
        vec![vec![Value::Text("[1,\"two\",3.5,null]".into())]]
    );
    assert_eq!(
        q(&db, "SELECT json_object('k', 'v', 'n', 7)"),
        vec![vec![Value::Text("{\"k\":\"v\",\"n\":7}".into())]]
    );
    assert_eq!(
        q(&db, "SELECT json_quote('say \"hi\"')"),
        vec![vec![Value::Text("\"say \\\"hi\\\"\"".into())]]
    );
    // json() minifies.
    assert_eq!(
        q(&db, "SELECT json('{\"b\": 1,  \"a\": 2 }')"),
        vec![vec![Value::Text("{\"b\":1,\"a\":2}".into())]]
    );
    assert_eq!(
        q(&db, "SELECT json_array_length('[1, 2, 3]')"),
        vec![vec![Value::Integer(3)]]
    );
}

#[test]
fn json_mutators() {
    let db = Database::open_in_memory().unwrap();
    assert_eq!(
        q(&db, "SELECT json_set('{\"a\": 1}', '$.b', 2)"),
        vec![vec![Value::Text("{\"a\":1,\"b\":2}".into())]]
    );
    // insert only fills absent paths.
    assert_eq!(
        q(&db, "SELECT json_insert('{\"a\": 1}', '$.a', 9, '$.b', 2)"),
        vec![vec![Value::Text("{\"a\":1,\"b\":2}".into())]]
    );
    // replace only touches existing paths.
    assert_eq!(
        q(&db, "SELECT json_replace('{\"a\": 1}', '$.a', 9, '$.b', 2)"),
        vec![vec![Value::Text("{\"a\":9}".into())]]
    );
    assert_eq!(
        q(&db, "SELECT json_remove('{\"a\": 1, \"b\": 2}', '$.a')"),
        vec![vec![Value::Text("{\"b\":2}".into())]]
    );
    // RFC 7396 merge patch: null deletes, new keys add.
    assert_eq!(
        q(&db, "SELECT json_patch('{\"a\": 1, \"b\": 2}', '{\"b\": null, \"c\": 3}')"),
        vec![vec![Value::Text("{\"a\":1,\"c\":3}".into())]]
    );
}

#[test]
fn json_extract_over_table_data() {
    // The OLTP shape: extract from a JSON column across rows.
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE events (id INTEGER PRIMARY KEY, payload TEXT)", []).unwrap();
    db.execute(
        "INSERT INTO events (payload) VALUES ('{\"user\": \"ann\", \"n\": 3}'), ('{\"user\": \"bob\", \"n\": 5}')",
        [],
    )
    .unwrap();
    assert_eq!(
        q(&db, "SELECT json_extract(payload, '$.user') FROM events ORDER BY id"),
        vec![
            vec![Value::Text("ann".into())],
            vec![Value::Text("bob".into())],
        ]
    );
    let rows = q(&db, "SELECT SUM(json_extract(payload, '$.n')) FROM events");
    assert_eq!(rows[0][0], Value::Integer(8));
    // WHERE on an extracted field.
    assert_eq!(
        q(&db, "SELECT id FROM events WHERE json_extract(payload, '$.n') > 4"),
        vec![vec![Value::Integer(2)]]
    );
}

#[test]
fn json_unicode_and_escapes() {
    let db = Database::open_in_memory().unwrap();
    // \u escapes round-trip.
    assert_eq!(
        q(&db, "SELECT json_extract('{\"k\": \"caf\\u00e9\"}', '$.k')"),
        vec![vec![Value::Text("café".into())]]
    );
    // Emoji (surrogate pair).
    assert_eq!(
        q(&db, "SELECT json_extract('{\"k\": \"\\ud83d\\ude00\"}', '$.k')"),
        vec![vec![Value::Text("😀".into())]]
    );
    // Embedded quotes re-escape on serialize.
    assert_eq!(
        q(&db, "SELECT json_object('k', 'a\"b')"),
        vec![vec![Value::Text("{\"k\":\"a\\\"b\"}".into())]]
    );
}
