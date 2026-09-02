//! sqlx + sea-orm on rustqlite — the end-to-end interop proof.
//!
//! Everything here is stock sqlx / sea-orm API usage. The binary links
//! rustqlite's SQLite C ABI compatibility library (via the patched
//! libsqlite3-sys), so every query executes on the rustqlite engine.

use sqlx::sqlite::SqlitePoolOptions;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Row, SqlitePool};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::temp_dir().join(format!("rustqlite-sqlx-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let url = format!("sqlite://{}", path.display());

    println!("== 1. pool connect ==");
    let opts = SqliteConnectOptions::new().filename(&path).create_if_missing(true);
    let pool: SqlitePool = SqlitePoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(opts)
        .await?;
    println!("connected: {:?}", url);

    println!("== 2. DDL + insert + select ==");
    sqlx::query("CREATE TABLE users (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, age INTEGER)")
        .execute(&pool)
        .await?;

    for (name, age) in [("alice", 30), ("bob", 25), ("carol", 35)] {
        let r = sqlx::query("INSERT INTO users (name, age) VALUES (?, ?)")
            .bind(name)
            .bind(age)
            .execute(&pool)
            .await?;
        println!("inserted {} -> rows_affected={} last_insert_id={}", name, r.rows_affected(), r.last_insert_rowid());
    }

    let rows = sqlx::query("SELECT id, name, age FROM users WHERE age > ? ORDER BY id")
        .bind(24)
        .fetch_all(&pool)
        .await?;
    println!("rows with age > 24: {}", rows.len());
    for row in &rows {
        let id: i64 = row.try_get("id")?;
        let name: String = row.try_get("name")?;
        let age: i32 = row.try_get("age")?;
        assert_eq!(id > 0, true);
        println!("  {} {} {}", id, name, age);
    }
    assert_eq!(rows.len(), 3, "expected 3 users");

    println!("== 3. query_as + FromRow ==");
    #[derive(Debug, sqlx::FromRow)]
    struct User {
        id: i64,
        name: String,
        age: i32,
    }
    let users: Vec<User> = sqlx::query_as::<_, User>("SELECT id, name, age FROM users ORDER BY id")
        .fetch_all(&pool)
        .await?;
    assert_eq!(users.len(), 3);
    assert_eq!(users[0].name, "alice");
    println!("query_as ok: {:?}", users);

    println!("== 4. aggregates + expressions ==");
    let (count, avg): (i64, f64) = sqlx::query_as("SELECT COUNT(*), AVG(age) FROM users")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 3);
    println!("count={} avg={:.2}", count, avg);

    let (mn, mx, total): (i32, i32, i64) = sqlx::query_as("SELECT MIN(age), MAX(age), SUM(age) FROM users")
        .fetch_one(&pool)
        .await?;
    assert_eq!((mn, mx, total), (25, 35, 90));
    println!("min={} max={} sum={}", mn, mx, total);

    println!("== 5. transactions (BEGIN/COMMIT via prepared statements) ==");
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE users SET age = age + 1 WHERE name = ?")
        .bind("alice")
        .execute(&mut *tx)
        .await?;
    let age: i32 = sqlx::query_scalar("SELECT age FROM users WHERE name = 'alice'")
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(age, 31, "inside tx: alice age should be 31");
    tx.commit().await?;
    let age: i32 = sqlx::query_scalar("SELECT age FROM users WHERE name = 'alice'")
        .fetch_one(&pool)
        .await?;
    assert_eq!(age, 31, "after commit: alice age should be 31");
    println!("commit ok, alice age = {}", age);

    println!("== 6. rollback ==");
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM users").execute(&mut *tx).await?;
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users").fetch_one(&mut *tx).await?;
    assert_eq!(n, 0, "inside tx all deleted");
    tx.rollback().await?;
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users").fetch_one(&pool).await?;
    assert_eq!(n, 3, "after rollback, 3 users remain");
    println!("rollback ok, {} users", n);

    println!("== 7. constraint error mapping ==");
    sqlx::query("CREATE TABLE uniq (id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE)")
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO uniq (email) VALUES ('a@x')")
        .execute(&pool)
        .await?;
    let dup = sqlx::query("INSERT INTO uniq (email) VALUES ('a@x')")
        .execute(&pool)
        .await;
    match dup {
        Err(sqlx::Error::Database(db_err)) => {
            println!("duplicate insert -> kind={:?}", db_err.kind());
            assert!(
                matches!(db_err.kind(), sqlx::error::ErrorKind::UniqueViolation),
                "expected a unique violation, got {:?}",
                db_err.kind()
            );
            println!("message: {}", db_err.message());
        }
        other => panic!("expected a database error, got ok={:?}", other.is_ok()),
    }
    let null_violation = sqlx::query("INSERT INTO uniq (email) VALUES (NULL)")
        .execute(&pool)
        .await;
    match null_violation {
        Err(sqlx::Error::Database(db_err)) => {
            println!("null insert -> kind={:?}", db_err.kind());
            println!("message: {}", db_err.message());
        }
        other => panic!("expected a database error, got ok={:?}", other.is_ok()),
    }

    println!("== 8. multi-statement script (exec) ==");
    sqlx::raw_sql("CREATE TABLE t2 (x); INSERT INTO t2 VALUES (10), (20); INSERT INTO t2 VALUES (30);")
        .execute(&pool)
        .await?;
    let total: i64 = sqlx::query_scalar("SELECT SUM(x) FROM t2").fetch_one(&pool).await?;
    assert_eq!(total, 60);
    println!("raw_sql script ok, sum={}", total);

    println!("== 9. concurrent pool usage (multi-connection) ==");
    let mut handles: Vec<tokio::task::JoinHandle<sqlx::Result<(i32, usize, u64)>>> = Vec::new();
    for i in 0..8 {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            let rows: Vec<i64> =
                sqlx::query_scalar("SELECT id FROM users WHERE age >= ?").bind(25).fetch_all(&pool).await?;
            let r = sqlx::query("UPDATE users SET age = age + 1 WHERE id = ?")
                .bind(1)
                .execute(&pool)
                .await?;
            sqlx::Result::Ok((i, rows.len(), r.rows_affected()))
        }));
    }
    for h in handles {
        let (i, n, aff) = h.await??;
        assert_eq!(n, 3);
        println!("  task {} read {} rows, affected {}", i, n, aff);
    }

    println!("== 10. NULL + typed parameters ==");
    sqlx::query("CREATE TABLE blobs (id INTEGER PRIMARY KEY, data BLOB, note TEXT)")
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO blobs (data, note) VALUES (?, ?)")
        .bind(vec![1u8, 2, 3, 4, 5])
        .bind(Option::<String>::None)
        .execute(&pool)
        .await?;
    let (data, note): (Vec<u8>, Option<String>) =
        sqlx::query_as("SELECT data, note FROM blobs WHERE id = 1")
            .fetch_one(&pool)
            .await?;
    assert_eq!(data, vec![1, 2, 3, 4, 5]);
    assert!(note.is_none());
    println!("blob + NULL roundtrip ok");

    println!("== 11. read-only connection (mode=ro / SqliteConnectOptions.read_only) ==");
    {
        // Read-only pool over the SAME file: SELECTs work, writes are
        // rejected with SQLITE_READONLY ("attempt to write a readonly
        // database") — SQLite's readonly-connection semantics.
        let ro_opts = SqliteConnectOptions::new()
            .filename(&path)
            .read_only(true);
        let ro_pool: SqlitePool = SqlitePoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(ro_opts)
            .await?;
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blobs")
            .fetch_one(&ro_pool)
            .await?;
        assert_eq!(n, 1);
        let write = sqlx::query("INSERT INTO blobs (data, note) VALUES (?, ?)")
            .bind(vec![9u8])
            .bind("nope")
            .execute(&ro_pool)
            .await;
        let msg = format!("{}", write.err().expect("write must fail on ro connection"));
        assert!(
            msg.contains("readonly") || msg.contains("read-only"),
            "unexpected error: {}",
            msg
        );
        println!("read-only enforcement ok: writes rejected, reads served");
        ro_pool.close().await;
    }

    pool.close().await;
    let _ = std::fs::remove_file(&path);
    println!("\nALL SQLX INTEROP TESTS PASSED");
    Ok(())
}
