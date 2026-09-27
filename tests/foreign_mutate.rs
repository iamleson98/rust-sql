//! PAGE-LEVEL SPLICING (see `storage::sqlitefmt::mutator`) — the
//! container-side b-tree mutator that applies a commit's row deltas
//! directly onto the object's existing foreign-format pages (descend,
//! verify, patch, split, propagate), touching only the CHANGED pages.
//!
//! What these tests pin:
//! * **engagement + root stability** — autocommit DML after the seed
//!   commits through the mutate path: rootpages never move (the
//!   mutator's split discipline keeps the root's identity), commits
//!   stay a handful of WAL frames, and the reopened file passes real
//!   SQLite's integrity_check with exact content.
//! * **same-commit double allocation** — one commit whose deltas make
//!   a table AND its index both split (both mutation sessions allocate
//!   fresh pages in the same publish): the page grants must be
//!   disjoint. This is the "2nd reference to page N" corruption class
//!   the million-record differential caught.
//! * **fallback** — non-journaled write paths (SAVEPOINT / ROLLBACK
//!   TO) poison the journal; the commit falls back to the whole-object
//!   splice and the file stays correct.
//! * **WITHOUT ROWID** — entry-tree point mutations (insert / update /
//!   delete) through the mutator's index-tree discipline.
//! * **overflow chains** — big-blob rows move their chains on update
//!   and free them on delete; integrity holds through the churn.
//! * **rowid-alias moves** — `UPDATE t SET id = X` (an internal
//!   delete+insert reported as one UPDATE event) mutates correctly.
//! * **reopen-compare** — rounds of mixed autocommit DML, each round
//!   re-verified by a fresh engine open and real SQLite.

use rustqlite::storage::sqlitefmt::reader::wal_path_of;
use rustqlite::{Database, Value};

fn temp_path(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("rsql_mut_{}_{}", name, std::process::id()));
    for ext in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{}", p.display(), ext));
    }
    p
}

fn integrity(con: &rusqlite::Connection) -> String {
    con.query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap()
}

fn rootpage(con: &rusqlite::Connection, name: &str) -> i64 {
    con.query_row(
        "SELECT rootpage FROM sqlite_schema WHERE name = ?1",
        [name],
        |r| r.get(0),
    )
    .unwrap()
}

/// Verify a LIVE session's committed state without touching the
/// original files (a real-SQLite open would checkpoint the WAL sidecar
/// away, and the coordinator's external-touch guard would then force a
/// full republish — correct, but it would move rootpages and defeat
/// the stability assertion). Copy main + WAL aside, verify the copy.
fn verify_copy(path: &std::path::Path, name: &str) -> (String, i64, i64) {
    let tmp = path.with_extension("verify.db");
    let tmp_wal = wal_path_of(&tmp);
    for p in [
        &tmp,
        &tmp_wal,
        &tmp.with_extension("verify.db-shm"),
        &tmp.with_extension("verify.db-journal"),
    ] {
        let _ = std::fs::remove_file(p);
    }
    std::fs::copy(path, &tmp).unwrap();
    let _ = std::fs::copy(wal_path_of(path), &tmp_wal);
    let con = rusqlite::Connection::open(&tmp).unwrap();
    let ic = integrity(&con);
    let rp = rootpage(&con, name);
    let n: i64 = con
        .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
        .unwrap();
    drop(con);
    for p in [
        &tmp,
        &tmp_wal,
        &tmp.with_extension("verify.db-shm"),
        &tmp.with_extension("verify.db-journal"),
    ] {
        let _ = std::fs::remove_file(p);
    }
    (ic, rp, n)
}

/// Count the frames of each commit in a WAL sidecar (commits are
/// separated by the commit-marker frame's big-endian size field).
fn commits_frames(wal: &std::path::Path) -> Vec<usize> {
    let bytes = std::fs::read(wal).unwrap();
    let fsz = 24 + 4096usize;
    let mut out = Vec::new();
    let mut frames = 0usize;
    let mut off = 32usize;
    while off + fsz <= bytes.len() {
        frames += 1;
        let commit_bytes = u32::from_be_bytes([
            bytes[off + 4],
            bytes[off + 5],
            bytes[off + 6],
            bytes[off + 7],
        ]);
        if commit_bytes != 0 {
            out.push(frames);
            frames = 0;
        }
        off += fsz;
    }
    out
}

// ---------------------------------------------------------------------------
// 1. Engagement: root stability + tiny commits + exact content
// ---------------------------------------------------------------------------

#[test]
fn autocommit_dml_mutates_in_place_roots_never_move() {
    let path = temp_path("engage");
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        db.execute(
            "CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT, b INTEGER, pad TEXT)",
            [],
        )
        .unwrap();
        db.execute("CREATE INDEX ib ON t(b)", []).unwrap();
        db.execute("BEGIN", []).unwrap();
        db.execute(
            "WITH RECURSIVE seq(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM seq WHERE i < 600) \
             INSERT INTO t(a, b, pad) SELECT 'a'||i, (i*7919)%600, printf('%.*c', 60, 'x') FROM seq",
            [],
        )
        .unwrap();
        db.execute("COMMIT", []).unwrap();

        // The seed's commit moved roots (whole-tree splices on growth
        // inside the transaction); record the settled roots now — from a
        // COPY of the file (a direct real-SQLite open would checkpoint
        // the sidecar and invalidate the coordinator's layout).
        let (ic0, root_t, n0) = verify_copy(&path, "t");
        assert_eq!(ic0, "ok");
        assert_eq!(n0, 600);
        let (_, root_ib, _) = {
            // The index root: same copy trick, index-specific query.
            let tmp = path.with_extension("verify.db");
            std::fs::copy(&path, &tmp).unwrap();
            let _ = std::fs::copy(wal_path_of(&path), wal_path_of(&tmp));
            let con = rusqlite::Connection::open(&tmp).unwrap();
            let rp = rootpage(&con, "ib");
            drop(con);
            let _ = std::fs::remove_file(&tmp);
            let _ = std::fs::remove_file(wal_path_of(&tmp));
            ("ok".to_string(), rp, 0i64)
        };

        // The measured phase: 20 INSERTs + 20 UPDATEs + 10 DELETEs,
        // all autocommit — the mutate path. Roots must NOT move.
        for i in 1..=20i64 {
            db.execute(
                "INSERT INTO t(a, b, pad) VALUES ('n'||?1, ?2, printf('%.*c', 50, 'y'))",
                [Value::Integer(i), Value::Integer((i * 104729) % 600)],
            )
            .unwrap();
        }
        for i in 1..=20i64 {
            db.execute(
                "UPDATE t SET b = ?1, pad = printf('%.*c', 45, 'z') WHERE id = ?2",
                [Value::Integer((i * 7919) % 600), Value::Integer(i)],
            )
            .unwrap();
        }
        for i in 1..=10i64 {
            db.execute("DELETE FROM t WHERE id = ?1", [Value::Integer(i)])
                .unwrap();
        }
        let got: i64 = db.query("SELECT count(*) FROM t", []).unwrap()[0][0].as_integer();
        assert_eq!(got, 610, "live count after the mutate churn");

        // Root stability across the whole mutate phase: the mutator's
        // splits keep the root page's identity, so the schema row (and
        // the file's rootpage) never moves on DML. Verified on a COPY
        // while the session is live; the final check uses the cleanly
        // closed original.
        let (ic1, root_t2, n1) = verify_copy(&path, "t");
        assert_eq!(ic1, "ok", "real SQLite integrity after mutate churn");
        assert_eq!(n1, 610);
        let (_, root_ib2, _) = {
            let tmp = path.with_extension("verify.db");
            std::fs::copy(&path, &tmp).unwrap();
            let _ = std::fs::copy(wal_path_of(&path), wal_path_of(&tmp));
            let con = rusqlite::Connection::open(&tmp).unwrap();
            let rp = rootpage(&con, "ib");
            let s: i64 = con
                .query_row("SELECT sum(b) FROM t", [], |r| r.get(0))
                .unwrap();
            drop(con);
            let _ = std::fs::remove_file(&tmp);
            let _ = std::fs::remove_file(wal_path_of(&tmp));
            assert!(s > 0);
            ("ok".to_string(), rp, 0i64)
        };
        assert_eq!(root_t, root_t2, "table rootpage must not move on DML");
        assert_eq!(root_ib, root_ib2, "index rootpage must not move on DML");
    }
    // Final full verification on the cleanly-closed original.
    let con = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(integrity(&con), "ok");
    let n: i64 = con
        .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 610);
    drop(con);
    for ext in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{}", path.display(), ext));
    }
}

// ---------------------------------------------------------------------------
// 2. Same-commit double allocation (the corruption class)
// ---------------------------------------------------------------------------

#[test]
fn same_commit_table_and_index_splits_allocate_disjoint_pages() {
    let path = temp_path("dualsplit");
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        db.execute(
            "CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT, b UNIQUE)",
            [],
        )
        .unwrap();
        // Seed: one small batch so the tree exists with a span + version.
        db.execute(
            "WITH RECURSIVE seq(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM seq WHERE i < 40) \
             INSERT INTO t(a, b) SELECT 'a'||i, i*13 FROM seq",
            [],
        )
        .unwrap();

        // ONE autocommit multi-row INSERT (~600 rows x ~70-byte payloads)
        // whose deltas make BOTH the table b-tree and the UNIQUE index
        // b-tree split several times inside the SAME publish — the two
        // mutation sessions must hand out DISJOINT fresh pages. The
        // pre-fix failure was real SQLite's "2nd reference to page N".
        db.execute(
            "WITH RECURSIVE seq(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM seq WHERE i < 600) \
             INSERT INTO t(a, b) SELECT 'b'||i, 10000+i*7 FROM seq",
            [],
        )
        .unwrap();
        let got: i64 = db.query("SELECT count(*) FROM t", []).unwrap()[0][0].as_integer();
        assert_eq!(got, 640);
    }
    let con = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        integrity(&con),
        "ok",
        "same-commit table+index splits must allocate disjoint pages"
    );
    let n: i64 = con
        .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 640);
    let dups: i64 = con
        .query_row(
            "SELECT count(*) FROM (SELECT b FROM t GROUP BY b HAVING count(*) > 1)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(dups, 0, "the UNIQUE index must be exact");
    drop(con);
    for ext in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{}", path.display(), ext));
    }
}

// ---------------------------------------------------------------------------
// 3. Fallback: poisoned journal still commits correctly
// ---------------------------------------------------------------------------

#[test]
fn poisoned_journal_falls_back_to_whole_object_splice() {
    let path = temp_path("fallback");
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)", [])
            .unwrap();
        db.execute(
            "WITH RECURSIVE seq(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM seq WHERE i < 300) \
             INSERT INTO t(v) SELECT 'v'||i FROM seq",
            [],
        )
        .unwrap();
        // SAVEPOINT paths poison the delta journal (not journaled);
        // the DML inside must still commit exactly.
        db.execute("SAVEPOINT s1", []).unwrap();
        db.execute("INSERT INTO t(v) VALUES ('sv1')", []).unwrap();
        db.execute("RELEASE s1", []).unwrap();
        // ROLLBACK TO: partial undo also poisons; correctness must not
        // depend on the journal.
        db.execute("SAVEPOINT s2", []).unwrap();
        db.execute("INSERT INTO t(v) VALUES ('sv2-kept')", [])
            .unwrap();
        db.execute("SAVEPOINT s3", []).unwrap();
        db.execute("INSERT INTO t(v) VALUES ('sv3-gone')", [])
            .unwrap();
        db.execute("ROLLBACK TO s3", []).unwrap();
        db.execute("RELEASE s2", []).unwrap();
        // Plain DML after the poison (capture stays suspended until the
        // next publish resets it).
        db.execute("UPDATE t SET v = 'upd' WHERE id = 5", [])
            .unwrap();
        let got: i64 = db.query("SELECT count(*) FROM t", []).unwrap()[0][0].as_integer();
        assert_eq!(got, 302, "300 seed + sv1 + sv2-kept");
        let v5: String = db.query("SELECT v FROM t WHERE id = 5", []).unwrap()[0][0].as_text();
        assert_eq!(v5, "upd");
    }
    let con = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(integrity(&con), "ok");
    let n: i64 = con
        .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 302);
    let v5: String = con
        .query_row("SELECT v FROM t WHERE id = 5", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v5, "upd");
    drop(con);
    for ext in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{}", path.display(), ext));
    }
}

// ---------------------------------------------------------------------------
// 4. WITHOUT ROWID entry trees
// ---------------------------------------------------------------------------

#[test]
fn without_rowid_point_mutations_stay_exact() {
    let path = temp_path("worowid");
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        db.execute(
            "CREATE TABLE wr(k TEXT PRIMARY KEY, v INTEGER) WITHOUT ROWID",
            [],
        )
        .unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 1..=400i64 {
            db.execute(
                "INSERT INTO wr(k, v) VALUES (?1, ?2)",
                [Value::Text(format!("key{i:04}").into()), Value::Integer(i)],
            )
            .unwrap();
        }
        db.execute("COMMIT", []).unwrap();
        // Mutate-path point ops (entry insert / replace / delete).
        for i in 1..=30i64 {
            db.execute(
                "INSERT INTO wr(k, v) VALUES (?1, ?2)",
                [
                    Value::Text(format!("new{i:04}").into()),
                    Value::Integer(1000 + i),
                ],
            )
            .unwrap();
        }
        for i in 1..=20i64 {
            db.execute(
                "UPDATE wr SET v = ?1 WHERE k = ?2",
                [
                    Value::Integer(2000 + i),
                    Value::Text(format!("key{i:04}").into()),
                ],
            )
            .unwrap();
        }
        for i in 1..=10i64 {
            db.execute(
                "DELETE FROM wr WHERE k = ?1",
                [Value::Text(format!("key{:04}", 50 + i).into())],
            )
            .unwrap();
        }
        let n: i64 = db.query("SELECT count(*) FROM wr", []).unwrap()[0][0].as_integer();
        assert_eq!(n, 420, "400 + 30 - 10");
    }
    let con = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(integrity(&con), "ok", "WITHOUT ROWID mutate integrity");
    let n: i64 = con
        .query_row("SELECT count(*) FROM wr", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 420);
    // Key order must be the b-tree order (real SQLite walks it).
    let k1: String = con
        .query_row("SELECT k FROM wr ORDER BY k LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(k1, "key0001", "sorted order preserved through mutations");
    let sum: i64 = con
        .query_row("SELECT sum(v) FROM wr", [], |r| r.get(0))
        .unwrap();
    // Seed 1..=400; +30 inserts (v = 1001..=1030); 20 updates replace
    // v = 1..=20 with 2001..=2020; 10 deletes remove v = 51..=60.
    let expect: i64 = (1..=400).sum::<i64>() - (1..=20).sum::<i64>() + (2001..=2020).sum::<i64>()
        - (51..=60).sum::<i64>()
        + (1001..=1030).sum::<i64>();
    assert_eq!(sum, expect, "exact content through the churn");
    drop(con);
    for ext in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{}", path.display(), ext));
    }
}

// ---------------------------------------------------------------------------
// 5. Overflow chains
// ---------------------------------------------------------------------------

#[test]
fn overflow_chains_move_and_free_through_mutations() {
    let path = temp_path("ovf");
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, blob BLOB)", [])
            .unwrap();
        let big: Vec<u8> = (0..24_000u32).map(|i| (i % 251) as u8).collect();
        // Seed a couple of rows, then drive everything through
        // autocommit (the mutate path).
        for i in 1..=6i64 {
            db.execute(
                "INSERT INTO t(id, blob) VALUES (?1, ?2)",
                [Value::Integer(i), Value::Blob(big.clone())],
            )
            .unwrap();
        }
        // Update to a SMALL blob: the overflow chain must free.
        db.execute(
            "UPDATE t SET blob = ?1 WHERE id = 2",
            [Value::Blob(vec![7u8; 40])],
        )
        .unwrap();
        // Update a small back to a BIG one: a chain must (re)allocate.
        db.execute(
            "UPDATE t SET blob = ?1 WHERE id = 2",
            [Value::Blob(big.clone())],
        )
        .unwrap();
        // Grow a row repeatedly (chain extends).
        for round in 0..3i64 {
            let grown: Vec<u8> = (0..(24_000 + round * 8_000) as u32)
                .map(|i| (i % 249) as u8)
                .collect();
            db.execute("UPDATE t SET blob = ?1 WHERE id = 4", [Value::Blob(grown)])
                .unwrap();
        }
        // Delete chain-carrying rows entirely.
        db.execute("DELETE FROM t WHERE id IN (1, 3)", []).unwrap();
        let n: i64 = db.query("SELECT count(*) FROM t", []).unwrap()[0][0].as_integer();
        assert_eq!(n, 4);
        let len2: i64 = db
            .query("SELECT length(blob) FROM t WHERE id = 2", [])
            .unwrap()[0][0]
            .as_integer();
        assert_eq!(len2, 24_000);
    }
    let con = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(integrity(&con), "ok", "overflow chains through mutations");
    let n: i64 = con
        .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 4);
    let len4: i64 = con
        .query_row("SELECT length(blob) FROM t WHERE id = 4", [], |r| r.get(0))
        .unwrap();
    assert_eq!(len4, 40_000, "the thrice-grown row's chain is exact");
    drop(con);
    for ext in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{}", path.display(), ext));
    }
}

// ---------------------------------------------------------------------------
// 6. Rowid-alias moves (UPDATE ... SET id = X)
// ---------------------------------------------------------------------------

#[test]
fn rowid_alias_moves_mutate_exactly() {
    let path = temp_path("aliasmove");
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)", [])
            .unwrap();
        db.execute(
            "WITH RECURSIVE seq(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM seq WHERE i < 200) \
             INSERT INTO t(v) SELECT 'v'||i FROM seq",
            [],
        )
        .unwrap();
        // Move rows to fresh high ids (append side).
        for i in 1..=10i64 {
            db.execute(
                "UPDATE t SET id = ?1 WHERE id = ?2",
                [Value::Integer(10_000 + i), Value::Integer(i)],
            )
            .unwrap();
        }
        // Move a row onto a VACATED low id (reuse).
        db.execute("UPDATE t SET id = 3 WHERE id = 10001", [])
            .unwrap();
        // Collision: moving onto an occupied id must error and change
        // nothing on disk.
        let err = db.execute("UPDATE t SET id = 50 WHERE id = 10002", []);
        assert!(err.is_err(), "alias move onto an occupied rowid must fail");
        let n: i64 = db.query("SELECT count(*) FROM t", []).unwrap()[0][0].as_integer();
        assert_eq!(n, 200);
        let v3: String = db.query("SELECT v FROM t WHERE id = 3", []).unwrap()[0][0].as_text();
        assert_eq!(v3, "v1", "the moved-onto-3 row is the original id=1 row");
    }
    let con = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(integrity(&con), "ok");
    let n: i64 = con
        .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 200);
    let v3: String = con
        .query_row("SELECT v FROM t WHERE id = 3", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v3, "v1");
    drop(con);
    for ext in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{}", path.display(), ext));
    }
}

// ---------------------------------------------------------------------------
// 7. Mixed reopen-compare rounds
// ---------------------------------------------------------------------------

#[test]
fn mixed_autocommit_rounds_reopen_compare() {
    let path = temp_path("rounds");
    // A deterministic LCG so failures reproduce.
    let mut seed_state = 0x5eed_1234u64;
    let mut next = move |m: u64| -> u64 {
        seed_state = seed_state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed_state >> 33) % m
    };
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        db.execute(
            "CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT, b INTEGER)",
            [],
        )
        .unwrap();
        db.execute("CREATE INDEX ib ON t(b)", []).unwrap();
        db.execute("BEGIN", []).unwrap();
        db.execute(
            "WITH RECURSIVE seq(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM seq WHERE i < 500) \
             INSERT INTO t(a, b) SELECT 'a'||i, (i*48271)%500 FROM seq",
            [],
        )
        .unwrap();
        db.execute("COMMIT", []).unwrap();

        let mut expect: std::collections::HashMap<i64, (String, i64)> = (1..=500i64)
            .map(|i| (i, (format!("a{i}"), (i * 48271) % 500)))
            .collect();
        let mut next_id = 501i64;
        for round in 0..12 {
            for _ in 0..25 {
                match next(3) {
                    0 => {
                        let id = next_id;
                        next_id += 1;
                        let b = next(1000) as i64;
                        let a = format!("r{round}x{id}");
                        db.execute(
                            "INSERT INTO t(id, a, b) VALUES (?1, ?2, ?3)",
                            [
                                Value::Integer(id),
                                Value::Text(a.clone().into()),
                                Value::Integer(b),
                            ],
                        )
                        .unwrap();
                        expect.insert(id, (a, b));
                    }
                    1 if !expect.is_empty() => {
                        let keys: Vec<i64> = expect.keys().copied().collect();
                        let id = keys[next(keys.len() as u64) as usize];
                        let b = next(1000) as i64;
                        let a = format!("u{round}x{id}");
                        db.execute(
                            "UPDATE t SET a = ?1, b = ?2 WHERE id = ?3",
                            [
                                Value::Text(a.clone().into()),
                                Value::Integer(b),
                                Value::Integer(id),
                            ],
                        )
                        .unwrap();
                        expect.insert(id, (a, b));
                    }
                    _ => {
                        let keys: Vec<i64> = expect.keys().copied().collect();
                        let id = keys[next(keys.len() as u64) as usize];
                        db.execute("DELETE FROM t WHERE id = ?1", [Value::Integer(id)])
                            .unwrap();
                        expect.remove(&id);
                    }
                }
            }
            // In-memory truth.
            let n: i64 = db.query("SELECT count(*) FROM t", []).unwrap()[0][0].as_integer();
            assert_eq!(n, expect.len() as i64, "round {round}: live count");
            let sum: i64 = db.query("SELECT sum(b) FROM t", []).unwrap()[0][0].as_integer();
            let want: i64 = expect.values().map(|(_, b)| b).sum();
            assert_eq!(sum, want, "round {round}: live sum");

            // Reopen: a fresh engine must load exactly the same state.
            drop(db);
            let db2 = Database::open(&path).unwrap();
            let n2: i64 = db2.query("SELECT count(*) FROM t", []).unwrap()[0][0].as_integer();
            assert_eq!(n2, expect.len() as i64, "round {round}: reopen count");
            let sum2: i64 = db2.query("SELECT sum(b) FROM t", []).unwrap()[0][0].as_integer();
            assert_eq!(sum2, want, "round {round}: reopen sum");
            drop(db2);

            // Real SQLite verifies structure + content.
            let con = rusqlite::Connection::open(&path).unwrap();
            assert_eq!(integrity(&con), "ok", "round {round}: sqlite integrity");
            let n3: i64 = con
                .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n3, expect.len() as i64, "round {round}: sqlite count");
            let sum3: i64 = con
                .query_row("SELECT sum(b) FROM t", [], |r| r.get(0))
                .unwrap();
            assert_eq!(sum3, want, "round {round}: sqlite sum");
            drop(con);

            db = Database::open(&path).unwrap();
        }
    }
    for ext in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{}", path.display(), ext));
    }
}

// ---------------------------------------------------------------------------
// 8. Commit-frame sanity on the mutate path
// ---------------------------------------------------------------------------

#[test]
fn mutate_commits_carry_a_handful_of_frames() {
    let path = temp_path("frames");
    {
        let mut db = Database::open_sqlite_format(&path).unwrap();
        db.execute(
            "CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT, b INTEGER)",
            [],
        )
        .unwrap();
        db.execute("CREATE INDEX ib ON t(b)", []).unwrap();
        db.execute("BEGIN", []).unwrap();
        db.execute(
            "WITH RECURSIVE seq(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM seq WHERE i < 2000) \
             INSERT INTO t(a, b) SELECT 'a'||i, (i*48271)%2000 FROM seq",
            [],
        )
        .unwrap();
        db.execute("COMMIT", []).unwrap();
        // Fold the seed (clean close), then measure pure mutate commits.
        drop(db);
        let mut db = Database::open(&path).unwrap();
        for i in 1..=12i64 {
            db.execute(
                "INSERT INTO t(a, b) VALUES ('m'||?1, ?2)",
                [Value::Integer(i), Value::Integer(900_000 + i)],
            )
            .unwrap();
        }
        let wal = wal_path_of(&path);
        let frames = commits_frames(&wal);
        assert_eq!(frames.len(), 12, "one commit marker per autocommit INSERT");
        for (i, f) in frames.iter().enumerate() {
            assert!(
                *f <= 12,
                "mutate commit {i} carried {f} frames (leaf + index leaf + header; expected a handful)"
            );
        }
        let n: i64 = db.query("SELECT count(*) FROM t", []).unwrap()[0][0].as_integer();
        assert_eq!(n, 2012);
    }
    let con = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(integrity(&con), "ok");
    drop(con);
    for ext in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{}", path.display(), ext));
    }
}
