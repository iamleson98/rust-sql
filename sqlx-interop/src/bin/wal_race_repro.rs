//! Reproduction: the datxevui.com "WAL frame salt mismatch" outage (2026-09-18).
//!
//! Shape of the production incident:
//!   * sqlx pool (sea-orm) over the rustqlite engine, WAL mode
//!   * every connection runs `PRAGMA journal_mode = WAL` on open
//!   * sqlx retires connections at max_lifetime (prod: 30 min) — when the
//!     whole startup cohort retires together, the shared engine's refcount
//!     drops to zero and Engine::drop -> Pager::drop runs
//!     `checkpoint + remove_file(<db>-wal)`… racing the pool opener that is
//!     already creating the NEXT engine generation + a fresh WAL file.
//!   * observed prod symptoms, in order:
//!       (code 10) io error: failed to fill whole buffer
//!       (code 11) WAL frame salt mismatch
//!     with `ls /proc/<pid>/fd` showing `<db>-wal (deleted)` and NO WAL file
//!     on disk while the engine kept "committing" into the deleted inode.
//!
//! This repro compresses the timeline: max_lifetime ~ 300ms, idle_timeout
//! ~ 200ms, continuous concurrent INSERT/SELECT traffic, and a watchdog
//! that reports the exact prod failure signatures.

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

static ERR_SALT: AtomicUsize = AtomicUsize::new(0);
static ERR_EOF: AtomicUsize = AtomicUsize::new(0);
static ERR_OTHER: AtomicUsize = AtomicUsize::new(0);
static OK_INSERTS: AtomicUsize = AtomicUsize::new(0);
static BUG_SEEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::temp_dir().join(format!("walrace-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let db = dir.join("app.db");
    let _ = std::fs::remove_file(&db);
    let wal = {
        let mut s = db.as_os_str().to_os_string();
        s.push("-wal");
        std::path::PathBuf::from(s)
    };
    let _ = std::fs::remove_file(&wal);

    // Matches the app: WAL journal mode is sqlx's default; we also mirror
    // the app's explicit pragma application once after connect.
    let opts = SqliteConnectOptions::new()
        .filename(&db)
        .create_if_missing(true);

    println!("step: connecting pool…");
    let pool: SqlitePool = SqlitePoolOptions::new()
        .max_connections(8)
        .min_connections(2)
        .acquire_timeout(Duration::from_secs(10))
        .idle_timeout(Duration::from_millis(200))
        .max_lifetime(Duration::from_millis(300)) // prod: 30 min, compressed
        .connect_with(opts)
        .await?;
    println!("step: pool connected, creating table…");

    sqlx::query("CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT NOT NULL)")
        .execute(&pool)
        .await?;
    println!("step: table created, applying pragmas…");

    // WAL + the same pragmas the app applies.
    for pragma in [
        "PRAGMA journal_mode=WAL;",
        "PRAGMA synchronous=NORMAL;",
        "PRAGMA busy_timeout=5000;",
        "PRAGMA foreign_keys=ON;",
    ] {
        let _ = sqlx::query(pragma).execute(&pool).await;
    }

    let stop = Arc::new(AtomicBool::new(false));
    let start = Instant::now();

    // Watchdog: the PRECISE bug detector. In the broken state the process
    // holds an open fd to a DELETED `-wal` file and keeps "committing"
    // into the unlinked inode (non-durable). Legitimate inter-generation
    // gaps (last-connection-close removes the WAL; the next open
    // recreates it) have NO such fd. We watch /proc/self/fd.
    {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut deleted_wal_fds: usize = 0;
            while !stop.load(Ordering::Relaxed) {
                if let Ok(entries) = std::fs::read_dir("/proc/self/fd") {
                    for entry in entries.flatten() {
                        if let Ok(target) = std::fs::read_link(entry.path()) {
                            let t = target.to_string_lossy();
                            if t.ends_with("-wal (deleted)") || t.contains("-wal (deleted)") {
                                deleted_wal_fds += 1;
                                BUG_SEEN.store(true, Ordering::Relaxed);
                                println!("BUG DETECTED: live fd -> {}", t);
                            }
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            println!(
                "watchdog: deleted-wal fd sightings={} (0 = healthy), salt_errors={} eof_errors={} other={} ok_inserts={}",
                deleted_wal_fds,
                ERR_SALT.load(Ordering::Relaxed),
                ERR_EOF.load(Ordering::Relaxed),
                ERR_OTHER.load(Ordering::Relaxed),
                OK_INSERTS.load(Ordering::Relaxed)
            );
        });
    }

    // Writer tasks (concurrent inserts — mirrors register/login/session writes).
    let mut handles = Vec::new();
    for w in 0..4u32 {
        let pool = pool.clone();
        let stop = stop.clone();
        handles.push(tokio::spawn(async move {
            let mut i = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let res = sqlx::query("INSERT INTO t (v) VALUES (?)")
                    .bind(format!("w{}-{}", w, i))
                    .execute(&pool)
                    .await;
                match res {
                    Ok(_) => {
                        OK_INSERTS.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        let msg = format!("{e}");
                        if msg.contains("salt mismatch") {
                            ERR_SALT.fetch_add(1, Ordering::Relaxed);
                        } else if msg.contains("failed to fill whole buffer") {
                            ERR_EOF.fetch_add(1, Ordering::Relaxed);
                        } else {
                            ERR_OTHER.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                i += 1;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }));
    }

    // Reader tasks (mirrors SELECTs).
    for _ in 0..2u32 {
        let pool = pool.clone();
        let stop = stop.clone();
        handles.push(tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                let res = sqlx::query("SELECT COUNT(*), MAX(id) FROM t")
                    .fetch_one(&pool)
                    .await;
                if let Err(e) = res {
                    let msg = format!("{e}");
                    if msg.contains("salt mismatch") {
                        ERR_SALT.fetch_add(1, Ordering::Relaxed);
                    } else if msg.contains("failed to fill whole buffer") {
                        ERR_EOF.fetch_add(1, Ordering::Relaxed);
                    } else {
                        ERR_OTHER.fetch_add(1, Ordering::Relaxed);
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }));
    }

    // Pool churn driver: periodic acquire+release bursts — mirrors request
    // patterns forcing the pool to open fresh connections right around the
    // lifetime-retirement boundary.
    {
        let pool = pool.clone();
        let stop = stop.clone();
        handles.push(tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                for _ in 0..6 {
                    if let Ok(mut conn) = pool.acquire().await {
                        let _ = sqlx::query("SELECT 1").fetch_one(&mut *conn).await;
                        drop(conn);
                    }
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        }));
    }

    // Run the race window for a while.
    tokio::time::sleep(Duration::from_secs(12)).await;
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        let _ = h.await;
    }
    pool.close().await;

    let elapsed = start.elapsed();
    let salt = ERR_SALT.load(Ordering::Relaxed);
    let eof = ERR_EOF.load(Ordering::Relaxed);
    let other = ERR_OTHER.load(Ordering::Relaxed);
    let ok = OK_INSERTS.load(Ordering::Relaxed);

    println!("\n===== RESULT after {:.1}s =====", elapsed.as_secs_f32());
    println!("ok_inserts={} salt_mismatch={} eof_errors={} other_errors={}", ok, salt, eof, other);

    // Integrity: reopen and verify all committed rows are visible.
    // (NOT read-only: a WAL with committed frames needs recovery-on-open,
    // which writes — a read-only handle cannot recover and errors.)
    let opts2 = SqliteConnectOptions::new().filename(&db).create_if_missing(true);
    let pool2: SqlitePool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts2)
        .await?;
    if let Ok(row) = sqlx::query("SELECT COUNT(*) AS c, MAX(id) AS m FROM t").fetch_one(&pool2).await {
        let c: i64 = row.try_get("c")?;
        let m: i64 = row.try_get("m")?;
        println!("reopen: count={} max_id={}", c, m);
        if (c as usize) < ok {
            println!("DATA LOSS: {} inserts ok but only {} rows durable!", ok, c);
        }
    }
    pool2.close().await;

    // Final verdict
    if salt > 0 || eof > 0 || BUG_SEEN.load(Ordering::Relaxed) {
        println!(
            "REPRODUCED: the WAL race is live (salt={} eof={} deleted_fd={})",
            salt,
            eof,
            BUG_SEEN.load(Ordering::Relaxed)
        );
        std::process::exit(2);
    } else {
        println!("NOT reproduced in this window (ok={} other={}) — widen timing if flaky.", ok, other);
        Ok(())
    }
}
