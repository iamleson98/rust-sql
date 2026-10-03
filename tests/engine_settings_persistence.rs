//! Engine-wide PRAGMA settings must survive engine recreation.
//!
//! Regression for the 2026-10-03 datxevui.com incident: the compat
//! layer's engines() registry holds each per-file engine behind a
//! `Weak` — when the last connection closes, the engine is dropped and
//! the next open recreates it with the BUILT-IN defaults. A pool that
//! had applied `PRAGMA cache_size=-65536` at startup silently fell
//! back to the 2 MiB default (permanently full cache, ~2.5% hit rate
//! on a 633 MB database) the moment its connection set cycled through
//! zero.
//!
//! The fix: the pager remembers the last engine-wide settings per
//! canonical file path (see `FILE_SETTINGS` in src/storage/pager.rs)
//! and re-applies them when a new engine opens the same file. This
//! test pins that mechanism at the engine level — open, configure,
//! DROP every handle, reopen, and the settings must still be there.

use rustqlite::Database;

fn temp_path(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("engine-settings-{}-{}", tag, std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join(format!("{tag}.db"))
}

#[test]
fn engine_wide_pragmas_survive_engine_recreation() {
    let path = temp_path("recreate");
    let _ = std::fs::remove_file(&path);

    // Generation 1: open, configure engine-wide settings, write data.
    {
        let mut db = Database::open(&path).expect("open generation 1");
        db.execute("CREATE TABLE t(a INTEGER)", []).expect("create");
        db.execute("INSERT INTO t VALUES (42)", []).expect("insert");
        db.execute("PRAGMA cache_size=-8192", [])
            .expect("cache_size");
        db.execute("PRAGMA synchronous=1", []).expect("synchronous");
        db.execute("PRAGMA temp_store=2", []).expect("temp_store");
        // 8 MiB at the default 4 KiB page size = 2048 pages.
        assert_eq!(db.pager().cache_capacity(), 2048, "capacity applied");
        assert_eq!(db.pager().cache_size_setting(), -8192);
        assert_eq!(db.pager().synchronous(), 1);
        assert_eq!(db.pager().temp_store(), 2);
    }
    // Generation 1 is dropped here — no handles left, exactly like a
    // pool whose connections have all been reaped.

    // Generation 2: a brand-new engine for the same file must inherit
    // the settings instead of resetting to the defaults (512 pages /
    // -2000 KiB / FULL sync / FILE temp store).
    {
        let db = Database::open(&path).expect("open generation 2");
        assert_eq!(
            db.pager().cache_capacity(),
            2048,
            "cache_size must survive engine recreation (got {} pages)",
            db.pager().cache_capacity()
        );
        assert_eq!(db.pager().cache_size_setting(), -8192);
        assert_eq!(db.pager().synchronous(), 1, "synchronous inherited");
        assert_eq!(db.pager().temp_store(), 2, "temp_store inherited");
        // And the data is intact, of course.
        let (cols, rows) = db
            .query_with_columns("SELECT a FROM t", [])
            .expect("select");
        assert_eq!(cols.len(), 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], rustqlite::Value::Integer(42));
    }

    let _ = std::fs::remove_file(&path);
}

#[test]
fn aliased_paths_share_one_settings_entry() {
    // The app may open the same file via different path strings across
    // generations (relative in one config, absolute in another, or with
    // redundant `..` components). The settings key is the CANONICAL
    // path, so all of them must resolve to the same remembered
    // settings. No CWD games here — `..`-flavored aliases are just as
    // good and are portable (cargo test runs tests in parallel
    // threads; a set_current_dir would race every other file test).
    let dir = std::env::temp_dir().join(format!("engine-settings-canon-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let parent = dir.parent().expect("parent").to_path_buf();
    let abs = dir.join("canon.db");
    let _ = std::fs::remove_file(&abs);

    // Configure via the plain absolute path.
    {
        let mut db = Database::open(&abs).expect("open abs");
        db.execute("PRAGMA cache_size=-4096", []).expect("pragma");
        assert_eq!(db.pager().cache_capacity(), 1024);
    }
    // Reopen via the aliased path `<parent>/<dir-name>/../<dir-name>/
    // canon.db` — same file, different path STRING.
    let alias = parent
        .join(dir.file_name().expect("dir name"))
        .join("..")
        .join(dir.file_name().expect("dir name"))
        .join("canon.db");
    let db = Database::open(&alias).expect("open alias");
    assert_eq!(
        db.pager().cache_capacity(),
        1024,
        "settings key must be canonical (aliased open inherits)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn in_memory_databases_never_record_settings() {
    // `:memory:` pagers never persist (the cache IS the store) — and
    // they must not pollute the per-file map for a future file that
    // happens to be created at an equal-looking path.
    {
        let mut db = Database::open_in_memory().expect("memory open");
        db.execute("PRAGMA cache_size=-8192", []).expect("pragma");
        // Capacity semantics are irrelevant for memory stores; the
        // assertion is that this does NOT poison a later file open.
    }
    let path = temp_path("unpoisoned");
    let _ = std::fs::remove_file(&path);
    {
        let db = Database::open(&path).expect("open fresh file");
        assert_eq!(
            db.pager().cache_capacity(),
            512,
            "a fresh file must keep the engine default, not inherit a \
             memory pager's settings"
        );
    }
    let _ = std::fs::remove_file(&path);
}
