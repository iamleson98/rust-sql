//! Atomic-write staging-file hygiene — the `db.rsqltmp*` lifecycle.
//!
//! A full-image write stages its bytes in `{stem}.rsqltmp{pid}x{seq}`
//! beside the database file and renames them over it. Three guarantees
//! pin the lifecycle shut:
//!
//! 1. A successful write session leaves NO staging file behind.
//! 2. A FAILED write (rename denied, IO error) still removes its
//!    staging file — a dev directory must never accumulate debris.
//!    Regression: `db.rsqltmp19508`-style files appeared beside dev
//!    databases whenever a make command's close-time dump hit a
//!    Windows sharing violation whose error the Drop path swallows.
//! 3. OPENING a database sweeps staging files orphaned by crashed or
//!    killed processes, while leaving foreign files (and this
//!    process's own staging files) untouched.
//!
//! Run: cargo test --test tempfile_hygiene

use rustqlite::storage::sqlitefmt::writer::write_image_atomic;
use rustqlite::Database;

/// A unique-enough temp path for one test. Removes any stale file so
/// re-running a failed test starts clean.
fn tmpdb(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("rustqlite_tmp_{}.db", name));
    let _ = std::fs::remove_file(&path);
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
    }
    path
}

/// The engine-owned staging files currently sitting beside `db`
/// (`{stem}.rsqltmp` + digits — the sweep's exact recognition rule).
fn staging_files(db: &std::path::Path) -> Vec<String> {
    let Some(dir) = db.parent() else {
        return Vec::new();
    };
    let Some(stem) = db.file_stem() else {
        return Vec::new();
    };
    let prefix = format!("{}.rsqltmp", stem.to_string_lossy());
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(&prefix)
            && name[prefix.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
        {
            found.push(name);
        }
    }
    found
}

/// Guarantee 1: the canonical open → write → flush → drop session
/// leaves no staging file, and the data survives the reopen.
#[test]
fn successful_writes_leave_no_staging_file() {
    let path = tmpdb("success");
    {
        let mut db = Database::open_sqlite_format(&path).expect("open");
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)", [])
            .expect("create");
        for _ in 0..25 {
            db.execute("INSERT INTO t(v) VALUES ('row')", [])
                .expect("insert");
        }
        db.flush().expect("flush");
    } // Drop: the close-time checkpoint writes the final image.
    assert!(
        staging_files(&path).is_empty(),
        "staging debris after clean session: {:?}",
        staging_files(&path)
    );

    let db = Database::open(&path).expect("reopen");
    let rows = db.query("SELECT COUNT(*) FROM t", []).expect("count");
    assert_eq!(rows[0][0].as_integer(), 25);
    drop(db);
    assert!(
        staging_files(&path).is_empty(),
        "staging debris after reopen: {:?}",
        staging_files(&path)
    );
    let _ = std::fs::remove_file(&path);
}

/// Guarantee 2: a write that cannot land must take its staging file
/// with it. The "destination" is a directory, so rename, remove and
/// the in-place rewrite all fail — the exact error path that used to
/// leak `db.rsqltmp*` debris.
#[test]
fn failed_write_removes_its_staging_file() {
    let dir = std::env::temp_dir().join("rustqlite_tmp_fail_dir");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // The destination path IS a directory: every landing strategy
    // fails, only the cleanup path can succeed.
    let dest = dir.join("db.sqlite");
    std::fs::create_dir_all(&dest).unwrap();

    let err = write_image_atomic(&dest, &[0u8; 4096]).expect_err("must fail");
    assert!(
        err.contains("rename") || err.contains("in-place"),
        "unexpected error: {err}"
    );
    let leaked = staging_files(&dest);
    assert!(leaked.is_empty(), "failed write leaked staging: {leaked:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Two failed writes in one process must BOTH clean up — and, via the
/// per-write seq counter, never share a staging path (concurrent
/// dumpers interleaving bytes through one file is corruption, not
/// just litter).
#[test]
fn repeated_failed_writes_leave_nothing_behind() {
    let dir = std::env::temp_dir().join("rustqlite_tmp_seq_dir");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dest = dir.join("db.sqlite");
    std::fs::create_dir_all(&dest).unwrap();

    assert!(write_image_atomic(&dest, &[0u8; 64]).is_err());
    assert!(write_image_atomic(&dest, &[0u8; 64]).is_err());

    let leaked = staging_files(&dest);
    assert!(
        leaked.is_empty(),
        "leaked staging after two failures: {leaked:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Guarantee 3a: opening a database reclaims staging files orphaned
/// by crashed or killed processes — both the old pid-only naming and
/// the current `pid x seq` naming.
#[test]
fn open_sweeps_orphaned_staging_files() {
    let path = tmpdb("sweep");
    {
        let mut db = Database::open_sqlite_format(&path).expect("open");
        db.execute("CREATE TABLE keep(v TEXT)", []).expect("create");
        db.execute("INSERT INTO keep VALUES ('x')", [])
            .expect("insert");
        db.flush().expect("flush");
    }
    // Simulate a crashed writer: staging files from OTHER pids.
    let stem = path.file_stem().unwrap().to_string_lossy().to_string();
    let dir = path.parent().unwrap();
    let old_fmt = dir.join(format!("{}.rsqltmp999999", stem));
    let new_fmt = dir.join(format!("{}.rsqltmp999998x3", stem));
    std::fs::write(&old_fmt, b"stale").unwrap();
    std::fs::write(&new_fmt, b"stale").unwrap();

    let db = Database::open(&path).expect("reopen sweeps");
    let rows = db.query("SELECT v FROM keep", []).expect("query");
    assert_eq!(rows[0][0].as_text(), "x");
    drop(db);

    assert!(!old_fmt.exists(), "old-format orphan not swept");
    assert!(!new_fmt.exists(), "new-format orphan not swept");
    assert!(staging_files(&path).is_empty());
    let _ = std::fs::remove_file(&path);
}

/// Guarantee 3b: the sweep only touches engine-owned names. A user
/// file sharing the prefix (non-digit suffix) and this process's own
/// (hypothetically live) staging file must survive.
#[test]
fn sweep_spares_foreign_and_own_files() {
    let path = tmpdb("spare");
    {
        let mut db = Database::open_sqlite_format(&path).expect("open");
        db.execute("CREATE TABLE t(v TEXT)", []).expect("create");
        db.flush().expect("flush");
    }
    let stem = path.file_stem().unwrap().to_string_lossy().to_string();
    let dir = path.parent().unwrap();
    // Non-digit suffix after the marker: not engine-owned.
    let foreign = dir.join(format!("{}.rsqltmp-notes.txt", stem));
    // This process's own staging file (live-write stand-in).
    let own = dir.join(format!("{}.rsqltmp{}x7", stem, std::process::id()));
    std::fs::write(&foreign, b"user data").unwrap();
    std::fs::write(&own, b"in-flight").unwrap();

    let db = Database::open(&path).expect("reopen");
    drop(db);

    assert!(foreign.exists(), "foreign file deleted by sweep");
    assert!(own.exists(), "own staging file deleted by sweep");
    let _ = std::fs::remove_file(&foreign);
    let _ = std::fs::remove_file(&own);
    let _ = std::fs::remove_file(&path);
}
