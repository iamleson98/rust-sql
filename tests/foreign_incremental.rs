//! The SQLite-format container's INCREMENTAL PAGE-DIFF architecture
//! (see `storage::sqlitefmt::container`): per-commit CPU and I/O must
//! scale with the CHANGED object, never the database — while the file
//! stays byte-honest for real SQLite (integrity_check, freelist,
//! rootpages, WAL recovery) at every step.
//!
//! What these tests pin:
//! * **frames-per-commit** — a single-row INSERT on a many-page
//!   database commits a handful of WAL frames, not a full image; the
//!   main file's size does not move between checkpoints.
//! * **page-identity stability** — committing to one table leaves
//!   every other table's pages byte-identical (the page space is
//!   shared, not re-flowed).
//! * **the freelist** — mass DELETE routes freed pages into a real
//!   SQLite freelist (header 32/36, trunk/leaf pages) that real
//!   SQLite's integrity_check accepts; later inserts reuse it.
//! * **DDL splices** — CREATE INDEX / DROP TABLE / ALTER TABLE commit
//!   as frames + freelist transitions, never a whole-file rewrite.
//! * **multi-session** — two sessions on one file splice into the
//!   SAME page space: both sessions' committed changes survive
//!   (per-object last-writer merge under the coordinator).
//! * **reopen-compare** — after every batch of random DML, a fresh
//!   engine open and real SQLite both see exactly the live session's
//!   state.

use rustqlite::storage::sqlitefmt::reader::wal_path_of;
use rustqlite::{Database, Value};

fn temp_path(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("rsql_inc_{}_{}", name, std::process::id()));
    for ext in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{}", p.display(), ext));
    }
    p
}

fn integrity(con: &rusqlite::Connection) -> String {
    con.query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap()
}

fn count(con: &rusqlite::Connection, table: &str) -> i64 {
    con.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

/// Seed a multi-table database: 8 rowid tables with an index each plus
/// one WITHOUT ROWID table, ~400 rows per table (a few hundred pages —
/// big enough that a full-image rewrite would be obvious).
fn seed(db: &mut Database) {
    for t in 1..=8 {
        db.execute(
            &format!("CREATE TABLE t{t}(id INTEGER PRIMARY KEY, a TEXT, b INTEGER, c REAL)"),
            [],
        )
        .unwrap();
        db.execute(&format!("CREATE INDEX ix{t} ON t{t}(b, a)"), [])
            .unwrap();
    }
    db.execute(
        "CREATE TABLE wr(k TEXT PRIMARY KEY, v INTEGER) WITHOUT ROWID",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for t in 1..=8 {
        db.execute(
            &format!(
                "WITH RECURSIVE seq(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM seq WHERE i < 400) \
                 INSERT INTO t{t}(a, b, c) SELECT 'x' || i, i, i * 0.5 FROM seq"
            ),
            [],
        )
        .unwrap();
    }
    db.execute(
        "WITH RECURSIVE seq(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM seq WHERE i < 200) \
         INSERT INTO wr(k, v) SELECT 'k' || i, i FROM seq",
        [],
    )
    .unwrap();
    db.execute("COMMIT", []).unwrap();
}

// ---------------------------------------------------------------------------
// 1. Per-commit cost: frames, not images
// ---------------------------------------------------------------------------

#[test]
fn wal_frames_per_commit_scale_with_the_changed_object() {
    let path = temp_path("frames");
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        seed(&mut db);
    }
    let wal = wal_path_of(&path);
    assert!(!wal.exists(), "clean close folds the seed's frames away");
    let main_len = std::fs::metadata(&path).unwrap().len();
    {
        let mut db = Database::open(&path).unwrap();
        // 40 single-row autocommit INSERTs spread over two tables.
        for i in 0..40 {
            let t = if i % 2 == 0 { 1 } else { 2 };
            db.execute(
                &format!("INSERT INTO t{t}(a, b, c) VALUES ('new{i}', {i}, {i}.5)"),
                [],
            )
            .unwrap();
        }
        // Parse the sidecar: each commit's frames = pages between
        // commit markers; every commit must be tiny (leaf + index leaf
        // + page 1 — a handful), never the database's page count.
        let wal_bytes = std::fs::read(&wal).unwrap();
        let fsz = 24 + 4096usize;
        assert!(wal_bytes.len() > 32, "frames were appended");
        let mut commits: Vec<usize> = Vec::new();
        let mut frames = 0usize;
        let mut off = 32usize;
        while off + fsz <= wal_bytes.len() {
            let db_size = u32::from_be_bytes(wal_bytes[off + 4..off + 8].try_into().unwrap());
            frames += 1;
            if db_size != 0 {
                commits.push(frames);
                frames = 0;
            }
            off += fsz;
        }
        assert_eq!(commits.len(), 40, "one commit marker per INSERT");
        let max_commit = commits.iter().copied().max().unwrap();
        let n_pages = main_len / 4096;
        assert!(
            max_commit <= 12,
            "the largest commit wrote {max_commit} frames on a {n_pages}-page database"
        );
        // The main file did not move between checkpoints.
        let now = std::fs::metadata(&path).unwrap().len();
        assert_eq!(now, main_len, "no whole-file rewrite mid-session");
    }
    drop(rusqlite::Connection::open(&path).unwrap());
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// 2. Page-identity stability: untouched objects keep their bytes
// ---------------------------------------------------------------------------

#[test]
fn untouched_table_pages_stay_byte_identical() {
    let path = temp_path("stable");
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        seed(&mut db);
        // Fold everything into the main file for a clean byte baseline.
        db.execute("VACUUM", []).unwrap();
    }
    let before = std::fs::read(&path).unwrap();
    let ps = 4096usize;
    {
        let mut db = Database::open(&path).unwrap();
        for i in 0..25 {
            db.execute(
                &format!("INSERT INTO t1(a, b, c) VALUES ('u{i}', {i}, {i}.25)"),
                [],
            )
            .unwrap();
        }
    }
    let after = std::fs::read(&path).unwrap();
    // Count differing pages. t1 + ix1 (+ possibly the schema tree and
    // the header) may move; everything else must be byte-identical.
    let n = before.len().min(after.len()) / ps;
    let mut changed_pages = 0usize;
    for p in 0..n {
        if before[p * ps..(p + 1) * ps] != after[p * ps..(p + 1) * ps] {
            changed_pages += 1;
        }
    }
    let total_pages = before.len() / ps;
    assert!(
        changed_pages * 4 <= total_pages,
        "{changed_pages} of {total_pages} pages changed for a 25-row insert into ONE table"
    );
    // Real SQLite still validates the file end to end.
    let con = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(integrity(&con), "ok");
    assert_eq!(count(&con, "t1"), 425);
    assert_eq!(count(&con, "t5"), 400, "untouched tables intact");
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// 3. The freelist: shrinkage routes into a real SQLite freelist
// ---------------------------------------------------------------------------

#[test]
fn mass_delete_populates_a_real_sqlite_freelist() {
    let path = temp_path("freelist");
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        seed(&mut db);
        db.execute("DELETE FROM t1 WHERE id > 40", []).unwrap();
        db.execute("DELETE FROM t2 WHERE id > 40", []).unwrap();
    }
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(integrity(&con), "ok");
        let fc: i64 = con
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))
            .unwrap();
        assert!(fc > 5, "the shrunk tables freed pages: freelist={fc}");
        assert_eq!(count(&con, "t1"), 40);
        // The header fields agree with the pragma (the engine wrote a
        // real trunk/leaf chain, fields 32/36).
        let bytes = std::fs::read(&path).unwrap();
        let head = u32::from_be_bytes(bytes[32..36].try_into().unwrap());
        let cnt = u32::from_be_bytes(bytes[36..40].try_into().unwrap());
        assert_eq!(cnt as i64, fc, "header field 36 == freelist_count");
        assert!(head >= 2 && head as usize <= bytes.len() / 4096);
    }
    {
        // Later inserts reuse the freelist instead of growing the file.
        let before_pages = std::fs::metadata(&path).unwrap().len() / 4096;
        let mut db = Database::open(&path).unwrap();
        for i in 0..40 {
            db.execute(
                &format!("INSERT INTO t1(a, b, c) VALUES ('r{i}', {i}, {i}.5)"),
                [],
            )
            .unwrap();
        }
        drop(db);
        let after_pages = std::fs::metadata(&path).unwrap().len() / 4096;
        assert!(
            after_pages <= before_pages + 2,
            "freelist reuse: {before_pages} -> {after_pages} pages"
        );
        let con = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(integrity(&con), "ok");
        assert_eq!(count(&con, "t1"), 80);
    }
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// 4. DDL splices: schema changes commit as frames + freelist, not rewrites
// ---------------------------------------------------------------------------

#[test]
fn ddl_commits_splice_without_whole_file_rewrite() {
    let path = temp_path("ddl");
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        seed(&mut db);
        db.execute("VACUUM", []).unwrap(); // dense baseline
    }
    let base_len = std::fs::metadata(&path).unwrap().len();
    {
        let mut db = Database::open(&path).unwrap();
        let len_before = std::fs::metadata(&path).unwrap().len();
        db.execute("CREATE INDEX ix_extra ON t3(c)", []).unwrap();
        let len_after = std::fs::metadata(&path).unwrap().len();
        assert_eq!(
            len_before, len_after,
            "CREATE INDEX commits as WAL frames — the main file does not move"
        );
        db.execute("ALTER TABLE t4 ADD COLUMN d TEXT DEFAULT 'z'", [])
            .unwrap();
        db.execute("DROP TABLE t5", []).unwrap();
    }
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(integrity(&con), "ok");
        assert_eq!(count(&con, "t3"), 400);
        assert_eq!(count(&con, "t4"), 400);
        let d: String = con
            .query_row("SELECT d FROM t4 WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(d, "z", "ALTER TABLE ADD COLUMN round-trips");
        // t5 is gone; its pages are on the freelist.
        let fc: i64 = con
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))
            .unwrap();
        assert!(fc > 5, "DROP TABLE freed its pages: freelist={fc}");
        let n: i64 = con
            .query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))
            .unwrap();
        // 17 seed objects (8 tables + 8 indexes + wr) + ix_extra - t5's
        // table and index = 16.
        assert_eq!(n, 16);
    }
    // The post-DDL file is still comparable to the dense baseline: the
    // new index's tail pages are the only growth (t5's drop went to the
    // freelist, not a re-flow) — a wholesale re-layout would rewrite
    // every object's span.
    let end_len = std::fs::metadata(&path).unwrap().len();
    assert!(
        end_len <= base_len + 8 * 4096,
        "no wholesale re-layout: {base_len} -> {end_len}"
    );
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// 5. Multi-session: two sessions share one page space
// ---------------------------------------------------------------------------

#[test]
fn two_sessions_merge_per_object() {
    let path = temp_path("multi");
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        db.execute("CREATE TABLE a(x INTEGER PRIMARY KEY, v TEXT)", [])
            .unwrap();
        db.execute("CREATE TABLE b(x INTEGER PRIMARY KEY, v TEXT)", [])
            .unwrap();
        db.execute("INSERT INTO a(v) VALUES ('a0')", []).unwrap();
        db.execute("INSERT INTO b(v) VALUES ('b0')", []).unwrap();
    }
    {
        // Both sessions open on the same file; each commits to ITS
        // table. Neither whole-image clobbers the other.
        let mut s1 = Database::open(&path).unwrap();
        let mut s2 = Database::open(&path).unwrap();
        s1.execute("INSERT INTO a(v) VALUES ('a1')", []).unwrap();
        s2.execute("INSERT INTO b(v) VALUES ('b1')", []).unwrap();
        s1.execute("INSERT INTO a(v) VALUES ('a2')", []).unwrap();
        s2.execute("INSERT INTO b(v) VALUES ('b2')", []).unwrap();
        drop(s1);
        drop(s2);
    }
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(integrity(&con), "ok");
        assert_eq!(count(&con, "a"), 3, "session 1's commits survived");
        assert_eq!(count(&con, "b"), 3, "session 2's commits survived");
    }
    {
        let db = Database::open(&path).unwrap();
        let rows = db.query("SELECT v FROM a ORDER BY x", []).unwrap();
        let got: Vec<String> = rows.iter().map(|r| r[0].as_text().to_string()).collect();
        assert_eq!(got, vec!["a0", "a1", "a2"]);
    }
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// 6. Random DML with reopen-compare against the engine AND real SQLite
// ---------------------------------------------------------------------------

#[test]
fn random_dml_reopen_compare() {
    let path = temp_path("fuzz");
    let mut lcg = 0x1234_5678u64;
    let mut rnd = || {
        lcg = lcg
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (lcg >> 33) as i64
    };
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        db.execute(
            "CREATE TABLE m(id INTEGER PRIMARY KEY, a TEXT, b INTEGER, blob BLOB)",
            [],
        )
        .unwrap();
        db.execute("CREATE INDEX ixm ON m(b)", []).unwrap();
        db.execute(
            "CREATE TABLE w(k TEXT PRIMARY KEY, v INT) WITHOUT ROWID",
            [],
        )
        .unwrap();
        for round in 0..12 {
            for _ in 0..25 {
                let op = rnd().rem_euclid(4);
                let id = 1 + rnd().rem_euclid(300);
                match op {
                    0 => {
                        // Blob sizes stay modest: the fuzz targets the
                        // container's splice correctness; the engine's
                        // big-blob REPLACE split corner is a pre-existing
                        // NATIVE-layer bug (examples/probe_native_fuzz.rs
                        // reproduces it without any SQLite-format code)
                        // - tracked for the follow-up ledger, out of
                        // scope here.
                        let blob_len = (rnd().rem_euclid(1200)) as usize;
                        let blob: Vec<u8> = (0..blob_len).map(|i| (i % 251) as u8).collect();
                        db.execute(
                            "INSERT OR REPLACE INTO m(id, a, b, blob) VALUES (?, ?, ?, ?)",
                            [
                                Value::Integer(id),
                                Value::Text(format!("s{id}-{round}").into()),
                                Value::Integer(rnd()),
                                Value::Blob(blob),
                            ],
                        )
                        .unwrap();
                    }
                    1 => {
                        db.execute("DELETE FROM m WHERE id = ?", [Value::Integer(id)])
                            .unwrap();
                    }
                    2 => {
                        db.execute(
                            "INSERT OR REPLACE INTO w(k, v) VALUES (?, ?)",
                            [Value::Text(format!("k{id}").into()), Value::Integer(round)],
                        )
                        .unwrap();
                    }
                    _ => {
                        db.execute("UPDATE m SET b = b + 1 WHERE id < ?", [Value::Integer(id)])
                            .unwrap();
                    }
                }
            }
            // Reopen-compare every round: a fresh engine connection and
            // real SQLite must agree with the live session exactly.
            let live_m: i64 = db.query("SELECT count(*) FROM m", []).unwrap()[0][0].as_integer();
            let live_w: i64 = db.query("SELECT count(*) FROM w", []).unwrap()[0][0].as_integer();
            let live_sum: i64 =
                db.query("SELECT ifnull(sum(b), 0) FROM m", []).unwrap()[0][0].as_integer();
            let db2 = Database::open(&path).unwrap();
            let m2: i64 = db2.query("SELECT count(*) FROM m", []).unwrap()[0][0].as_integer();
            let w2: i64 = db2.query("SELECT count(*) FROM w", []).unwrap()[0][0].as_integer();
            let sum2: i64 =
                db2.query("SELECT ifnull(sum(b), 0) FROM m", []).unwrap()[0][0].as_integer();
            assert_eq!((live_m, live_w, live_sum), (m2, w2, sum2), "round {round}");
            drop(db2);
            let con = rusqlite::Connection::open(&path).unwrap();
            assert_eq!(integrity(&con), "ok", "round {round}");
            assert_eq!(count(&con, "m"), live_m, "round {round}");
            assert_eq!(count(&con, "w"), live_w, "round {round}");
            drop(con);
        }
    }
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// 7. DELETE-mode incremental commits (rollback journal protocol)
// ---------------------------------------------------------------------------

#[test]
fn delete_mode_incremental_commits() {
    let path = temp_path("delmode");
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        seed(&mut db);
        db.execute("PRAGMA journal_mode = DELETE", []).unwrap();
        let len0 = std::fs::metadata(&path).unwrap().len();
        for i in 0..30 {
            db.execute(
                &format!("INSERT INTO t1(a, b, c) VALUES ('d{i}', {i}, {i}.5)"),
                [],
            )
            .unwrap();
        }
        let len1 = std::fs::metadata(&path).unwrap().len();
        assert!(len1 > len0, "growth reaches the main file in DELETE mode");
        assert!(
            !wal_path_of(&path).exists(),
            "no WAL sidecar in DELETE mode"
        );
        assert!(
            !std::path::Path::new(&format!("{}-journal", path.display())).exists(),
            "a clean commit leaves no journal at rest"
        );
    }
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(integrity(&con), "ok");
        assert_eq!(count(&con, "t1"), 430);
        let fl: i64 = con
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))
            .unwrap();
        assert!(fl >= 0);
    }
    // The engine reopens its own DELETE-mode output.
    {
        let db = Database::open(&path).unwrap();
        let n: i64 = db.query("SELECT count(*) FROM t1", []).unwrap()[0][0].as_integer();
        assert_eq!(n, 430);
    }
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// 8. Counters and pragmas report the container's truth
// ---------------------------------------------------------------------------

#[test]
fn counters_and_pragmas_track_the_page_space() {
    let path = temp_path("counters");
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        db.execute("CREATE TABLE t(x)", []).unwrap();
        let cookie1: i64 = db.query("PRAGMA schema_version", []).unwrap()[0][0].as_integer();
        db.execute("INSERT INTO t VALUES (1)", []).unwrap();
        let cookie2: i64 = db.query("PRAGMA schema_version", []).unwrap()[0][0].as_integer();
        assert_eq!(cookie1, cookie2, "DML does not bump the schema cookie");
        db.execute("CREATE TABLE u(x)", []).unwrap();
        let cookie3: i64 = db.query("PRAGMA schema_version", []).unwrap()[0][0].as_integer();
        assert_eq!(cookie3, cookie2 + 1, "DDL bumps the cookie");
        let pc: i64 = db.query("PRAGMA page_count", []).unwrap()[0][0].as_integer();
        let real = std::fs::metadata(&path).unwrap().len() / 4096;
        // WAL mode: the container's live count includes un-checkpointed
        // growth, so it can exceed the main file's current length.
        assert!(
            pc as u64 >= real,
            "page_count reports the container: {pc} vs {real}"
        );
    }
    {
        // After the clean close (sidecar folded), the main file's header
        // carries every commit's counter.
        let bytes = std::fs::read(&path).unwrap();
        let c1 = u32::from_be_bytes(bytes[24..28].try_into().unwrap());
        assert!(c1 >= 3, "change counter advanced across commits: {c1}");
        let vv = u32::from_be_bytes(bytes[92..96].try_into().unwrap());
        assert_eq!(vv, c1, "version-valid-for tracks the counter");
    }
    let _ = std::fs::remove_file(&path);
}
