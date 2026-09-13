//! CLI tooling tests: `.dump` / `.export-schema` / `.export-data` /
//! `.import` (SQL + CSV) / `.backup` round-trips, driven through the
//! REAL `rustqlite-cli` binary, verified against the library AND real
//! SQLite (rusqlite executes our dump output — the strongest compat
//! proof a dump can have).

use rustqlite::{Database, Value};
use std::io::Write;
use std::process::{Command, Stdio};

fn cli() -> &'static str {
    env!("CARGO_BIN_EXE_rustqlite-cli")
}

/// Run the CLI with a piped stdin script; return (stdout, stderr, ok).
fn run_cli(args: &[&str], stdin_script: Option<&str>) -> (String, String, bool) {
    let mut cmd = Command::new(cli());
    cmd.args(args);
    if let Some(script_text) = stdin_script {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn cli");
        let mut handle = child.stdin.take().unwrap();
        let script = script_text.to_string();
        std::thread::spawn(move || {
            let _ = handle.write_all(script.as_bytes());
        });
        let out = child.wait_with_output().expect("wait cli");
        (
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
            out.status.success(),
        )
    } else {
        let out = cmd.output().expect("run cli");
        (
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
            out.status.success(),
        )
    }
}

/// The demo schema: every value type, FKs, views, triggers, AUTOINCREMENT.
const SETUP: &str = "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL, data BLOB);\n\
INSERT INTO users (name, score, data) VALUES ('alice', 91.5, X'deadbeef');\n\
INSERT INTO users (name, score, data) VALUES ('bob, jr', 70.0, NULL);\n\
INSERT INTO users (name, score, data) VALUES ('it''s \"quoted\"', NULL, X'');\n\
INSERT INTO users (name, score, data) VALUES ('multi\nline', -0.25, X'00ff');\n\
INSERT INTO users (name, score, data) VALUES ('unicode é 😀', 1e10, NULL);\n\
INSERT INTO users (name, score, data) VALUES ('nan holder', NULL, NULL);\n\
UPDATE users SET score = 1e999 WHERE name = 'nan holder';\n\
CREATE INDEX idx_users_name ON users(name);\n\
CREATE VIEW v_top AS SELECT id, name FROM users WHERE score > 80;\n\
CREATE TABLE log (id INTEGER PRIMARY KEY, msg TEXT);\n\
CREATE TRIGGER trg_after_ins AFTER INSERT ON users BEGIN INSERT INTO log (msg) VALUES ('new'); END;\n\
CREATE TABLE auto (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT);\n\
INSERT INTO auto (v) VALUES ('a'), ('b'), ('c');\n\
DELETE FROM auto WHERE v = 'c';\n";

/// Build a database at `path` through the library (deterministic content).
/// One multi-statement execute — the engine's split_script handles the
/// trigger body's internal semicolons correctly.
fn build_db(path: &std::path::Path) {
    let mut db = Database::open(path).unwrap();
    db.execute(SETUP, []).unwrap();
}

/// Sorted set of (type, name, sql) from sqlite_master (internal tables
/// excluded), for round-trip comparison.
fn schema_fingerprint(path: &std::path::Path) -> Vec<(String, String, String)> {
    let db = Database::open(path).unwrap();
    let (_, rows) = db
        .query_with_columns(
            "SELECT type, name, sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
            [],
        )
        .unwrap();
    rows.iter()
        .map(|r| match (r[0].clone(), r[1].clone(), r[2].clone()) {
            (Value::Text(a), Value::Text(b), Value::Text(c)) => (
                a.as_str().to_string(),
                b.as_str().to_string(),
                c.as_str().to_string(),
            ),
            _ => panic!("bad row"),
        })
        .collect()
}

fn user_rows(path: &std::path::Path) -> Vec<Vec<Value>> {
    let db = Database::open(path).unwrap();
    let (_, rows) = db
        .query_with_columns("SELECT id, name, score, data FROM users ORDER BY id", [])
        .unwrap();
    rows
}

#[test]
fn dump_roundtrip_native() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.db");
    let dst = dir.path().join("dst.db");
    let dump = dir.path().join("dump.sql");
    build_db(&src);

    let (out, err, ok) = run_cli(
        &["--dump", dump.to_str().unwrap(), src.to_str().unwrap()],
        None,
    );
    assert!(ok, "{} {}", out, err);

    // The dump must be REAL SQLite loadable — the strongest check.
    let text = std::fs::read_to_string(&dump).unwrap();
    let sqlite = rusqlite::Connection::open_in_memory().unwrap();
    sqlite
        .execute_batch(&text)
        .unwrap_or_else(|e| panic!("real SQLite rejected our dump: {}\n{}", e, text));
    let rows: Vec<(i64, String)> = sqlite
        .prepare("SELECT id, name FROM users ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows.len(), 6);
    assert_eq!(rows[0].1, "alice");
    assert_eq!(rows[4].1, "unicode é 😀");
    assert_eq!(rows[3].1, "multi\nline");
    let blob: Vec<u8> = sqlite
        .query_row("SELECT data FROM users WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(blob, vec![0xde, 0xad, 0xbe, 0xef]);
    let nan_holder: f64 = sqlite
        .query_row(
            "SELECT score FROM users WHERE name = 'nan holder'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        nan_holder.is_infinite(),
        "1e999 must round-trip as +inf: {}",
        nan_holder
    );
    // sqlite_sequence high-water
    let seq: i64 = sqlite
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 'auto'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(seq, 3);
    // Trigger never fired during the import.
    let log: i64 = sqlite
        .query_row("SELECT count(*) FROM log", [], |r| r.get(0))
        .unwrap();
    assert_eq!(log, 0, "trigger must be dumped AFTER the data");

    // Import into a fresh rustqlite database: schema + data identical.
    let (out, err, ok) = run_cli(
        &["--import", dump.to_str().unwrap(), dst.to_str().unwrap()],
        None,
    );
    assert!(ok, "{} {}", out, err);
    assert_eq!(schema_fingerprint(&src), schema_fingerprint(&dst));
    assert_eq!(user_rows(&src), user_rows(&dst));

    // AUTOINCREMENT high-water honored after import.
    let (out, _, ok) = run_cli(
        &[dst.to_str().unwrap()],
        Some("INSERT INTO auto (v) VALUES ('d');\nSELECT id FROM auto WHERE v = 'd';\n"),
    );
    assert!(ok, "{}", out);
    assert!(
        out.contains("| 4"),
        "next id must be 4 (high-water 3): {}",
        out
    );

    // The trigger fires normally AFTER the import.
    let (out, _, ok) = run_cli(
        &[dst.to_str().unwrap()],
        Some("INSERT INTO users (name) VALUES ('post');\nSELECT count(*) FROM log;\n"),
    );
    assert!(ok, "{}", out);
    assert!(
        out.contains("| 1"),
        "trigger must fire after import: {}",
        out
    );
}

#[test]
fn dump_roundtrip_sqlite_format_target() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.db");
    let dst = dir.path().join("dst.db");
    let dump = dir.path().join("dump.sql");
    build_db(&src);
    let (out, err, ok) = run_cli(
        &["--dump", dump.to_str().unwrap(), src.to_str().unwrap()],
        None,
    );
    assert!(ok, "{} {}", out, err);
    // Native dump -> SQLite-format target (dumps are pure SQL; the target
    // format is irrelevant to the dump path).
    let (out, err, ok) = run_cli(
        &[
            "--sqlite-format",
            "--import",
            dump.to_str().unwrap(),
            dst.to_str().unwrap(),
        ],
        None,
    );
    assert!(ok, "{} {}", out, err);
    assert_eq!(schema_fingerprint(&src), schema_fingerprint(&dst));
    assert_eq!(user_rows(&src), user_rows(&dst));
    // The SQLite-format result must ALSO be readable by real SQLite.
    let sqlite = rusqlite::Connection::open(dst.to_str().unwrap()).unwrap();
    let n: i64 = sqlite
        .query_row("SELECT count(*) FROM users", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 6);
}

#[test]
fn export_schema_and_data_split() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.db");
    let schema = dir.path().join("schema.sql");
    let data = dir.path().join("data.sql");
    build_db(&src);

    let (out, err, ok) = run_cli(
        &[
            "--export-schema",
            schema.to_str().unwrap(),
            src.to_str().unwrap(),
        ],
        None,
    );
    assert!(ok, "{} {}", out, err);
    let schema_text = std::fs::read_to_string(&schema).unwrap();
    assert!(schema_text.contains("CREATE TABLE users"));
    assert!(schema_text.contains("CREATE INDEX idx_users_name"));
    assert!(schema_text.contains("CREATE VIEW v_top"));
    assert!(schema_text.contains("CREATE TRIGGER trg_after_ins"));
    // No bare data statements (trigger BODIES legitimately contain
    // INSERT INTO — check statement starts, not substrings).
    assert!(
        !schema_text
            .lines()
            .any(|l| l.trim_start().starts_with("INSERT INTO")),
        "{}",
        schema_text
    );
    assert!(
        !schema_text.contains("BEGIN TRANSACTION"),
        "{}",
        schema_text
    );

    let (out, err, ok) = run_cli(
        &[
            "--export-data",
            data.to_str().unwrap(),
            src.to_str().unwrap(),
        ],
        None,
    );
    assert!(ok, "{} {}", out, err);
    let data_text = std::fs::read_to_string(&data).unwrap();
    assert!(data_text.contains("INSERT INTO \"users\" VALUES(1,'alice',91.5,X'deadbeef');"));
    assert!(data_text.contains("INSERT INTO \"sqlite_sequence\""));
    assert!(!data_text.contains("CREATE TABLE"), "{}", data_text);

    // Schema + data recombined reproduce the database.
    let both = format!(
        "PRAGMA foreign_keys=OFF;\nBEGIN;\n{}{}COMMIT;\n",
        schema_text, data_text
    );
    let sqlite = rusqlite::Connection::open_in_memory().unwrap();
    sqlite.execute_batch(&both).unwrap();
    let n: i64 = sqlite
        .query_row("SELECT count(*) FROM users", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 6);
}

#[test]
fn backup_physical_copy() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.db");
    let dst = dir.path().join("backup.db");
    build_db(&src);
    let (out, err, ok) = run_cli(
        &["--backup", dst.to_str().unwrap(), src.to_str().unwrap()],
        None,
    );
    assert!(ok, "{} {}", out, err);
    assert!(out.contains("backup written"));
    assert_eq!(schema_fingerprint(&src), schema_fingerprint(&dst));
    assert_eq!(user_rows(&src), user_rows(&dst));

    // Backup of a SQLite-format database is a real SQLite file.
    let sqsrc = dir.path().join("sq.db");
    let sqdst = dir.path().join("sq-backup.db");
    let (out, err, ok) = run_cli(
        &["--sqlite-format", "--import-sqlite-never", "unused"],
        None,
    );
    assert!(!ok, "unknown flags must be rejected: {} {}", out, err);
    let _ = out;
    // Build the SQLite-format db via CLI import of the dump.
    let dump = dir.path().join("dump.sql");
    run_cli(
        &["--dump", dump.to_str().unwrap(), src.to_str().unwrap()],
        None,
    );
    let (_, err, ok) = run_cli(
        &[
            "--sqlite-format",
            "--import",
            dump.to_str().unwrap(),
            sqsrc.to_str().unwrap(),
        ],
        None,
    );
    assert!(ok, "{}", err);
    let (out, err, ok) = run_cli(
        &["--backup", sqdst.to_str().unwrap(), sqsrc.to_str().unwrap()],
        None,
    );
    assert!(ok, "{} {}", out, err);
    let sqlite = rusqlite::Connection::open(sqdst.to_str().unwrap()).unwrap();
    let n: i64 = sqlite
        .query_row("SELECT count(*) FROM users", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 6);
}

#[test]
fn csv_import_modes() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("csv.db");

    // Existing table: every row is data (no header skip), matching
    // sqlite3; quoted commas / newlines / embedded quotes survive.
    std::fs::write(
        dir.path().join("a.csv"),
        "name,notes\ncarol,\"has, comma\"\ndave,\"say \"\"hi\"\" now\"\n",
    )
    .unwrap();
    let (out, _, ok) = run_cli(
        &[db.to_str().unwrap()],
        Some("CREATE TABLE t (name TEXT, notes TEXT);\n"),
    );
    assert!(ok, "{}", out);
    let (out, err, ok) = run_cli(
        &[
            "--import-csv",
            dir.path().join("a.csv").to_str().unwrap(),
            "t",
            db.to_str().unwrap(),
        ],
        None,
    );
    assert!(ok, "{} {}", out, err);
    assert!(out.contains("imported 3 rows"), "{}", out);
    let engine = Database::open(&db).unwrap();
    let (_, rows) = engine
        .query_with_columns("SELECT name, notes FROM t ORDER BY name", [])
        .unwrap();
    let text = |v: &Value| match v {
        Value::Text(t) => t.as_str().to_string(),
        _ => panic!("expected text"),
    };
    let got: Vec<(String, String)> = rows.iter().map(|r| (text(&r[0]), text(&r[1]))).collect();
    assert_eq!(
        got,
        vec![
            ("carol".to_string(), "has, comma".to_string()),
            ("dave".to_string(), "say \"hi\" now".to_string()),
            ("name".to_string(), "notes".to_string()),
        ],
        "{:#?}",
        rows
    );

    // Missing table: created from the header row, all TEXT.
    let (out, err, ok) = run_cli(
        &[
            "--import-csv",
            dir.path().join("a.csv").to_str().unwrap(),
            "auto_created",
            db.to_str().unwrap(),
        ],
        None,
    );
    assert!(ok, "{} {}", out, err);
    assert!(out.contains("created auto_created"), "{}", out);
    let engine = Database::open(&db).unwrap();
    let (_, rows) = engine
        .query_with_columns("SELECT name FROM auto_created ORDER BY name", [])
        .unwrap();
    assert_eq!(rows.len(), 2);

    // Column-count mismatch: error, correct counts in the message.
    std::fs::write(dir.path().join("b.csv"), "a,b\n1,2,3\n").unwrap();
    let (_, err, ok) = run_cli(
        &[
            "--import-csv",
            dir.path().join("b.csv").to_str().unwrap(),
            "auto_created",
            db.to_str().unwrap(),
        ],
        None,
    );
    assert!(!ok, "must fail on column count mismatch");
    assert!(err.contains("expected 2 columns but found 3"), "{}", err);
}

#[test]
fn dot_commands_over_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("dot.db");
    build_db(&db);
    let (out, _, ok) = run_cli(
        &[db.to_str().unwrap()],
        Some(".tables\n.schema users\n.mode csv\nSELECT id FROM users WHERE id <= 2;\n.quit\n"),
    );
    assert!(ok, "{}", out);
    assert!(out.contains("log"), ".tables must list tables: {}", out);
    assert!(out.contains("v_top"), ".tables must list views: {}", out);
    assert!(
        !out.contains("sqlite_sequence"),
        ".tables must hide internal tables: {}",
        out
    );
    assert!(
        out.contains("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL, data BLOB);"),
        ".schema users: {}",
        out
    );
    assert!(out.contains("1\n2"), "csv mode rows: {}", out);

    // .schema pattern filtering.
    let (out, _, ok) = run_cli(&[db.to_str().unwrap()], Some(".schema v_top\n"));
    assert!(ok);
    assert!(out.contains("CREATE VIEW v_top"));
    assert!(!out.contains("CREATE TABLE users"), "{}", out);
}

#[test]
fn dump_stdout_flag() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("src.db");
    build_db(&db);
    let (out, err, ok) = run_cli(&["--dump", "-", db.to_str().unwrap()], None);
    assert!(ok, "{} {}", out, err);
    assert!(out.contains("BEGIN TRANSACTION;"), "{}", out);
    assert!(
        out.contains("INSERT INTO \"users\" VALUES(1,'alice',91.5,X'deadbeef');"),
        "{}",
        out
    );
    assert!(out.contains("COMMIT;"), "{}", out);
    // NaN dumps as NULL, ±inf as ±9.0e999.
    assert!(
        out.contains("INSERT INTO \"users\" VALUES(6,'nan holder',9.0e999,NULL);"),
        "{}",
        out
    );
    // Real-valued REALs keep their REAL shape.
    assert!(out.contains("70.0"), "{}", out);
}

#[test]
fn sqlite_master_sql_strips_trailing_semicolon() {
    // Regression: DDL entered WITH a trailing `;` (the interactive /
    // split_script shape) must store the same sqlite_master.sql text real
    // SQLite stores via its prepare path: the statement minus the `;` and
    // surrounding trivia. (SQLite's own exec path keeps pre-`;` spaces —
    // SQLite is internally inconsistent here; the prepare contract is the
    // one drivers and tools see.)
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("semi.db");
    let mut engine = Database::open(&db).unwrap();
    engine
        .execute("CREATE TABLE semi_t (id INTEGER PRIMARY KEY, v TEXT);", [])
        .unwrap();
    engine
        .execute("CREATE INDEX semi_i ON semi_t(v)   ;", [])
        .unwrap();
    drop(engine);
    let sqlite = rusqlite::Connection::open_in_memory().unwrap();
    sqlite
        .execute("CREATE TABLE semi_t (id INTEGER PRIMARY KEY, v TEXT);", [])
        .unwrap();
    sqlite
        .execute("CREATE INDEX semi_i ON semi_t(v)   ;", [])
        .unwrap();
    let ours: Vec<(String, String)> = {
        let db = Database::open(&db).unwrap();
        let (_, rows) = db
            .query_with_columns(
                "SELECT name, sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY name",
                [],
            )
            .unwrap();
        rows.iter()
            .map(|r| match (r[0].clone(), r[1].clone()) {
                (Value::Text(a), Value::Text(b)) => {
                    (a.as_str().to_string(), b.as_str().to_string())
                }
                _ => panic!(),
            })
            .collect()
    };
    let theirs: Vec<(String, String)> = {
        let mut stmt = sqlite
            .prepare("SELECT name, sql FROM sqlite_master ORDER BY name")
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap();
        rows.map(Result::unwrap).collect()
    };
    assert_eq!(ours, theirs);
    assert!(
        !ours.iter().any(|(_, sql)| sql.ends_with(';')),
        "stored DDL must never end with a semicolon: {:?}",
        ours
    );
}

#[test]
fn import_sql_script_via_dot_read() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("read.db");
    let script = dir.path().join("script.sql");
    std::fs::write(
        &script,
        "CREATE TABLE r (x INTEGER);\nINSERT INTO r VALUES (10), (20);\n",
    )
    .unwrap();
    // Run with the tempdir as cwd so the RELATIVE path resolves (that's
    // the shell-like contract: .read works on paths relative to where
    // the user is).
    let out = {
        let mut cmd = Command::new(cli());
        cmd.args([db.to_str().unwrap()])
            .current_dir(dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = cmd
            .spawn()
            .and_then(|mut child| {
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(b".read script.sql\nSELECT sum(x) FROM r;\n")?;
                child.wait_with_output()
            })
            .expect("run cli");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).to_string()
    };
    assert!(out.contains("| 30"), "{}", out);
    // cwd-relative paths work; absolute too.
    let (out, err, ok) = run_cli(
        &[db.to_str().unwrap()],
        Some(&format!(
            ".read {}\nSELECT count(*) FROM r;\n",
            script.display()
        )),
    );
    assert!(ok, "{} {}", out, err);
    assert!(out.contains("| 2"), "{}", out);
}
