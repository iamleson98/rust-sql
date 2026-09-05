//! Integration tests for the native sqlx driver (`sqlx` feature).
//!
//! These exercise the driver through the REAL `sqlx` facade crate (dev-dep)
//! — the exact API surface a user would use — proving facade/driver type
//! identity, plus the engine-level semantics (transactions, constraint
//! error mapping, concurrency) that the C-ABI interop suite covers.
//!
//! Run: `cargo test --features sqlx --test sqlx_driver`

#![cfg(feature = "sqlx")]

use rustqlite::sqlx_driver::{raw_sql, RustqliteConnectOptions, RustqlitePool};
use rustqlite::sqlx_driver::{SqlStr, Statement as _Statement};
use std::sync::atomic::{AtomicU64, Ordering};

use sqlx::{Connection, Executor, Row};

static POOL_ID: AtomicU64 = AtomicU64::new(0);

/// Each test gets its own NAMED shared in-memory database so parallel tests
/// never fight over one engine (SQLite `file:NAME?mode=memory&cache=shared`
/// semantics).
async fn mem_pool() -> RustqlitePool {
    let id = POOL_ID.fetch_add(1, Ordering::Relaxed);
    RustqlitePool::connect_with(RustqliteConnectOptions::shared_memory(format!("test-{id}")))
        .await
        .unwrap()
}

#[derive(Debug, sqlx::FromRow, PartialEq)]
struct User {
    id: i64,
    name: String,
    score: Option<f64>,
}

#[tokio::test]
async fn connect_and_ddl() {
    let pool = mem_pool().await;
    // DDL via execute (raw SQL, no binds)
    sqlx::query("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();

    // ...and via raw_sql()
    raw_sql("CREATE TABLE t2 (a INT, b REAL); CREATE INDEX t2_a ON t2 (a);")
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn typed_binds_and_fetch() {
    let pool = mem_pool().await;
    sqlx::query("CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL)")
        .execute(&pool)
        .await
        .unwrap();

    let result = sqlx::query("INSERT INTO u (name, score) VALUES (?, ?)")
        .bind("Ada")
        .bind(1.5f64)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(result.rows_affected(), 1);
    assert_eq!(result.last_insert_rowid(), 1);

    // i64 / &str / Vec<u8> / bool / Option<T> / f32 binds
    sqlx::query("INSERT INTO u (name, score) VALUES (?, ?)")
        .bind("Bob")
        .bind(None::<f64>)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE u SET score = ? WHERE name = ?")
        .bind(2)
        .bind("Bob")
        .execute(&pool)
        .await
        .unwrap();

    // fetch_all + query_as derive
    let rows: Vec<User> = sqlx::query_as("SELECT id, name, score FROM u ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(
        rows,
        vec![
            User {
                id: 1,
                name: "Ada".into(),
                score: Some(1.5)
            },
            User {
                id: 2,
                name: "Bob".into(),
                score: Some(2.0)
            },
        ]
    );

    // fetch_one / fetch_optional / query_scalar
    let name: String = sqlx::query_scalar("SELECT name FROM u WHERE id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(name, "Ada");

    let missing: Option<String> = sqlx::query_scalar("SELECT name FROM u WHERE id = 99")
        .fetch_optional(&pool)
        .await
        .unwrap()
        .flatten();
    assert_eq!(missing, None);

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM u")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn returning_and_row_api() {
    let pool = mem_pool().await;
    sqlx::query("CREATE TABLE r (id INTEGER PRIMARY KEY, v TEXT)")
        .execute(&pool)
        .await
        .unwrap();

    // INSERT ... RETURNING via fetch_one
    let id: i64 = sqlx::query_scalar("INSERT INTO r (v) VALUES (?) RETURNING id")
        .bind("x")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(id, 1);

    // Row API by name and by ordinal
    let row = sqlx::query("SELECT id, v FROM r WHERE id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let v: &str = row.try_get("v").unwrap();
    let id2: i64 = row.try_get(0).unwrap();
    assert_eq!((v, id2), ("x", 1));

    // Debug formatting of rows (TypeChecking path)
    let dbg = format!("{:?}", row);
    assert!(dbg.contains("id"), "row Debug: {dbg}");

    // NULL round-trip
    sqlx::query("INSERT INTO r (v) VALUES (NULL)")
        .execute(&pool)
        .await
        .unwrap();
    let null_v: Option<String> = sqlx::query_scalar("SELECT v FROM r WHERE id = 2")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(null_v, None);
}

#[tokio::test]
async fn blob_bool_and_types() {
    let pool = mem_pool().await;
    sqlx::query("CREATE TABLE b (id INTEGER PRIMARY KEY, data BLOB, flag BOOLEAN)")
        .execute(&pool)
        .await
        .unwrap();

    sqlx::query("INSERT INTO b (data, flag) VALUES (?, ?)")
        .bind(vec![1u8, 2, 3, 4])
        .bind(true)
        .execute(&pool)
        .await
        .unwrap();

    let (data, flag): (Vec<u8>, bool) = sqlx::query_as("SELECT data, flag FROM b WHERE id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(data, vec![1, 2, 3, 4]);
    assert!(flag);

    // i32 / u32 decode with narrowing check
    let n: i32 = sqlx::query_scalar("SELECT 42")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 42);

    // text with unicode
    let s: String = sqlx::query_scalar("SELECT 'héllo wörld ✓'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(s, "héllo wörld ✓");
}

#[tokio::test]
async fn transactions_commit_and_rollback() {
    let pool = mem_pool().await;
    sqlx::query("CREATE TABLE tx (id INTEGER PRIMARY KEY, v TEXT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();

    // commit path
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO tx (v) VALUES ('kept')")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // rollback path
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO tx (v) VALUES ('lost')")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tx")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);

    // drop without commit = rollback
    {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("INSERT INTO tx (v) VALUES ('also-lost')")
            .execute(&mut *tx)
            .await
            .unwrap();
        // dropped here
    }
    // force the pending rollback through the next statement
    let _count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tx")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(_count, 1);

    // nested transactions (savepoints)
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO tx (v) VALUES ('outer')")
        .execute(&mut *tx)
        .await
        .unwrap();
    {
        let mut inner = tx.begin().await.unwrap();
        sqlx::query("INSERT INTO tx (v) VALUES ('inner')")
            .execute(&mut *inner)
            .await
            .unwrap();
        inner.rollback().await.unwrap();
    }
    tx.commit().await.unwrap();

    let vs: Vec<String> = sqlx::query_scalar("SELECT v FROM tx ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(vs, vec!["kept".to_string(), "outer".to_string()]);
}

#[tokio::test]
async fn connection_level_api() {
    let opts = RustqliteConnectOptions::new();
    let mut conn = rustqlite::sqlx_driver::RustqliteConnection::open(&opts).unwrap();
    sqlx::query("CREATE TABLE c (v TEXT)")
        .execute(&mut conn)
        .await
        .unwrap();
    conn.ping().await.unwrap();

    let v: String = sqlx::query_scalar("SELECT 'ping'")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(v, "ping");
    conn.close().await.unwrap();
}

#[tokio::test]
async fn constraint_error_mapping() {
    let pool = mem_pool().await;
    sqlx::query("CREATE TABLE uniq (email TEXT NOT NULL UNIQUE)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO uniq (email) VALUES ('a@x')")
        .execute(&pool)
        .await
        .unwrap();

    // UNIQUE → UniqueViolation with the SQLite-exact message
    let err = sqlx::query("INSERT INTO uniq (email) VALUES ('a@x')")
        .execute(&pool)
        .await
        .unwrap_err();
    let db_err = err.as_database_error().expect("database error");
    assert_eq!(db_err.kind(), sqlx::error::ErrorKind::UniqueViolation);
    assert_eq!(db_err.message(), "UNIQUE constraint failed: uniq.email");

    // NOT NULL → NotNullViolation
    let err = sqlx::query("INSERT INTO uniq (email) VALUES (NULL)")
        .execute(&pool)
        .await
        .unwrap_err();
    let db_err = err.as_database_error().expect("database error");
    assert_eq!(db_err.kind(), sqlx::error::ErrorKind::NotNullViolation);
    assert_eq!(db_err.message(), "NOT NULL constraint failed: uniq.email");
}

#[tokio::test]
async fn url_parsing() {
    use std::str::FromStr;

    let o = RustqliteConnectOptions::from_str("rustqlite::memory:").unwrap();
    assert!(o.is_in_memory());

    let o = RustqliteConnectOptions::from_str("rustqlite://:memory:?cache=shared").unwrap();
    assert!(o.is_in_memory() && o.is_shared_cache());

    let o = RustqliteConnectOptions::from_str("rustqlite://app.db?mode=rwc").unwrap();
    assert!(!o.is_in_memory());
}

#[tokio::test]
async fn file_backed_pool() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("app.db");
    let opts = RustqliteConnectOptions::filename(&path).create_if_missing(true);
    let pool = RustqlitePool::connect_with(opts).await.unwrap();

    sqlx::query("CREATE TABLE f (id INTEGER PRIMARY KEY, v TEXT)")
        .execute(&pool)
        .await
        .unwrap();
    for i in 0..10 {
        sqlx::query("INSERT INTO f (v) VALUES (?)")
            .bind(format!("row{i}"))
            .execute(&pool)
            .await
            .unwrap();
    }
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM f")
        .fetch_one(&pool)
        .await
        .unwrap();
    if n != 10 {
        // Failure forensics: WHICH rows made it distinguishes a lost LAST
        // insert (commit/chain flush), a rowid collision (overlap), or a
        // dropped middle row (allocation race).
        let rows: Vec<(i64, String)> = sqlx::query_as("SELECT id, v FROM f ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
        panic!("COUNT(*) = {n} (expected 10); rows present: {rows:?}");
    }
    assert_eq!(n, 10);
    pool.close().await;

    // Reopen: persistence across pools (registry keyed by path, engine
    // re-reads the file on a fresh process; within one process the
    // engine stays hot — verify by reading back through a new pool).
    let opts = RustqliteConnectOptions::filename(&path);
    let pool2 = RustqlitePool::connect_with(opts).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM f")
        .fetch_one(&pool2)
        .await
        .unwrap();
    assert_eq!(n, 10);
    pool2.close().await;
}

#[tokio::test]
async fn concurrent_pool_connections() {
    // Shared-cache in-memory pool: 8 connections hammering the engine.
    let pool = mem_pool().await;
    sqlx::query("CREATE TABLE conc (id INTEGER PRIMARY KEY, v TEXT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();

    let mut handles = Vec::new();
    for t in 0..8 {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            for i in 0..25 {
                let r = sqlx::query("INSERT INTO conc (v) VALUES (?)")
                    .bind(format!("t{t}-i{i}"))
                    .execute(&pool)
                    .await
                    .unwrap();
                assert_eq!(r.rows_affected(), 1);
                let _: Option<String> = sqlx::query_scalar("SELECT v FROM conc WHERE id = ?")
                    .bind(r.last_insert_rowid())
                    .fetch_optional(&pool)
                    .await
                    .unwrap()
                    .flatten();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conc")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 200);
}

#[tokio::test]
async fn multi_statement_scripts() {
    let pool = mem_pool().await;
    // DDL script via raw_sql
    raw_sql(
        "CREATE TABLE m (id INTEGER PRIMARY KEY, a INT, b TEXT);\n\
         INSERT INTO m (a, b) VALUES (1, 'one');\n\
         INSERT INTO m (a, b) VALUES (2, 'two');\n\
         -- trailing comment\n",
    )
    .execute(&pool)
    .await
    .unwrap();

    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM m")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 2);

    // Query with binds + multiple statements → protocol error
    let err = sqlx::query("SELECT 1; SELECT 2")
        .fetch_all(&pool)
        .await
        .unwrap_err();
    assert!(matches!(err, sqlx::Error::Protocol(_)), "{err:?}");
}

#[tokio::test]
async fn pragma_roundtrip() {
    // foreign_keys is connection state (a fresh pool connection would
    // re-apply the default ON, exactly like sqlx-sqlite) — use ONE
    // connection for a deterministic sequence.
    let mut conn =
        rustqlite::sqlx_driver::RustqliteConnection::open(&RustqliteConnectOptions::new()).unwrap();
    // read form
    let fk: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(fk, 1, "foreign_keys defaults ON (sqlx parity)");

    // write form
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(&mut conn)
        .await
        .unwrap();
    let fk: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(fk, 0);
}

#[tokio::test]
async fn prepare_and_statement_api() {
    let pool = mem_pool().await;
    sqlx::query("CREATE TABLE p (id INTEGER PRIMARY KEY, v TEXT)")
        .execute(&pool)
        .await
        .unwrap();

    let mut conn = pool.acquire().await.unwrap();
    let stmt = conn
        .prepare(SqlStr::from_static("INSERT INTO p (v) VALUES (?)"))
        .await
        .unwrap();
    assert_eq!(stmt.parameters(), Some(sqlx::Either::Right(1)));

    // query through the prepared statement
    stmt.query()
        .bind("prepared")
        .execute(&mut *conn)
        .await
        .unwrap();
    let v: String = sqlx::query_scalar("SELECT v FROM p WHERE id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(v, "prepared");
}

#[tokio::test]
async fn complex_queries() {
    let pool = mem_pool().await;
    raw_sql(
        "CREATE TABLE o (id INTEGER PRIMARY KEY, cust TEXT, total REAL);
         CREATE TABLE li (oid INT, qty INT, price REAL);
         INSERT INTO o (cust, total) VALUES ('ada', 30), ('bob', 12);
         INSERT INTO li (oid, qty, price) VALUES (1, 3, 10), (2, 2, 6);",
    )
    .execute(&pool)
    .await
    .unwrap();

    // JOIN + GROUP BY + ORDER BY
    #[derive(sqlx::FromRow)]
    struct Agg {
        cust: String,
        items: i64,
        amount: f64,
    }
    let rows: Vec<Agg> = sqlx::query_as(
        "SELECT o.cust, SUM(li.qty) AS items, SUM(li.qty * li.price) AS amount
         FROM o JOIN li ON li.oid = o.id
         GROUP BY o.cust
         HAVING SUM(li.qty) > 2
         ORDER BY amount DESC",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].cust, "ada");
    assert_eq!(rows[0].items, 3);
    assert!((rows[0].amount - 30.0).abs() < 1e-9);

    // window-ish subquery + LIMIT/OFFSET
    let top: Option<String> =
        sqlx::query_scalar("SELECT cust FROM (SELECT cust FROM o ORDER BY total DESC LIMIT 1)")
            .fetch_optional(&pool)
            .await
            .unwrap()
            .flatten();
    assert_eq!(top.as_deref(), Some("ada"));
}

#[test]
fn connection_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<rustqlite::sqlx_driver::RustqliteConnection>();
    assert_send_sync::<rustqlite::sqlx_driver::RustqlitePool>();
    assert_send_sync::<rustqlite::sqlx_driver::RustqliteRow>();
}

// ===========================================================================
// Isolation & concurrency semantics (SQLite snapshot/BUSY parity)
// ===========================================================================

/// Pool with a TINY busy timeout for fast negative tests.
async fn mem_pool_fast() -> RustqlitePool {
    let id = POOL_ID.fetch_add(1, Ordering::Relaxed);
    RustqlitePool::connect_with(
        RustqliteConnectOptions::shared_memory(format!("test-{id}"))
            .busy_timeout(std::time::Duration::from_millis(150)),
    )
    .await
    .unwrap()
}

/// No dirty reads: connection B must NEVER see A's uncommitted rows.
#[tokio::test]
async fn isolation_no_dirty_read() {
    let pool = mem_pool_fast().await;
    sqlx::query("CREATE TABLE iso (id INTEGER PRIMARY KEY, v TEXT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO iso (v) VALUES ('committed')")
        .execute(&pool)
        .await
        .unwrap();

    let mut a = pool.acquire().await.unwrap();
    let mut b = pool.acquire().await.unwrap();

    sqlx::query("BEGIN").execute(&mut *a).await.unwrap();
    sqlx::query("INSERT INTO iso (v) VALUES ('uncommitted')")
        .execute(&mut *a)
        .await
        .unwrap();

    // B reads while A's write tx is open: must NOT see the uncommitted row.
    // With a 150 ms busy timeout the read either waits-and-fails (BUSY) or,
    // if the tx closed first, sees committed state only.
    let res: Result<i64, _> = sqlx::query_scalar("SELECT COUNT(*) FROM iso")
        .fetch_one(&mut *b)
        .await;
    match res {
        Ok(n) => assert_eq!(n, 1, "read during open tx must see committed state only"),
        Err(e) => assert!(
            e.to_string().contains("database is locked"),
            "blocked read must surface SQLITE_BUSY, got: {e}"
        ),
    }

    // A still sees its own write (read-your-own-writes).
    let n_a: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM iso")
        .fetch_one(&mut *a)
        .await
        .unwrap();
    assert_eq!(n_a, 2);

    // After COMMIT, B immediately sees the row.
    sqlx::query("COMMIT").execute(&mut *a).await.unwrap();
    let n_b: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM iso")
        .fetch_one(&mut *b)
        .await
        .unwrap();
    assert_eq!(n_b, 2);
    drop(a);
    drop(b);
}

/// A ROLLBACK mid-transaction must never become visible either.
#[tokio::test]
async fn isolation_rollback_invisible() {
    let pool = mem_pool_fast().await;
    sqlx::raw_sql("CREATE TABLE rb (id INTEGER PRIMARY KEY, v TEXT)")
        .execute(&pool)
        .await
        .unwrap();

    let mut a = pool.acquire().await.unwrap();
    sqlx::query("BEGIN").execute(&mut *a).await.unwrap();
    sqlx::query("INSERT INTO rb (v) VALUES ('x')")
        .execute(&mut *a)
        .await
        .unwrap();
    sqlx::query("ROLLBACK").execute(&mut *a).await.unwrap();

    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rb")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

/// Read-only foreign transactions stay fully concurrent with readers.
#[tokio::test]
async fn readonly_tx_does_not_block_readers() {
    let pool = mem_pool().await;
    sqlx::raw_sql("CREATE TABLE ro (id INTEGER PRIMARY KEY, v INT)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql("INSERT INTO ro (v) VALUES (1),(2),(3)")
        .execute(&pool)
        .await
        .unwrap();

    let mut a = pool.acquire().await.unwrap();
    let mut b = pool.acquire().await.unwrap();
    sqlx::query("BEGIN").execute(&mut *a).await.unwrap(); // no writes yet

    // B reads while A holds a read-only tx — must be instant, not BUSY.
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ro")
        .fetch_one(&mut *b)
        .await
        .expect("read-only tx must not block readers");
    assert_eq!(n, 3);
    sqlx::query("COMMIT").execute(&mut *a).await.unwrap();
    drop(a);
    drop(b);
}

/// Writes during another connection's transaction wait for the busy
/// timeout and then fail with SQLITE_BUSY ("database is locked").
#[tokio::test]
async fn write_busy_timeout_then_sqlite_busy() {
    let pool = mem_pool_fast().await; // 150 ms busy timeout
    sqlx::raw_sql("CREATE TABLE bt (id INTEGER PRIMARY KEY, v TEXT)")
        .execute(&pool)
        .await
        .unwrap();

    let mut a = pool.acquire().await.unwrap();
    let mut b = pool.acquire().await.unwrap();
    sqlx::query("BEGIN").execute(&mut *a).await.unwrap();
    sqlx::query("INSERT INTO bt (v) VALUES ('a')")
        .execute(&mut *a)
        .await
        .unwrap();

    let t0 = std::time::Instant::now();
    let err = sqlx::query("INSERT INTO bt (v) VALUES ('b')")
        .execute(&mut *b)
        .await
        .expect_err("write during foreign tx must fail");
    let waited = t0.elapsed();
    assert!(
        err.to_string().contains("database is locked"),
        "expected SQLITE_BUSY, got: {err}"
    );
    assert!(
        waited >= std::time::Duration::from_millis(140),
        "busy timeout must actually wait (waited {waited:?})"
    );
    sqlx::query("ROLLBACK").execute(&mut *a).await.unwrap();
    drop(a);
    drop(b);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bt")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0, "A rolled back");
}

/// A writer blocked by a foreign transaction proceeds the moment that
/// transaction commits (busy WAIT, not just busy FAIL).
#[tokio::test]
async fn writer_wakes_after_foreign_commit() {
    let pool = mem_pool().await; // 5 s busy timeout
    sqlx::raw_sql("CREATE TABLE wk (id INTEGER PRIMARY KEY, v TEXT)")
        .execute(&pool)
        .await
        .unwrap();

    let mut a = pool.acquire().await.unwrap();
    let mut b = pool.acquire().await.unwrap();
    sqlx::query("BEGIN").execute(&mut *a).await.unwrap();
    sqlx::query("INSERT INTO wk (v) VALUES ('a')")
        .execute(&mut *a)
        .await
        .unwrap();

    // b writes while a's tx is open; a commits 150 ms later.
    let committer = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        sqlx::query("COMMIT").execute(&mut *a).await.unwrap();
    });
    let t0 = std::time::Instant::now();
    sqlx::query("INSERT INTO wk (v) VALUES ('b')")
        .execute(&mut *b)
        .await
        .expect("writer should proceed after the foreign tx commits");
    let waited = t0.elapsed();
    committer.await.unwrap();
    drop(b);
    assert!(
        waited >= std::time::Duration::from_millis(120),
        "writer should have waited for the commit ({waited:?})"
    );
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM wk")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 2);
}

/// busy_timeout = 0 restores instant BUSY (fail-fast) semantics.
#[tokio::test]
async fn zero_busy_timeout_fails_instantly() {
    let id = POOL_ID.fetch_add(1, Ordering::Relaxed);
    let pool = RustqlitePool::connect_with(
        RustqliteConnectOptions::shared_memory(format!("test-{id}"))
            .busy_timeout(std::time::Duration::ZERO),
    )
    .await
    .unwrap();
    sqlx::raw_sql("CREATE TABLE z (id INTEGER PRIMARY KEY)")
        .execute(&pool)
        .await
        .unwrap();

    let mut a = pool.acquire().await.unwrap();
    let mut b = pool.acquire().await.unwrap();
    sqlx::query("BEGIN").execute(&mut *a).await.unwrap();
    sqlx::query("INSERT INTO z (id) VALUES (1)")
        .execute(&mut *a)
        .await
        .unwrap();

    let t0 = std::time::Instant::now();
    let err = sqlx::query("INSERT INTO z (id) VALUES (2)")
        .execute(&mut *b)
        .await
        .expect_err("must fail");
    assert!(err.to_string().contains("database is locked"));
    assert!(
        t0.elapsed() < std::time::Duration::from_millis(100),
        "must be instant"
    );
    sqlx::query("ROLLBACK").execute(&mut *a).await.unwrap();
    drop(a);
    drop(b);
}

/// Dropping a connection mid-transaction rolls it back — one connection
/// can never wedge the pool by leaking a transaction.
#[tokio::test]
async fn dropped_connection_releases_tx() {
    let pool = mem_pool_fast().await;
    sqlx::raw_sql("CREATE TABLE dr (id INTEGER PRIMARY KEY, v TEXT)")
        .execute(&pool)
        .await
        .unwrap();

    // Open a raw BEGIN + INSERT on a dedicated connection, then DROP it.
    let mut a = pool.acquire().await.unwrap();
    sqlx::query("BEGIN").execute(&mut *a).await.unwrap();
    sqlx::query("INSERT INTO dr (v) VALUES ('doomed')")
        .execute(&mut *a)
        .await
        .unwrap();
    drop(a); // must roll back

    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dr")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

/// Even a raw-script BEGIN (invisible to sqlx's tx_depth) is cleaned up on drop.
#[tokio::test]
async fn dropped_connection_releases_raw_script_tx() {
    let pool = mem_pool_fast().await;
    sqlx::raw_sql("CREATE TABLE ds (id INTEGER PRIMARY KEY)")
        .execute(&pool)
        .await
        .unwrap();

    let mut a = pool.acquire().await.unwrap();
    sqlx::raw_sql("BEGIN; INSERT INTO ds (id) VALUES (1);")
        .execute(&mut *a)
        .await
        .unwrap();
    drop(a);

    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ds")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

/// The sqlx `Transaction` API on the pool: commit keeps data, rollback
/// discards, nested savepoints behave.
#[tokio::test]
async fn sqlx_transactions_with_isolation() {
    let pool = mem_pool().await;
    sqlx::raw_sql("CREATE TABLE tr (id INTEGER PRIMARY KEY, v TEXT)")
        .execute(&pool)
        .await
        .unwrap();

    // Commit path
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO tr (v) VALUES ('commit-me')")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // Rollback path
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO tr (v) VALUES ('discard-me')")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();

    // Nested savepoint: inner rollback only discards inner writes.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO tr (v) VALUES ('outer')")
        .execute(&mut *tx)
        .await
        .unwrap();
    {
        let mut sp = tx.begin().await.unwrap();
        sqlx::query("INSERT INTO tr (v) VALUES ('inner-doomed')")
            .execute(&mut *sp)
            .await
            .unwrap();
        sp.rollback().await.unwrap();
    }
    tx.commit().await.unwrap();

    let rows: Vec<String> = sqlx::query_scalar("SELECT v FROM tr ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(rows, vec!["commit-me".to_string(), "outer".to_string()]);
}

/// Concurrent transactions serialize cleanly: two BEGIN+INSERT+COMMIT
/// batches from different connections both land (no lost updates).
#[tokio::test]
async fn concurrent_transactions_serialize_correctly() {
    let pool = mem_pool().await;
    sqlx::raw_sql("CREATE TABLE cs (id INTEGER PRIMARY KEY, w INT, v INT)")
        .execute(&pool)
        .await
        .unwrap();

    let mut handles = Vec::new();
    for w in 0..4 {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            for i in 0..10 {
                let mut tx = pool.begin().await.unwrap();
                sqlx::query("INSERT INTO cs (w, v) VALUES (?, ?)")
                    .bind(w)
                    .bind(i)
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 40);
}

/// Mixed concurrent readers + writers: readers always observe a CONSISTENT
/// committed state (never partial batches) and writers all land.
#[tokio::test]
async fn concurrent_mixed_read_write_consistency() {
    let pool = mem_pool().await;
    sqlx::raw_sql("CREATE TABLE mx (id INTEGER PRIMARY KEY, batch INT, v INT)")
        .execute(&pool)
        .await
        .unwrap();

    const BATCHES: i64 = 20;
    const BATCH_SIZE: i64 = 5;

    // Writer: one tx per batch of 5.
    let writer = {
        let pool = pool.clone();
        tokio::spawn(async move {
            for batch in 0..BATCHES {
                let mut tx = pool.begin().await.unwrap();
                for i in 0..BATCH_SIZE {
                    sqlx::query("INSERT INTO mx (batch, v) VALUES (?, ?)")
                        .bind(batch)
                        .bind(i)
                        .execute(&mut *tx)
                        .await
                        .unwrap();
                }
                tx.commit().await.unwrap();
            }
        })
    };

    // Readers: sample COUNT continuously; it must always be a multiple of
    // BATCH_SIZE (a committed batch boundary — never a partial batch).
    let reader = {
        let pool = pool.clone();
        tokio::spawn(async move {
            let mut max_seen = 0i64;
            for _ in 0..400 {
                let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mx")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                assert_eq!(
                    n % BATCH_SIZE,
                    0,
                    "torn read: {n} rows — a partial batch was observed"
                );
                max_seen = max_seen.max(n);
                tokio::task::yield_now().await;
            }
            max_seen
        })
    };

    writer.await.unwrap();
    let max_seen = reader.await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mx")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, BATCHES * BATCH_SIZE);
    assert!(max_seen <= n);
}

/// BEGIN DEFERRED inside a select-only tx stays lock-free: several
/// connections can hold read transactions simultaneously.
#[tokio::test]
async fn multiple_concurrent_read_transactions() {
    let pool = mem_pool().await;
    sqlx::raw_sql("CREATE TABLE rt (id INTEGER PRIMARY KEY, v INT)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql("INSERT INTO rt (v) VALUES (42)")
        .execute(&pool)
        .await
        .unwrap();

    let mut handles = Vec::new();
    for _ in 0..4 {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            let mut tx = pool.begin().await.unwrap();
            let v: i64 = sqlx::query_scalar("SELECT v FROM rt WHERE id = 1")
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            tx.commit().await.unwrap();
            v
        }));
    }
    for h in handles {
        assert_eq!(h.await.unwrap(), 42);
    }
}

/// Cross-path-spelling engine coherence: the shared-engine registry must
/// map every spelling of a database path (raw, symlinked, pre-creation)
/// to ONE engine. The original key — `canonicalize(path)` with a raw
/// fallback — was computed BEFORE the file existed for connection #1
/// (raw spelling) and AFTER creation for later connections (canonical:
/// symlink-resolved on macOS `/var` → `/private/var`, long-path on
/// Windows 8.3 names), so pools could end up with TWO engines on one
/// file: the per-engine count cache then served `COUNT(*) = 0` while
/// `SELECT *` saw every row (the macOS/Windows CI `file_backed_pool`
/// failures). This test opens one pool through a SYMLINKED directory and
/// a second through the REAL path: both must see each other's writes
/// immediately.
#[tokio::test]
async fn file_backed_pool_path_spellings_share_one_engine() {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("real");
    std::fs::create_dir_all(&real).unwrap();
    let link = tmp.path().join("link");
    let linked = {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&real, &link).is_ok()
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_dir(&real, &link).is_ok()
        }
        #[cfg(not(any(unix, windows)))]
        {
            false
        }
    };
    if !linked {
        // Windows symlinks need developer mode; the canonical-key logic
        // is still exercised by file_backed_pool on every platform.
        eprintln!("symlink unavailable; skipping path-spelling coherence test");
        return;
    }

    // Pool #1: file does NOT exist yet, path spelled through the symlink.
    let p1 = RustqlitePool::connect_with(
        RustqliteConnectOptions::filename(link.join("coh.db")).create_if_missing(true),
    )
    .await
    .unwrap();
    sqlx::query("CREATE TABLE c (id INTEGER PRIMARY KEY, v TEXT)")
        .execute(&p1)
        .await
        .unwrap();
    for i in 0..3 {
        sqlx::query("INSERT INTO c (v) VALUES (?)")
            .bind(format!("r{i}"))
            .execute(&p1)
            .await
            .unwrap();
    }

    // Pool #2: the REAL spelling, file now exists. Must land on the SAME
    // engine (canonical key), not a second engine with a private pager.
    let p2 = RustqlitePool::connect_with(RustqliteConnectOptions::filename(real.join("coh.db")))
        .await
        .unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM c")
        .fetch_one(&p2)
        .await
        .unwrap();
    assert_eq!(n, 3, "second pool must see the first pool's rows");

    // Writes through pool #1 must be immediately visible to pool #2 —
    // one shared engine, not two engines with private page caches.
    sqlx::query("INSERT INTO c (v) VALUES ('more')")
        .execute(&p1)
        .await
        .unwrap();
    let n2: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM c")
        .fetch_one(&p2)
        .await
        .unwrap();
    assert_eq!(n2, 4, "cross-pool write visibility broke (split engines)");

    let n1: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM c")
        .fetch_one(&p1)
        .await
        .unwrap();
    assert_eq!(n1, 4);
    p1.close().await;
    p2.close().await;
}

/// Diagnostic companion to `file_backed_pool`: the macOS CI runner has
/// seen that test lose the LAST insert (COUNT=9 of 10, rows row0..row8)
/// three runs in a row while every other platform passes. This test runs
/// the same scenario with a COUNT after EVERY insert, so a CI failure
/// pinpoints exactly which insert vanishes (and the state dump separates
/// a lost write from a lost visibility).
#[tokio::test]
async fn file_backed_pool_diagnostic() {
    for round in 0..5 {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("diag.db");
        let opts = RustqliteConnectOptions::filename(&path).create_if_missing(true);
        let pool = RustqlitePool::connect_with(opts).await.unwrap();
        sqlx::query("CREATE TABLE f (id INTEGER PRIMARY KEY, v TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        for i in 0..10i64 {
            sqlx::query("INSERT INTO f (v) VALUES (?)")
                .bind(format!("row{i}"))
                .execute(&pool)
                .await
                .unwrap();
            let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM f")
                .fetch_one(&pool)
                .await
                .unwrap();
            if n != i + 1 {
                let rows: Vec<(i64, String)> = sqlx::query_as("SELECT id, v FROM f ORDER BY id")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
                let max: Option<i64> = sqlx::query_scalar("SELECT MAX(id) FROM f")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                panic!(
                    "round {round}: after insert #{i} COUNT={n} (expected {}); rows={rows:?}; MAX(id)={max:?}",
                    i + 1
                );
            }
        }
        pool.close().await;
    }
}

// ---------------------------------------------------------------------------
// WAL-grade committed-view read concurrency
// ---------------------------------------------------------------------------

/// Pool with a ZERO busy timeout: reads that would need to WAIT for a
/// foreign transaction fail instantly — the only reads that succeed are
/// the ones the engine serves from the committed view.
async fn zero_timeout_pool() -> RustqlitePool {
    let id = POOL_ID.fetch_add(1, Ordering::Relaxed);
    RustqlitePool::connect_with(
        RustqliteConnectOptions::shared_memory(format!("test-{id}"))
            .busy_timeout(std::time::Duration::ZERO),
    )
    .await
    .unwrap()
}

/// Reads during another connection's open WRITE transaction succeed
/// INSTANTLY (zero busy timeout) and see the BEGIN-time committed state
/// — WAL reader semantics. Before the committed view, these reads waited
/// for the transaction (or failed BUSY at timeout zero).
#[tokio::test]
async fn committed_read_during_open_write_txn() {
    let pool = zero_timeout_pool().await;
    sqlx::raw_sql("CREATE TABLE cv (id INTEGER PRIMARY KEY, v TEXT)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql("INSERT INTO cv (v) VALUES ('seed')")
        .execute(&pool)
        .await
        .unwrap();

    let mut a = pool.acquire().await.unwrap();
    let mut b = pool.acquire().await.unwrap();
    sqlx::query("BEGIN").execute(&mut *a).await.unwrap();
    sqlx::query("INSERT INTO cv (v) VALUES ('uncommitted')")
        .execute(&mut *a)
        .await
        .unwrap();

    // Zero busy timeout + a dirty foreign transaction: the read MUST still
    // succeed — served from the committed view, not the gate.
    let t0 = std::time::Instant::now();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cv")
        .fetch_one(&mut *b)
        .await
        .expect("committed-view read must not block on the open write txn");
    assert!(
        t0.elapsed() < std::time::Duration::from_millis(100),
        "must be instant"
    );
    assert_eq!(
        n, 1,
        "reader sees BEGIN-time committed state, not uncommitted rows"
    );

    // Uncommitted value must be invisible.
    let v: Option<String> = sqlx::query_scalar("SELECT v FROM cv WHERE id = 2")
        .fetch_optional(&mut *b)
        .await
        .unwrap();
    assert!(v.is_none(), "uncommitted insert invisible to readers");

    // Commit makes it visible.
    sqlx::query("COMMIT").execute(&mut *a).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cv")
        .fetch_one(&mut *b)
        .await
        .unwrap();
    assert_eq!(n, 2, "post-commit visibility");
    drop(a);
    drop(b);
}

/// ROLLBACK: the committed view state never leaks to readers.
#[tokio::test]
async fn committed_read_txn_rollback() {
    let pool = zero_timeout_pool().await;
    sqlx::raw_sql("CREATE TABLE cr (id INTEGER PRIMARY KEY, v TEXT)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql("INSERT INTO cr (v) VALUES ('keep')")
        .execute(&pool)
        .await
        .unwrap();

    let mut a = pool.acquire().await.unwrap();
    let mut b = pool.acquire().await.unwrap();
    sqlx::query("BEGIN").execute(&mut *a).await.unwrap();
    sqlx::query("UPDATE cr SET v = 'mutated'")
        .execute(&mut *a)
        .await
        .unwrap();

    let v: String = sqlx::query_scalar("SELECT v FROM cr WHERE id = 1")
        .fetch_one(&mut *b)
        .await
        .expect("committed read during open txn");
    assert_eq!(v, "keep", "reader sees the pre-image value");

    sqlx::query("ROLLBACK").execute(&mut *a).await.unwrap();
    let v: String = sqlx::query_scalar("SELECT v FROM cr WHERE id = 1")
        .fetch_one(&mut *b)
        .await
        .unwrap();
    assert_eq!(v, "keep", "rollback restored the committed value");
    drop(a);
    drop(b);
}

/// sqlx's Transaction API (deferred BEGIN, engine tx starts at the first
/// write): readers during the write phase see the committed snapshot and
/// never block.
#[tokio::test]
async fn committed_read_during_sqlx_transaction() {
    let pool = zero_timeout_pool().await;
    sqlx::raw_sql("CREATE TABLE tx (id INTEGER PRIMARY KEY, v INTEGER)")
        .execute(&pool)
        .await
        .unwrap();
    for i in 1..=10i64 {
        sqlx::query("INSERT INTO tx (v) VALUES (?)")
            .bind(i)
            .execute(&pool)
            .await
            .unwrap();
    }

    let mut a = pool.acquire().await.unwrap();
    let mut b = pool.acquire().await.unwrap();
    let mut txn = a.begin().await.unwrap();
    for i in 11..=20i64 {
        sqlx::query("INSERT INTO tx (v) VALUES (?)")
            .bind(i)
            .execute(&mut *txn)
            .await
            .unwrap();
    }

    // Reader during the open (deferred-started) write txn: instant, committed.
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tx")
        .fetch_one(&mut *b)
        .await
        .expect("committed read inside another connection's transaction");
    assert_eq!(n, 10, "BEGIN-time state");
    let sum: i64 = sqlx::query_scalar("SELECT SUM(v) FROM tx")
        .fetch_one(&mut *b)
        .await
        .unwrap();
    assert_eq!(sum, 55, "BEGIN-time aggregate");

    txn.commit().await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tx")
        .fetch_one(&mut *b)
        .await
        .unwrap();
    assert_eq!(n, 20);
    drop(a);
    drop(b);
}

/// Continuous reader THROUGHPUT while one long write transaction runs:
/// many reads complete mid-transaction (none block, none see
/// intermediate state). This is the read/write concurrency headline.
#[tokio::test]
async fn concurrent_reader_throughput_during_write_txn() {
    let pool = mem_pool().await; // 5 s busy timeout
    sqlx::raw_sql("CREATE TABLE rt (id INTEGER PRIMARY KEY, v INTEGER)")
        .execute(&pool)
        .await
        .unwrap();
    for i in 1..=100i64 {
        sqlx::query("INSERT INTO rt (v) VALUES (?)")
            .bind(i)
            .execute(&pool)
            .await
            .unwrap();
    }

    let mut a = pool.acquire().await.unwrap();
    let mut b = pool.acquire().await.unwrap();
    sqlx::query("BEGIN").execute(&mut *a).await.unwrap();

    // Writer inserts in the background.
    let writer = tokio::spawn(async move {
        for i in 101..=600i64 {
            sqlx::query("INSERT INTO rt (v) VALUES (?)")
                .bind(i)
                .execute(&mut *a)
                .await
                .unwrap();
            // Interleave with the reader (single-threaded runtimes run a
            // non-Pending task to completion otherwise).
            if i % 25 == 0 {
                tokio::task::yield_now().await;
            }
        }
        sqlx::query("COMMIT").execute(&mut *a).await.unwrap();
    });

    // Reader hammers counts while the txn is open: every read must return
    // the BEGIN-time snapshot (100) — no partial visibility, no waiting.
    let t0 = std::time::Instant::now();
    let mut reads = 0u32;
    while !writer.is_finished() {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rt")
            .fetch_one(&mut *b)
            .await
            .expect("read during open write txn");
        assert!(
            n == 100 || n == 600,
            "reader saw intermediate count {n} — isolation violated"
        );
        reads += 1;
        // The driver's committed-read path completes synchronously (no
        // Pending → no runtime yield): force a yield so the spawned writer
        // task gets scheduled on a current_thread runtime too.
        tokio::task::yield_now().await;
    }
    writer.await.unwrap();
    let elapsed = t0.elapsed();
    assert!(
        reads >= 5,
        "reader must have actually run mid-transaction ({reads} reads in {elapsed:?})"
    );
    drop(b);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rt")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 600);
}

/// DDL inside a transaction flips it to conservative gating: committed-
/// view reconstruction is not guaranteed while the schema moves, so
/// readers wait (rollback-journal semantics) instead.
#[tokio::test]
async fn ddl_txn_still_gates_readers() {
    let pool = mem_pool_fast().await; // 150 ms busy timeout
    sqlx::raw_sql("CREATE TABLE dg (id INTEGER PRIMARY KEY)")
        .execute(&pool)
        .await
        .unwrap();

    let mut a = pool.acquire().await.unwrap();
    let mut b = pool.acquire().await.unwrap();
    sqlx::query("BEGIN").execute(&mut *a).await.unwrap();
    sqlx::raw_sql("CREATE TABLE dg2 (id INTEGER PRIMARY KEY)")
        .execute(&mut *a)
        .await
        .unwrap();
    sqlx::raw_sql("INSERT INTO dg2 (id) VALUES (1)")
        .execute(&mut *a)
        .await
        .unwrap();

    // Reader during the DDL tx: waits the busy timeout, then BUSY.
    let t0 = std::time::Instant::now();
    let err = sqlx::query("SELECT COUNT(*) FROM dg")
        .fetch_one(&mut *b)
        .await
        .expect_err("read during DDL txn must gate");
    assert!(
        err.to_string().contains("database is locked"),
        "expected BUSY, got: {err}"
    );
    assert!(
        t0.elapsed() >= std::time::Duration::from_millis(140),
        "must have waited the busy timeout"
    );
    sqlx::query("ROLLBACK").execute(&mut *a).await.unwrap();
    drop(a);
    drop(b);
}

/// The transaction OWNER still reads its own uncommitted writes
/// (read-your-own-writes), even when its task migrates threads.
#[tokio::test]
async fn txn_owner_reads_own_writes() {
    let pool = mem_pool().await;
    sqlx::raw_sql("CREATE TABLE ow (id INTEGER PRIMARY KEY, v TEXT)")
        .execute(&pool)
        .await
        .unwrap();

    let mut a = pool.acquire().await.unwrap();
    sqlx::query("BEGIN").execute(&mut *a).await.unwrap();
    sqlx::query("INSERT INTO ow (v) VALUES ('mine')")
        .execute(&mut *a)
        .await
        .unwrap();

    // Owner read (same connection): uncommitted row VISIBLE.
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ow")
        .fetch_one(&mut *a)
        .await
        .expect("owner read");
    assert_eq!(n, 1, "owner reads its own uncommitted writes");

    // Yield points migrate tasks between workers: the connection identity
    // (not the thread) decides the view — still its own txn after yields.
    for _ in 0..3 {
        tokio::task::yield_now().await;
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ow")
            .fetch_one(&mut *a)
            .await
            .expect("owner read after yield");
        assert_eq!(
            n, 1,
            "owner still reads its own writes after task migration"
        );
    }

    sqlx::query("COMMIT").execute(&mut *a).await.unwrap();
    drop(a);
}
