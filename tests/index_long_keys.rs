//! Long index keys: SQLite's index-cell local-size rule (`RSQLDB06`).
//!
//! Index cells used to take the TABLE rule (up to `page_size - 128` bytes
//! in-page), so a ~1-4 KB key sat inline and every index page held ONE
//! cell: a fanout-1 tree of unary interior pages whose size grew
//! quadratically (3,000 rows with 3 KB TEXT keys: 181,132 pages, 742 MB,
//! for ~2 MB of data). Index cells now cap their in-page bytes at SQLite's
//! `maxLocal` (~1/4 page, >= 4 cells per page), spilling the rest to an
//! overflow chain; such cells carry a mark on their key-length varint, so
//! a `RSQLDB05` file reads unchanged and is re-stamped `RSQLDB06` by the
//! first write of a marked cell (an older build then refuses it instead
//! of misreading the mark).
//!
//! `PRAGMA integrity_check` now also does page accounting ("Page N is
//! never used" / "2nd reference to page N"), which is how the leak in the
//! interior-separator fast path was found; every case below runs it.

use rustqlite::{Database, Value};
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn magic(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap();
    String::from_utf8_lossy(&bytes[..8]).into_owned()
}

fn int(db: &Database, sql: &str) -> i64 {
    db.query(sql, []).unwrap()[0][0].as_integer()
}

fn integrity(db: &Database) -> Vec<String> {
    db.query("PRAGMA integrity_check", [])
        .unwrap()
        .iter()
        .map(|r| r[0].as_text().to_string())
        .collect()
}

fn assert_ok(db: &Database, what: &str) {
    assert_eq!(integrity(db), vec!["ok".to_string()], "{what}");
}

/// The fixture's rows (written by the pre-06 engine): 33 rounds over key
/// lengths straddling every rule boundary, ids `id % 7 == 3` deleted.
const LENS: [usize; 11] = [
    10, 600, 1003, 1500, 2500, 3900, 3968, 3969, 5000, 9000, 20000,
];

fn title(round: usize) -> String {
    format!("{:04}{}", round, "t".repeat(LENS[round % LENS.len()]))
}

fn stage_v5(dir: &tempfile::TempDir) -> PathBuf {
    let dst = dir.path().join("v5.db");
    std::fs::copy(fixture("v5_long_index_keys.db"), &dst).unwrap();
    dst
}

/// Problems a file written by the pre-06 engine legitimately carries:
/// pages that engine orphaned (its separator fast path leaked chains, and
/// every reopen rebuilt the WITHOUT ROWID PK index into fresh pages).
/// Page accounting reports them; VACUUM reclaims them.
fn only_legacy_orphans(db: &Database) -> bool {
    integrity(db)
        .iter()
        .all(|m| m == "ok" || (m.starts_with("Page ") && m.ends_with(" is never used")))
}

#[test]
fn v5_file_opens_reads_and_upgrades_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = stage_v5(&dir);
    assert_eq!(magic(&path), "RSQLDB05");
    {
        let db = Database::open(&path).unwrap();
        assert!(only_legacy_orphans(&db), "{:?}", integrity(&db));
        let rows = LENS.len() * 3;
        let kept = (1..=rows as i64).filter(|id| id % 7 != 3).count() as i64;
        assert_eq!(int(&db, "SELECT count(*) FROM doc"), kept);
        assert_eq!(int(&db, "SELECT count(*) FROM kv"), rows as i64);
        // Exact lookups through every key-length class, via the index.
        for round in 0..rows {
            let id = round as i64 + 1;
            let n = db
                .query(
                    "SELECT id FROM doc INDEXED BY doc_title WHERE title = ?1",
                    [Value::Text(title(round).into())],
                )
                .unwrap();
            let expect: Vec<i64> = if id % 7 == 3 { vec![] } else { vec![id] };
            assert_eq!(
                n.iter().map(|r| r[0].as_integer()).collect::<Vec<_>>(),
                expect,
                "round {round}"
            );
        }
        // Ordered index walk returns keys in order.
        let ordered = db
            .query(
                "SELECT title FROM doc INDEXED BY doc_title ORDER BY title",
                [],
            )
            .unwrap();
        let got: Vec<String> = ordered.iter().map(|r| r[0].as_text().to_string()).collect();
        let mut sorted = got.clone();
        sorted.sort();
        assert_eq!(got, sorted);
        // The hidden WITHOUT ROWID PK row never shows in the SQL schema.
        let master: Vec<String> = db
            .query(
                "SELECT type || ':' || name FROM sqlite_master ORDER BY 1",
                [],
            )
            .unwrap()
            .iter()
            .map(|r| r[0].as_text().to_string())
            .collect();
        assert_eq!(
            master,
            vec!["index:doc_body", "index:doc_title", "table:doc", "table:kv"]
        );
    }
    // The first open recorded the WITHOUT ROWID PK index (one-time
    // rebuild) and re-stamped the file; later opens ADOPT it — the file
    // stops growing.
    assert_eq!(magic(&path), "RSQLDB06");
    let size_after_first_open = std::fs::metadata(&path).unwrap().len();
    for _ in 0..3 {
        let db = Database::open(&path).unwrap();
        assert!(only_legacy_orphans(&db));
        drop(db);
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            size_after_first_open
        );
    }
    let mut db = Database::open(&path).unwrap();
    db.execute("VACUUM", []).unwrap();
    assert_ok(&db, "after VACUUM the legacy orphans are reclaimed");
}

#[test]
fn v5_file_mixes_legacy_and_marked_cells() {
    let dir = tempfile::tempdir().unwrap();
    let path = stage_v5(&dir);
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("VACUUM", []).unwrap();
        assert_ok(&db, "vacuumed fixture");
        // New long keys (marked cells) interleave with legacy cells.
        for i in 0..200i64 {
            db.execute(
                "INSERT INTO doc VALUES (?1, ?2, ?3)",
                vec![
                    Value::Integer(2000 + i),
                    Value::Text(
                        format!("{:04}{}", i, "n".repeat(1200 + (i as usize * 37) % 4000)).into(),
                    ),
                    Value::Text(format!("body{i}{}", "q".repeat(1500)).into()),
                ],
            )
            .unwrap();
        }
        assert_eq!(magic(&path), "RSQLDB06");
        assert_ok(&db, "legacy + marked cells after splits");
        // Unique enforcement across legacy and new cells.
        let dup = db.execute(
            "INSERT INTO doc VALUES (5000, 'dup', ?1)",
            [Value::Text(format!("body7{}", "q".repeat(1500)).into())],
        );
        assert!(dup.is_err(), "UNIQUE over a marked key must hold");
        // Delete a mix (legacy rows by id, new rows by range) and VACUUM.
        db.execute("DELETE FROM doc WHERE id % 3 = 0", []).unwrap();
        db.execute("DELETE FROM kv WHERE v % 4 = 1", []).unwrap();
        assert_ok(&db, "after mixed deletes");
        db.execute("VACUUM", []).unwrap();
        assert_ok(&db, "after VACUUM");
    }
    let db = Database::open(&path).unwrap();
    assert_eq!(magic(&path), "RSQLDB06");
    assert_ok(&db, "reopened");
    for i in [1i64, 50, 199] {
        if (2000 + i) % 3 == 0 {
            continue;
        }
        let t = format!("{:04}{}", i, "n".repeat(1200 + (i as usize * 37) % 4000));
        let n = int(
            &db,
            &format!("SELECT count(*) FROM doc WHERE title = '{}'", t),
        );
        assert_eq!(n, 1, "marked key {i} findable after reopen");
    }
}

/// Reopening a database with a WITHOUT ROWID table adopts its PK index
/// (persisted root): the file does not grow per open and the index stays
/// page-exact — the old engine rebuilt it into fresh pages on EVERY open.
#[test]
fn without_rowid_reopen_does_not_grow() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wr.db");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute(
            "CREATE TABLE kv(k TEXT PRIMARY KEY, v INTEGER) WITHOUT ROWID",
            [],
        )
        .unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 0..5000i64 {
            db.execute(
                "INSERT INTO kv VALUES (?1, ?2)",
                vec![
                    Value::Text(format!("key{:06}", (i * 7919) % 5000).into()),
                    Value::Integer(i),
                ],
            )
            .unwrap();
        }
        db.execute("COMMIT", []).unwrap();
    }
    let size = std::fs::metadata(&path).unwrap().len();
    for round in 0..4 {
        let mut db = Database::open(&path).unwrap();
        assert_ok(&db, &format!("reopen {round}"));
        assert!(
            db.execute("INSERT INTO kv VALUES ('key000005', 0)", [])
                .is_err(),
            "PK uniqueness enforced through the adopted index"
        );
        assert_eq!(int(&db, "SELECT v FROM kv WHERE k = 'key001234'"), {
            // (i * 7919) % 5000 == 1234
            (0..5000i64).find(|i| (i * 7919) % 5000 == 1234).unwrap()
        });
        drop(db);
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            size,
            "round {round}"
        );
    }
}

/// The shape that produced the 742 MB file: 3,000 rows, every fifth with a
/// 3,000-character indexed TEXT key. Page count must track the data
/// (SQLite's file for the same rows is the reference scale).
#[test]
fn long_text_index_stays_compact() {
    let mut db = Database::open_in_memory().unwrap();
    let lite = rusqlite::Connection::open_in_memory().unwrap();
    db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    lite.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)")
        .unwrap();
    db.execute("BEGIN", []).unwrap();
    lite.execute_batch("BEGIN").unwrap();
    for i in 0..3000i64 {
        let v = if i % 5 == 0 {
            format!("{i:05}{}", "x".repeat(3000))
        } else {
            format!("v{i}")
        };
        db.execute(
            "INSERT INTO t VALUES (?1, ?2)",
            vec![Value::Integer(i), Value::Text(v.clone().into())],
        )
        .unwrap();
        lite.execute("INSERT INTO t VALUES (?1, ?2)", rusqlite::params![i, v])
            .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    lite.execute_batch("COMMIT").unwrap();
    db.execute("CREATE INDEX t_v ON t(v)", []).unwrap();
    lite.execute_batch("CREATE INDEX t_v ON t(v)").unwrap();
    let ours = db.page_count() as i64;
    let theirs: i64 = lite
        .query_row("PRAGMA page_count", [], |r| r.get(0))
        .unwrap();
    assert!(
        ours <= theirs * 3 / 2,
        "index on long keys: {ours} pages vs SQLite's {theirs}"
    );
    assert_ok(&db, "long-key index");
    assert_eq!(
        int(&db, "SELECT count(*) FROM t WHERE v >= '00000' AND v < 'v'"),
        600
    );
}

/// Every key length around the cell-size boundaries, inserted in
/// shuffled order with deletes interleaved: the tree stays consistent and
/// page-exact (no leaked separator chains) at each step.
#[test]
fn boundary_lengths_churn_page_exact() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, k TEXT)", [])
        .unwrap();
    db.execute("CREATE INDEX t_k ON t(k)", []).unwrap();
    let lens = [
        1usize, 900, 1000, 1001, 1002, 1003, 1100, 2000, 3967, 3968, 3969, 4100, 8200,
    ];
    let mut seed = 0x9e3779b97f4a7c15u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for step in 0..900i64 {
        let len = lens[(next() % lens.len() as u64) as usize];
        let key = format!("{:06}{}", next() % 1_000_000, "k".repeat(len));
        db.execute("INSERT INTO t(k) VALUES (?1)", [Value::Text(key.into())])
            .unwrap();
        if step % 5 == 4 {
            db.execute(
                "DELETE FROM t WHERE id = (SELECT id FROM t ORDER BY k LIMIT 1 OFFSET ?1)",
                [Value::Integer((next() % 50) as i64)],
            )
            .unwrap();
        }
        if step % 150 == 149 {
            assert_ok(&db, &format!("step {step}"));
        }
    }
    let n = int(&db, "SELECT count(*) FROM t");
    let via_index = int(&db, "SELECT count(*) FROM t INDEXED BY t_k WHERE k > ''");
    assert_eq!(n, via_index);
}
