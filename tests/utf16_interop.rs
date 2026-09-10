//! UTF-16 SQLite-file interop: read, write, round-trip.
//!
//! rusqlite (bundled real SQLite) plays BOTH roles: it creates reference
//! UTF-16le / UTF-16be databases, and it verifies the files this engine
//! writes — `PRAGMA integrity_check`, `PRAGMA encoding`, index equality
//! lookups (which binary-search the b-trees we built, proving our index
//! key ORDER matches SQLite's comparator), and ORDER BY parity.
//!
//! Byte-order semantics baked into these tests (measured against the
//! bundled 3.46): SQLite's BINARY collation compares the raw bytes of a
//! value IN THE FILE'S ENCODING — UTF-16le files order by little-endian
//! unit bytes (NOT code-point order), UTF-16be files by code units. The
//! writer must sort index keys and WITHOUT ROWID records exactly so.
//! NOCASE folds ASCII and compares in UTF-8 space regardless of
//! encoding, so it is order-stable across encodings.

use rustqlite::{Database, Value};
use std::path::PathBuf;

fn temp_path(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("rsql_utf16_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(rustqlite::storage::sqlitefmt::reader::wal_path_of(&p));
    p
}

fn integrity_check(con: &rusqlite::Connection) -> String {
    con.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
        .unwrap()
}

/// i64 out of a single-row single-column result.
fn count_of(db: &Database, sql: &str) -> i64 {
    match &db.query(sql, ()).unwrap()[0][0] {
        Value::Integer(n) => *n,
        other => panic!("expected INTEGER, got {other:?}"),
    }
}

/// Adversarial text set: exposes every divergence between UTF-8
/// code-point order, UTF-16 code-unit order, and raw little-endian byte
/// order (see the module doc).
const ADVERSARIAL: [&str; 8] = [
    "\u{61}",    // "a"          UTF-16LE: 61 00
    "\u{6100}",  //              UTF-16LE: 00 61
    "z",         //
    "\u{E000}",  // private use  (high BMP, above surrogates)
    "\u{FFFD}",  // replacement  (high BMP)
    "\u{1F600}", // emoji        (astral: D83D DE00)
    "\u{10000}", // astral low   (D800 DC00)
    "aa",        // prefix-length tie-break
];

fn texts_of(rows: &[Vec<Value>]) -> Vec<String> {
    rows.iter()
        .map(|r| match &r[0] {
            Value::Text(t) => t.as_str().to_string(),
            other => panic!("expected TEXT, got {other:?}"),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Direction A: SQLite creates a UTF-16 database; rustqlite opens, reads,
// writes; SQLite verifies the result.
// ---------------------------------------------------------------------------

#[test]
fn sqlite_utf16_file_reads_and_writes_back() {
    for pragma_name in ["UTF-16le", "UTF-16be"] {
        let path = temp_path(&format!("a_rw_{pragma_name}"));
        {
            let con = rusqlite::Connection::open(&path).unwrap();
            con.pragma_update(None, "encoding", pragma_name).unwrap();
            let got: String = con.query_row("PRAGMA encoding", [], |r| r.get(0)).unwrap();
            assert_eq!(got, pragma_name, "header should report {pragma_name}");
            con.execute_batch(
                "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT, b BLOB);
                 CREATE INDEX ix ON t(v);
                 -- big: no index — records exercise UTF-16 overflow chains.
                 CREATE TABLE big(id INTEGER PRIMARY KEY, v TEXT, b BLOB);",
            )
            .unwrap();
            {
                let mut stmt = con.prepare("INSERT INTO t VALUES (?, ?, ?)").unwrap();
                for (i, s) in ADVERSARIAL.iter().enumerate() {
                    stmt.execute(rusqlite::params![(i + 1) as i64, s, s.as_bytes()])
                        .unwrap();
                }
            }
        }

        // rustqlite opens, reads (values + PRAGMA encoding), adds rows.
        {
            let mut db = Database::open(&path).unwrap();
            assert_eq!(db.disk_format(), "sqlite");
            let rows = db.query("SELECT v FROM t ORDER BY id", ()).unwrap();
            let texts = texts_of(&rows);
            assert_eq!(texts, ADVERSARIAL, "values must round-trip losslessly");
            let pragma_rows = db.query("PRAGMA encoding", ()).unwrap();
            assert_eq!(
                pragma_rows[0][0],
                Value::Text(pragma_name.into()),
                "PRAGMA encoding must report the file's encoding"
            );
            // Equality lookups by value (encoding-independent).
            for s in ADVERSARIAL {
                let sql = format!(
                    "SELECT COUNT(*) FROM t WHERE v = '{}'",
                    s.replace('\'', "''")
                );
                assert_eq!(count_of(&db, &sql), 1, "lookup {s:?}");
            }
            // Writes: new adversarial rows + a long overflow text (into
            // the no-index table: the engine's in-memory index keys are
            // page-bounded, while the record path supports overflow).
            let long: String = "\u{1F600}w\u{00F6}rterb\u{00FC}ch\u{E000}".repeat(6000); // ~90 KB UTF-16
            db.execute(
                "INSERT INTO big(id, v, b) VALUES (100, ?, ?)",
                vec![
                    Value::Text(long.clone().into()),
                    Value::Blob(long.as_bytes().to_vec()),
                ],
            )
            .unwrap();
            let back = db.query("SELECT v FROM big WHERE id = 100", ()).unwrap()[0][0].clone();
            assert_eq!(back, Value::Text(long.into()));
        }

        // SQLite verifies: integrity, encoding preserved, data readable,
        // index lookups over the ENGINE-written b-tree still resolve.
        {
            let con = rusqlite::Connection::open(&path).unwrap();
            let enc: String = con.query_row("PRAGMA encoding", [], |r| r.get(0)).unwrap();
            assert_eq!(enc, pragma_name, "encoding must survive the rewrite");
            assert_eq!(integrity_check(&con), "ok");
            for s in ADVERSARIAL {
                let n: i64 = con
                    .query_row("SELECT COUNT(*) FROM t WHERE v = ?", [s], |r| r.get(0))
                    .unwrap();
                assert_eq!(n, 1, "SQLite lookup into engine index: {s:?}");
            }
            let n: i64 = con
                .query_row("SELECT COUNT(*) FROM big WHERE v IS NOT NULL", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(n, 1, "engine-written overflow row is readable");
            let back: String = con
                .query_row("SELECT v FROM big WHERE id = 100", [], |r| r.get(0))
                .unwrap();
            assert_eq!(
                back,
                "\u{1F600}w\u{00F6}rterb\u{00FC}ch\u{E000}".repeat(6000),
                "long UTF-16 text must round-trip through overflow chains"
            );
        }
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn sqlite_utf16_nocase_index_and_without_rowid() {
    let path = temp_path("a_nocase_worowid");
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        con.pragma_update(None, "encoding", "UTF-16le").unwrap();
        con.execute_batch(
            "CREATE TABLE nc(v TEXT COLLATE NOCASE);
             CREATE INDEX ncix ON nc(v);
             CREATE TABLE w(k TEXT PRIMARY KEY, n INT) WITHOUT ROWID;",
        )
        .unwrap();
        {
            let mut stmt = con.prepare("INSERT INTO nc VALUES (?)").unwrap();
            for s in ["Z", "a", "A", "\u{6100}", "\u{1F600}", "b"] {
                stmt.execute([s]).unwrap();
            }
        }
        {
            let mut stmt = con.prepare("INSERT INTO w VALUES (?, ?)").unwrap();
            for (i, s) in ADVERSARIAL.iter().enumerate() {
                stmt.execute(rusqlite::params![s, (i + 1) as i64]).unwrap();
            }
        }
    }

    // rustqlite reads, then writes new keys through both structures.
    {
        let mut db = Database::open(&path).unwrap();
        assert_eq!(count_of(&db, "SELECT COUNT(*) FROM nc WHERE v = 'a'"), 2);
        db.execute("INSERT INTO nc VALUES ('B')", ()).unwrap();
        db.execute("INSERT INTO w VALUES ('\u{5D000}', 99)", ())
            .unwrap();
        assert_eq!(
            count_of(&db, "SELECT COUNT(*) FROM w"),
            (ADVERSARIAL.len() + 1) as i64
        );
    }

    // SQLite verifies: integrity, NOCASE lookups, WITHOUT ROWID lookups.
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(integrity_check(&con), "ok");
        let n: i64 = con
            .query_row("SELECT COUNT(*) FROM nc WHERE v = 'b'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "NOCASE lookup incl. engine-written 'B'");
        for s in ADVERSARIAL {
            let n: i64 = con
                .query_row("SELECT n FROM w WHERE k = ?", [s], |r| r.get(0))
                .unwrap();
            assert!(n >= 1, "WITHOUT ROWID lookup {s:?}");
        }
        let n: i64 = con
            .query_row("SELECT n FROM w WHERE k = '\u{5D000}'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 99, "engine-written WITHOUT ROWID key");
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn sqlite_utf16_schema_text_roundtrip() {
    let path = temp_path("a_schema_text");
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        con.pragma_update(None, "encoding", "UTF-16be").unwrap();
        con.execute_batch(
            "CREATE TABLE \"t\u{00EB}bl\u{00EB}\"(id INTEGER PRIMARY KEY, \"w\u{00F6}rt\" TEXT);
             INSERT INTO \"t\u{00EB}bl\u{00EB}\" VALUES (1, 'w\u{00F6}rter');
             CREATE VIEW vw AS SELECT * FROM \"t\u{00EB}bl\u{00EB}\";",
        )
        .unwrap();
    }
    {
        // Non-ASCII identifiers must survive the load (schema text is
        // stored in the FILE encoding).
        let mut db = Database::open(&path).unwrap();
        let rows = db
            .query("SELECT \"w\u{00F6}rt\" FROM \"t\u{00EB}bl\u{00EB}\"", ())
            .unwrap();
        assert_eq!(rows[0][0], Value::Text("w\u{00F6}rter".into()));
        let rows = db.query("SELECT * FROM vw", ()).unwrap();
        assert_eq!(rows.len(), 1);
        db.execute("INSERT INTO \"t\u{00EB}bl\u{00EB}\" VALUES (2, 'mehr')", ())
            .unwrap();
    }
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(integrity_check(&con), "ok");
        let n: i64 = con
            .query_row("SELECT COUNT(*) FROM \"t\u{00EB}bl\u{00EB}\"", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 2);
        let sql: String = con
            .query_row("SELECT sql FROM sqlite_master WHERE name = 'vw'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            sql.contains("t\u{00EB}bl\u{00EB}"),
            "schema SQL text must round-trip: {sql:?}"
        );
    }
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// Direction B: the engine CREATES a UTF-16 database (PRAGMA encoding
// before first content); SQLite opens and verifies everything.
// ---------------------------------------------------------------------------

#[test]
fn engine_creates_utf16_file_verified_by_sqlite() {
    for pragma_name in ["UTF-16le", "UTF-16be"] {
        let path = temp_path(&format!("b_create_{pragma_name}"));
        {
            let mut db = Database::open_sqlite_format(&path).unwrap();
            db.execute("PRAGMA encoding = ?", [Value::Text(pragma_name.into())])
                .unwrap();
            let reported = db.query("PRAGMA encoding", ()).unwrap();
            assert_eq!(reported[0][0], Value::Text(pragma_name.into()));
            db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)", ())
                .unwrap();
            db.execute("CREATE INDEX ix ON t(v)", ()).unwrap();
            db.execute(
                "CREATE TABLE w(k TEXT PRIMARY KEY, n INT) WITHOUT ROWID",
                (),
            )
            .unwrap();
            for (i, s) in ADVERSARIAL.iter().enumerate() {
                db.execute(
                    "INSERT INTO t VALUES (?, ?)",
                    [Value::Integer((i + 1) as i64), Value::Text((*s).into())],
                )
                .unwrap();
                db.execute(
                    "INSERT INTO w VALUES (?, ?)",
                    [Value::Text((*s).into()), Value::Integer((i + 1) as i64)],
                )
                .unwrap();
            }
        }

        // SQLite opens the engine's file: encoding, integrity, index
        // binary-search (equality) and WITHOUT ROWID lookups.
        {
            let con = rusqlite::Connection::open(&path).unwrap();
            let enc: String = con.query_row("PRAGMA encoding", [], |r| r.get(0)).unwrap();
            assert_eq!(enc, pragma_name, "engine must write header field 56");
            assert_eq!(integrity_check(&con), "ok");
            for s in ADVERSARIAL {
                // A miss here means our index-key order disagrees with
                // SQLite's encoding-aware BINARY comparator.
                let n: i64 = con
                    .query_row("SELECT COUNT(*) FROM t WHERE v = ?", [s], |r| r.get(0))
                    .unwrap();
                assert_eq!(n, 1, "SQLite binary-search in engine index: {s:?}");
                let k: i64 = con
                    .query_row("SELECT n FROM w WHERE k = ?", [s], |r| r.get(0))
                    .unwrap();
                assert!(k >= 1, "WITHOUT ROWID lookup {s:?}");
            }
        }

        // rustqlite re-opens its own UTF-16 file (writer->reader loop).
        {
            let db = Database::open(&path).unwrap();
            let rows = db.query("SELECT v FROM t ORDER BY id", ()).unwrap();
            assert_eq!(texts_of(&rows), ADVERSARIAL);
        }
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn engine_utf16_order_by_matches_sqlite_byte_order() {
    // SQLite's on a UTF-16 file is raw-encoding-byte order — and the
    // ENGINE's own ORDER BY now follows the same comparator (BINARY
    // text pairs compare the connection's file encoding — see
    // executor::conn_enc), not code-point order. The FILE's b-tree
    // order must follow SQLITE's comparator too — so ORDER BY through
    // SQLite's index scan on the engine-created file must equal
    // SQLite's own reference order for the same data.
    let data = ADVERSARIAL;
    for pragma_name in ["UTF-16le", "UTF-16be"] {
        let engine_path = temp_path(&format!("b_order_{pragma_name}"));
        let sqlite_path = temp_path(&format!("b_order_ref_{pragma_name}"));

        // Reference: SQLite's own UTF-16 database + ORDER BY.
        let reference: Vec<String> = {
            let con = rusqlite::Connection::open(&sqlite_path).unwrap();
            con.pragma_update(None, "encoding", pragma_name).unwrap();
            con.execute_batch("CREATE TABLE t(v TEXT); CREATE INDEX ix ON t(v);")
                .unwrap();
            {
                let mut stmt = con.prepare("INSERT INTO t VALUES (?)").unwrap();
                for s in data {
                    stmt.execute([s]).unwrap();
                }
            }
            let mut stmt = con.prepare("SELECT v FROM t ORDER BY v").unwrap();
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };

        // Engine-created file with the same data.
        {
            let mut db = Database::open_sqlite_format(&engine_path).unwrap();
            db.execute("PRAGMA encoding = ?", [Value::Text(pragma_name.into())])
                .unwrap();
            db.execute("CREATE TABLE t(v TEXT)", ()).unwrap();
            db.execute("CREATE INDEX ix ON t(v)", ()).unwrap();
            for s in data {
                db.execute("INSERT INTO t VALUES (?)", [Value::Text(s.into())])
                    .unwrap();
            }
            // THE ENGINE's OWN comparator (reopened: the pragma-era
            // connection also works, reopen proves the header's encoding
            // feeds the ordering).
            let engine_order: Vec<String> = {
                let db = Database::open(&engine_path).unwrap();
                let rows = db.query("SELECT v FROM t ORDER BY v", ()).unwrap();
                rows.iter()
                    .map(|r| match &r[0] {
                        Value::Text(t) => t.as_str().to_string(),
                        other => format!("{other:?}"),
                    })
                    .collect()
            };
            assert_eq!(
                engine_order, reference,
                "{pragma_name}: the ENGINE's ORDER BY must follow SQLite's byte order"
            );
        }

        // SQLite sorts the ENGINE's file: the same order as its own.
        {
            let con = rusqlite::Connection::open(&engine_path).unwrap();
            assert_eq!(integrity_check(&con), "ok");
            let mut stmt = con.prepare("SELECT v FROM t ORDER BY v").unwrap();
            let got: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            assert_eq!(
                got, reference,
                "{pragma_name}: index order must match SQLite's comparator"
            );
        }

        // Sanity: the LE/BE orders genuinely differ from code-point order
        // (the adversarial set is doing its job).
        let code_point: Vec<String> = {
            let mut v: Vec<String> = data.iter().map(|s| s.to_string()).collect();
            v.sort();
            v
        };
        assert_ne!(
            reference, code_point,
            "adversarial set must expose the encoding order divergence"
        );

        let _ = std::fs::remove_file(&engine_path);
        let _ = std::fs::remove_file(&sqlite_path);
    }
}

#[test]
fn engine_utf16_min_max_cast_match_sqlite() {
    // Aggregate min()/max(), the scalar forms, DESC ORDER BY and
    // CAST(v AS BLOB) — all compare/yield the FILE encoding's bytes on
    // a UTF-16 file; the engine must match real SQLite exactly.
    let data = ADVERSARIAL;
    for pragma_name in ["UTF-16le", "UTF-16be"] {
        let path = temp_path(&format!("b_mmc_{pragma_name}"));
        {
            let con = rusqlite::Connection::open(&path).unwrap();
            con.pragma_update(None, "encoding", pragma_name).unwrap();
            con.execute_batch("CREATE TABLE t(v TEXT);").unwrap();
            let mut stmt = con.prepare("INSERT INTO t VALUES (?)").unwrap();
            for s in data {
                stmt.execute([s]).unwrap();
            }
        }
        // SQLite's answers.
        let (sq_min, sq_max, sq_desc, sq_cast): (String, String, Vec<String>, Vec<Vec<u8>>) = {
            let con = rusqlite::Connection::open(&path).unwrap();
            let mn: String = con
                .query_row("SELECT min(v) FROM t", [], |r| r.get(0))
                .unwrap();
            let mx: String = con
                .query_row("SELECT max(v) FROM t", [], |r| r.get(0))
                .unwrap();
            let mut stmt = con.prepare("SELECT v FROM t ORDER BY v DESC").unwrap();
            let desc: Vec<String> = stmt
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            let mut stmt = con
                .prepare("SELECT hex(CAST(v AS BLOB)) FROM t ORDER BY v")
                .unwrap();
            let casts: Vec<Vec<u8>> = stmt
                .query_map([], |r| {
                    let h: String = r.get(0).unwrap();
                    Ok((0..h.len() / 2)
                        .map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap())
                        .collect::<Vec<u8>>())
                })
                .unwrap()
                .map(|r: Result<Vec<u8>, rusqlite::Error>| r.unwrap())
                .collect();
            (mn, mx, desc, casts)
        };
        // The engine's answers on the same file.
        let (e_min, e_max, e_desc, e_cast): (String, String, Vec<String>, Vec<Vec<u8>>) = {
            let db = Database::open(&path).unwrap();
            let q1 = db.query("SELECT min(v) FROM t", ()).unwrap();
            let q2 = db.query("SELECT max(v) FROM t", ()).unwrap();
            let q3 = db.query("SELECT v FROM t ORDER BY v DESC", ()).unwrap();
            let q4 = db
                .query("SELECT CAST(v AS BLOB) FROM t ORDER BY v", ())
                .unwrap();
            let as_str = |v: &Value| match v {
                Value::Text(t) => t.as_str().to_string(),
                other => format!("{other:?}"),
            };
            (
                as_str(&q1[0][0]),
                as_str(&q2[0][0]),
                q3.iter().map(|r| as_str(&r[0])).collect(),
                q4.iter()
                    .map(|r| match &r[0] {
                        Value::Blob(b) => b.clone(),
                        other => format!("{other:?}").into_bytes(),
                    })
                    .collect(),
            )
        };
        assert_eq!(e_min, sq_min, "{pragma_name}: min()");
        assert_eq!(e_max, sq_max, "{pragma_name}: max()");
        assert_eq!(e_desc, sq_desc, "{pragma_name}: ORDER BY DESC");
        assert_eq!(e_cast, sq_cast, "{pragma_name}: CAST(v AS BLOB) bytes");
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn engine_utf16_parallel_sort_serial_equality() {
    // The parallel ORDER BY worker split must stay bit-identical to the
    // serial sort under UTF-16 byte-order comparisons: worker threads
    // re-install the connection's encoding tag (TLS does not cross
    // threads) — a regression here means a worker sorted code-point
    // order while the main-thread merge compared UTF-16 order.
    let path = temp_path("b_par_sort");
    let n = 60_000; // above the parallel-sort threshold
    {
        let mut con = rusqlite::Connection::open(&path).unwrap();
        con.pragma_update(None, "encoding", "UTF-16le").unwrap();
        con.execute_batch("CREATE TABLE t(v TEXT);").unwrap();
        // One transaction: 60k per-row autocommit fsyncs would run for
        // many minutes on CI's Windows runners (~100ms per fsync there).
        let tx = con.transaction().unwrap();
        {
            let mut stmt = tx.prepare("INSERT INTO t VALUES (?)").unwrap();
            // Mixed-script adversarial values cycling with distinct rowids.
            for i in 0..n {
                let v = format!("{}{}", ADVERSARIAL[i % ADVERSARIAL.len()], i);
                stmt.execute([v]).unwrap();
            }
        }
        tx.commit().unwrap();
    }
    let db = Database::open(&path).unwrap();
    // Parallel path (bare-column ORDER BY over a full scan, no predicate).
    let parallel = db.query("SELECT v FROM t ORDER BY v", ()).unwrap();
    // Oracle: SQLite's own order for the same file.
    let reference: Vec<String> = {
        let con = rusqlite::Connection::open(&path).unwrap();
        let mut stmt = con.prepare("SELECT v FROM t ORDER BY v").unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    let got: Vec<String> = parallel
        .iter()
        .map(|r| match &r[0] {
            Value::Text(t) => t.as_str().to_string(),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(got, reference, "parallel UTF-16 ORDER BY must equal SQLite");
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// PRAGMA encoding semantics (probed against bundled 3.46)
// ---------------------------------------------------------------------------

#[test]
fn pragma_encoding_after_content_is_silently_ignored() {
    let path = temp_path("p_ignore");
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        con.pragma_update(None, "encoding", "UTF-16le").unwrap();
        con.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1);")
            .unwrap();
    }
    {
        let mut db = Database::open(&path).unwrap();
        // Write form: accepted, ignored (SQLite returns Ok too).
        db.execute("PRAGMA encoding = 'UTF-8'", ()).unwrap();
        let reported = db.query("PRAGMA encoding", ()).unwrap();
        assert_eq!(reported[0][0], Value::Text("UTF-16le".into()));
        // Unknown value: silently ignored (SQLite behavior).
        db.execute("PRAGMA encoding = 'CP1252'", ()).unwrap();
        let reported = db.query("PRAGMA encoding", ()).unwrap();
        assert_eq!(reported[0][0], Value::Text("UTF-16le".into()));
        // A write dumps — encoding must remain UTF-16le on disk.
        db.execute("INSERT INTO t VALUES (2)", ()).unwrap();
    }
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        let enc: String = con.query_row("PRAGMA encoding", [], |r| r.get(0)).unwrap();
        assert_eq!(enc, "UTF-16le");
        assert_eq!(integrity_check(&con), "ok");
        let n: i64 = con
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn pragma_encoding_switches_only_while_empty() {
    let path = temp_path("p_empty_switch");
    {
        // Empty interop db: the encoding is settable (effective at the
        // next dump).
        let mut db = Database::open_sqlite_format(&path).unwrap();
        db.execute("PRAGMA encoding = 'UTF-16be'", ()).unwrap();
        db.execute("CREATE TABLE t(x)", ()).unwrap();
        db.execute("INSERT INTO t VALUES ('a')", ()).unwrap();
    }
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        let enc: String = con.query_row("PRAGMA encoding", [], |r| r.get(0)).unwrap();
        assert_eq!(enc, "UTF-16be", "set-while-empty must take effect");
        assert_eq!(integrity_check(&con), "ok");
    }
    // After content: further switches are ignored, even after the
    // content is dropped (the header was materialized).
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("DROP TABLE t", ()).unwrap();
        db.execute("PRAGMA encoding = 'UTF-8'", ()).unwrap();
        let reported = db.query("PRAGMA encoding", ()).unwrap();
        assert_eq!(reported[0][0], Value::Text("UTF-16be".into()));
    }
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        let enc: String = con.query_row("PRAGMA encoding", [], |r| r.get(0)).unwrap();
        assert_eq!(enc, "UTF-16be");
        assert_eq!(integrity_check(&con), "ok");
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn pragma_encoding_reports_utf8_for_native_and_memory() {
    let mem = Database::open_in_memory().unwrap();
    let reported = mem.query("PRAGMA encoding", ()).unwrap();
    assert_eq!(reported[0][0], Value::Text("UTF-8".into()));
    // Native files: assignment is a no-op, the report stays UTF-8.
    let path = temp_path("p_native");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("PRAGMA encoding = 'UTF-16le'", ()).unwrap();
        let reported = db.query("PRAGMA encoding", ()).unwrap();
        assert_eq!(reported[0][0], Value::Text("UTF-8".into()));
        db.execute("CREATE TABLE t(x)", ()).unwrap();
        db.execute("INSERT INTO t VALUES (1)", ()).unwrap();
    }
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// Value-level codec checks (unit-ish, through a real file)
// ---------------------------------------------------------------------------

#[test]
fn utf16_value_type_matrix_roundtrip() {
    let path = temp_path("v_matrix");
    {
        let con = rusqlite::Connection::open(&path).unwrap();
        con.pragma_update(None, "encoding", "UTF-16le").unwrap();
        con.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY, v)")
            .unwrap();
        {
            let mut stmt = con.prepare("INSERT INTO t VALUES (?, ?)").unwrap();
            stmt.execute(rusqlite::params![1, 42i64]).unwrap();
            stmt.execute(rusqlite::params![2, -9223372036854775808i64])
                .unwrap();
            stmt.execute(rusqlite::params![3, 3.5f64]).unwrap();
            stmt.execute(rusqlite::params![4, Option::<String>::None])
                .unwrap();
            stmt.execute(rusqlite::params![5, "a\u{1F600}b"]).unwrap();
            stmt.execute(rusqlite::params![6, vec![0x00u8, 0xFF]])
                .unwrap();
            stmt.execute(rusqlite::params![7, 2.0f64]).unwrap();
        }
    }
    {
        let db = Database::open(&path).unwrap();
        let rows = db.query("SELECT v FROM t ORDER BY id", ()).unwrap();
        assert_eq!(rows[0][0], Value::Integer(42));
        assert_eq!(rows[1][0], Value::Integer(i64::MIN));
        assert_eq!(rows[2][0], Value::Real(3.5));
        assert_eq!(rows[3][0], Value::Null);
        assert_eq!(rows[4][0], Value::Text("a\u{1F600}b".into()));
        assert_eq!(rows[5][0], Value::Blob(vec![0x00, 0xFF]));
        assert_eq!(rows[6][0], Value::Real(2.0));
    }
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// WHERE range comparisons under the file's byte order (the sub-gap this
// suite pins): v > 'x', BETWEEN, and DML WHERE ranges on TEXT columns
// must match real SQLite exactly on UTF-16 files — including a query
// with an INDEX on the ranged column (the engine declines the index
// range plan there; the scan filter evaluates byte-ordered).
// ---------------------------------------------------------------------------

#[test]
fn engine_utf16_where_ranges_match_sqlite() {
    let data = ADVERSARIAL;
    let predicates = [
        "v > 'a'",
        "v >= 'a'",
        "v < 'z'",
        "v <= 'z'",
        "v > '\u{FFFD}'",
        "v < '\u{FFFD}'",
        "v >= '\u{1F600}'",
        "v <= '\u{E000}'",
        "v BETWEEN 'a' AND 'z'",
        "v BETWEEN '\u{E000}' AND '\u{FFFD}'",
        "v NOT BETWEEN '\u{61}' AND '\u{6100}'",
        "v > '\u{6100}' AND v < '\u{FFFD}'",
    ];
    for pragma_name in ["UTF-16le", "UTF-16be"] {
        // SQLite reference: rows selected by each predicate, rowid order.
        let path = temp_path(&format!("b_wr_{pragma_name}"));
        let sq_answers: Vec<Vec<String>> = {
            {
                let con = rusqlite::Connection::open(&path).unwrap();
                con.pragma_update(None, "encoding", pragma_name).unwrap();
                con.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);")
                    .unwrap();
                let mut stmt = con.prepare("INSERT INTO t (v) VALUES (?)").unwrap();
                for s in data {
                    stmt.execute([s]).unwrap();
                }
            }
            let con = rusqlite::Connection::open(&path).unwrap();
            predicates
                .iter()
                .map(|p| {
                    let sql = format!("SELECT v FROM t WHERE {p} ORDER BY id");
                    let mut stmt = con.prepare(&sql).unwrap();
                    stmt.query_map([], |r| r.get::<_, String>(0))
                        .unwrap()
                        .map(|r| r.unwrap())
                        .collect()
                })
                .collect()
        };
        // The engine on the same file.
        let db = Database::open(&path).unwrap();
        for (i, p) in predicates.iter().enumerate() {
            let sql = format!("SELECT v FROM t WHERE {p} ORDER BY id");
            let rows = db.query(&sql, ()).unwrap();
            let got: Vec<String> = rows
                .iter()
                .map(|r| match &r[0] {
                    Value::Text(t) => t.as_str().to_string(),
                    other => format!("{other:?}"),
                })
                .collect();
            assert_eq!(got, sq_answers[i], "{pragma_name}: WHERE {p}");
        }
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn engine_utf16_where_range_with_index_matches_sqlite() {
    // Same shape WITH an index on the ranged column: the engine's
    // in-memory index orders TEXT keys code-point-style, so the index
    // RANGE plan is declined under non-UTF-8 encodings and the scan
    // filter carries the predicate — results still match SQLite.
    let data = ADVERSARIAL;
    for pragma_name in ["UTF-16le", "UTF-16be"] {
        let path = temp_path(&format!("b_wri_{pragma_name}"));
        {
            let con = rusqlite::Connection::open(&path).unwrap();
            con.pragma_update(None, "encoding", pragma_name).unwrap();
            con.execute_batch(
                "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);
                 CREATE INDEX iv ON t(v);",
            )
            .unwrap();
            let mut stmt = con.prepare("INSERT INTO t (v) VALUES (?)").unwrap();
            for s in data {
                stmt.execute([s]).unwrap();
            }
        }
        let sq_answer: Vec<String> = {
            let con = rusqlite::Connection::open(&path).unwrap();
            let mut stmt = con
                .prepare("SELECT v FROM t WHERE v > 'a' ORDER BY id")
                .unwrap();
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        let db = Database::open(&path).unwrap();
        // Range + equality lookups both stay exact.
        let rows = db
            .query("SELECT v FROM t WHERE v > 'a' ORDER BY id", ())
            .unwrap();
        let got: Vec<String> = texts_of(&rows);
        assert_eq!(got, sq_answer, "{pragma_name}: indexed WHERE v > 'a'");
        // Equality through the index is unaffected.
        let n = count_of(&db, "SELECT COUNT(*) FROM t WHERE v = '\u{FFFD}'");
        assert_eq!(n, 1, "{pragma_name}: indexed equality lookup");
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn engine_utf16_dml_where_range_matches_sqlite() {
    // UPDATE / DELETE WHERE ranges on TEXT: the DML predicates follow the
    // same byte-order comparator.
    let data = ADVERSARIAL;
    for pragma_name in ["UTF-16le", "UTF-16be"] {
        let path = temp_path(&format!("b_dmlr_{pragma_name}"));
        {
            let con = rusqlite::Connection::open(&path).unwrap();
            con.pragma_update(None, "encoding", pragma_name).unwrap();
            con.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT, k INTEGER);")
                .unwrap();
            let mut stmt = con.prepare("INSERT INTO t (v, k) VALUES (?1, ?2)").unwrap();
            for (i, s) in data.iter().enumerate() {
                stmt.execute(rusqlite::params![s, (i + 1) as i64]).unwrap();
            }
        }
        // SQLite reference: DELETE everything above 'z', then list.
        let (sq_deleted, sq_remaining): (i64, Vec<String>) = {
            let con = rusqlite::Connection::open(&path).unwrap();
            let n = con.execute("DELETE FROM t WHERE v > 'z'", []).unwrap() as i64;
            let mut stmt = con.prepare("SELECT v FROM t ORDER BY id").unwrap();
            let rem: Vec<String> = stmt
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            (n, rem)
        };
        // The engine, on a PRISTINE copy (the reference DELETE above
        // already mutated the source — copy BEFORE deleting instead of
        // VACUUM INTO after): same inserts into a fresh file.
        let path2 = temp_path(&format!("b_dmlr2_{pragma_name}"));
        {
            let con = rusqlite::Connection::open(&path2).unwrap();
            con.pragma_update(None, "encoding", pragma_name).unwrap();
            con.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT, k INTEGER);")
                .unwrap();
            let mut stmt = con.prepare("INSERT INTO t (v, k) VALUES (?1, ?2)").unwrap();
            for (i, sv) in data.iter().enumerate() {
                stmt.execute(rusqlite::params![sv, (i + 1) as i64]).unwrap();
            }
        }
        let mut db = Database::open(&path2).unwrap();
        db.execute("UPDATE t SET k = 99 WHERE v BETWEEN 'a' AND 'z'", [])
            .unwrap();
        let updated = db.changes();
        // Everything between 'a' and 'z' under BOTH orders is the same
        // set here (BMP-plane bounds, no astral divergence) — verify k
        // was applied and nothing else.
        let k99 = count_of(&db, "SELECT COUNT(*) FROM t WHERE k = 99");
        assert!(
            updated >= 1 && k99 >= 1,
            "{pragma_name}: range UPDATE touched rows"
        );
        let k_orig = count_of(&db, "SELECT COUNT(*) FROM t WHERE k != 99");
        assert_eq!(
            k_orig,
            (data.len() as i64) - k99,
            "{pragma_name}: range UPDATE boundary"
        );
        db.execute("DELETE FROM t WHERE v > 'z'", []).unwrap();
        let del = db.changes();
        assert_eq!(del, sq_deleted, "{pragma_name}: range DELETE count");
        let rem: Vec<String> = texts_of(&db.query("SELECT v FROM t ORDER BY id", ()).unwrap());
        assert_eq!(
            rem, sq_remaining,
            "{pragma_name}: rows remaining after DELETE"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&path2);
    }
}

#[test]
fn engine_utf16_where_range_through_statement_step() {
    // The prepared-statement streaming path (cell serving + filter
    // evaluation) with a range predicate: same byte-order answers.
    let data = ADVERSARIAL;
    for pragma_name in ["UTF-16le", "UTF-16be"] {
        let path = temp_path(&format!("b_stpr_{pragma_name}"));
        {
            let con = rusqlite::Connection::open(&path).unwrap();
            con.pragma_update(None, "encoding", pragma_name).unwrap();
            con.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);")
                .unwrap();
            let mut stmt = con.prepare("INSERT INTO t (v) VALUES (?)").unwrap();
            for s in data {
                stmt.execute([s]).unwrap();
            }
        }
        let sq_answer: Vec<String> = {
            let con = rusqlite::Connection::open(&path).unwrap();
            let mut stmt = con
                .prepare("SELECT v FROM t WHERE v > 'a' ORDER BY id")
                .unwrap();
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        let db = Database::open(&path).unwrap();
        let mut stmt = db
            .prepare("SELECT id, v FROM t WHERE v > 'a' ORDER BY id")
            .unwrap();
        let mut got = Vec::new();
        use rustqlite::StepResult;
        while stmt.step().unwrap() == StepResult::Row {
            got.push(stmt.column_text(1).unwrap());
        }
        assert_eq!(got, sq_answer, "{pragma_name}: step-path WHERE v > 'a'");
        let _ = std::fs::remove_file(&path);
    }
}
