//! sqlx::migrate! end-to-end against the rustqlite engine: applies a
//! two-step migration directory, re-runs idempotently, verifies the
//! schema and data, and rolls back a failing migration atomically.

use sqlx::sqlite::SqlitePoolOptions;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::migrate::MigrateError;

#[tokio::main]
async fn main() {
    let dir = std::env::temp_dir().join(format!("rustqlite_migrate_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("migrate.db");

    let opts = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await
        .expect("connect");

    // 1. Fresh apply.
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrate run");
    println!("== 1. fresh migrate ok ==");

    // 2. Schema present + seed row applied.
    let (version,): (i64,) = sqlx::query_as("SELECT version FROM _sqlx_migrations ORDER BY version")
        .fetch_one(&pool)
        .await
        .expect("migrations recorded");
    println!("== 2. _sqlx_migrations version {} ==", version);

    let (n, note): (i64, String) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(MAX(note), '') FROM users",
    )
    .fetch_one(&pool)
    .await
    .expect("query users");
    assert_eq!(n, 1, "migration 2 seeded one user");
    assert_eq!(note, "first");
    println!("== 3. seeded data visible (n=1, note='first') ==");

    // ALTER TABLE ADD COLUMN from migration 2 round-trips.
    let cols: Vec<(String,)> = sqlx::query_as("SELECT name FROM pragma_table_info('users') ORDER BY cid")
        .fetch_all(&pool)
        .await
        .expect("pragma_table_info");
    let names: Vec<String> = cols.into_iter().map(|c| c.0).collect();
    assert_eq!(names, vec!["id", "name", "email", "note"]);
    println!("== 4. pragma_table_info columns: {:?} ==", names);

    // 3. Idempotent re-run: no error, no duplicate application.
    sqlx::migrate!("./migrations").run(&pool).await.expect("re-run");
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2, "still exactly two applied migrations");
    println!("== 5. idempotent re-run ok ({} applied) ==", count);

    // 4. A FAILING migration must roll back atomically (multi-statement
    //    transaction: the CREATE TABLE must not survive).
    let bad_dir = dir.join("bad_migrations");
    std::fs::create_dir_all(&bad_dir).unwrap();
    std::fs::write(
        bad_dir.join("20260901000003_bad.sql"),
        "CREATE TABLE should_not_survive (id INTEGER PRIMARY KEY);\nINSERT INTO nonexistent_table VALUES (1);",
    )
    .unwrap();
    let bad = format!("sqlite://{}?mode=rw", db_path.display());
    let pool2 = SqlitePoolOptions::new()
        .max_connections(2)
        .connect(&bad)
        .await
        .expect("connect rw");

    // Run the bad migration manually (bypass the macro — compile-time
    // directory). Apply the same statements inside a transaction.
    let result: Result<(), MigrateError> = async {
        let mut tx = pool2.begin().await?;
        sqlx::query("CREATE TABLE should_not_survive (id INTEGER PRIMARY KEY)")
            .execute(&mut *tx)
            .await?;
        let r = sqlx::query("INSERT INTO nonexistent_table VALUES (1)").execute(&mut *tx).await;
        match r {
            Ok(_) => {}
            Err(e) => return Err(MigrateError::Execute(e)),
        }
        tx.commit().await?;
        Ok(())
    }
    .await;
    assert!(result.is_err(), "bad migration must fail");
    let leftover: Result<(i64,), _> = sqlx::query_as("SELECT COUNT(*) FROM should_not_survive")
        .fetch_one(&pool2)
        .await;
    assert!(
        leftover.is_err(),
        "rolled-back transaction must not leave the table behind"
    );
    println!("== 6. failing migration rolled back atomically ==");

    // 5. Concurrent pools: second connection sees the migrated schema.
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(&pool2)
        .await
        .unwrap();
    assert_eq!(n, 1);
    println!("== 7. second pool sees migrated schema ==");

    let _ = std::fs::remove_dir_all(&dir);
    println!("\nALL SQLX MIGRATION TESTS PASSED");
}
