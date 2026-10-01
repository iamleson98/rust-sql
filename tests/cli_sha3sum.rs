//! `.sha3sum` parity: byte-identical hashes against the REAL sqlite3
//! 3.53.4 shell.
//!
//! The fixtures (`tests/fixtures/sha3sum_{a,b,empty}.db`) were CREATED by
//! the official sqlite3 3.53.4 binary, and every golden hash below was
//! PRODUCED by that same binary's `.sha3sum` on those exact files. The
//! rustqlite CLI then opens the same SQLite-format files and must emit
//! the identical hashes — which exercises the whole parity stack at once:
//!
//! * SQLite-format file reading (the schema, WITHOUT ROWID, blob and
//!   REAL payloads come from REAL SQLite pages),
//! * `SELECT * FROM "t" NOT INDEXED` execution (row order, column order,
//!   value types),
//! * the internal-table query forms (`sqlite_schema`, `sqlite_sequence`),
//! * the shathree.c stream encoding (`S<n>:<sql>`, `R`, `N`/`I`/`F`/`T`/`B`),
//! * the Keccak port itself (all four digest sizes).
//!
//! The command's edge behaviors are pinned too: the `--sha3-XXX` sizes,
//! `--schema` (sqlite_schema + sqlite_sequence hashed), per-table
//! separate mode with a LIKE pattern, the `sqlite_%` pattern flipping
//! schema mode on, and sqlite3's quirks — no output plus the
//! `.sha3sum failed.` tail when no user table matches, and the empty-DB
//! hash of `sqlite_schema` alone.
//!
//! Also covered: the `sha3()` / `sha3_agg()` SQL functions (golden values
//! from the real shell, including `sha3(1e300)` — the number-to-text
//! conversion feeding the hash), `.fullschema`'s ANALYZE sandwich, the
//! `.once -e|-x|-w` / `.excel` / `.www` temp-file captures (content
//! asserted through a PATH-injected fake opener), `.log`, `.scanstats`,
//! and the `list` / `html` output modes.

use std::io::Write;
use std::process::{Command, Stdio};

fn cli() -> &'static str {
    env!("CARGO_BIN_EXE_rustqlite-cli")
}

/// Run the CLI with a piped stdin script; return (stdout, stderr).
fn run_cli(args: &[&str], script: &str) -> (String, String) {
    let mut cmd = Command::new(cli());
    cmd.args(args)
        .env("HOME", std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn cli");
    let mut handle = child.stdin.take().unwrap();
    let text = script.to_string();
    std::thread::spawn(move || {
        let _ = handle.write_all(text.as_bytes());
    });
    let out = child.wait_with_output().expect("wait cli");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// Run a `.sha3sum` variant against a fixture and return its stdout with
/// the banner/prompts stripped.
fn sha3sum_out(db: &str, args: &str) -> String {
    let script = format!(".mode list\n.headers off\n.sha3sum {}\n.quit\n", args);
    let (out, _err) = run_cli(&[db], &script);
    out.lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.trim_start_matches("rustqlite> ").to_string())
        .filter(|l| {
            !l.is_empty()
                && !l.starts_with("rustqlite ")
                && !l.starts_with("Type ")
                && !l.starts_with("v0.1.0")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{}", env!("CARGO_MANIFEST_DIR"), name)
}

#[test]
fn sha3sum_golden_combined_all_sizes() {
    // Goldens from the real sqlite3 3.53.4 shell on sha3sum_a.db.
    assert_eq!(
        sha3sum_out(&fixture("sha3sum_a.db"), ""),
        "55d0ca8a5ec0db9ecb5a9a8261ca3e612dde9631ef4ddb33d8283241"
    );
    assert_eq!(
        sha3sum_out(&fixture("sha3sum_a.db"), "--sha3-256"),
        "a401bc9a59cd6163e926c20680895fced04ae3ffe6fdf5aa16173ca6980a3d35"
    );
    assert_eq!(
        sha3sum_out(&fixture("sha3sum_a.db"), "--sha3-384"),
        "9320ef5d02d12b64222fc04769ebc07f8794779ec998d1f6d82fcb242343ab30c89c7c552ad74add1904668f3be44161"
    );
    assert_eq!(
        sha3sum_out(&fixture("sha3sum_a.db"), "--sha3-512"),
        "83ae7d09ac82fd53fea6cfabe3f83218573b37d280ea4b4df213139721639a1e40ca4f905819877addeb5cbc7eec3d8ce049469b606656034e2e1dc1f9aaee6b"
    );
}

#[test]
fn sha3sum_golden_schema_mode() {
    // --schema adds sqlite_schema (always) and the internal tables.
    assert_eq!(
        sha3sum_out(&fixture("sha3sum_a.db"), "--schema"),
        "96e4313acbeb2af62563e29a82b4add33e1150d486f5bb26c8236b1f"
    );
    assert_eq!(
        sha3sum_out(&fixture("sha3sum_a.db"), "--sha3-384 --schema"),
        "179f337633dca4cfa5cc505aa7fe346bf80d5008796ba1cc5c0949112e055e40f41c3b02b0e5d2b39675789feaefdf14"
    );
    // The AUTOINCREMENT database: sqlite_sequence joins the hash set.
    assert_eq!(
        sha3sum_out(&fixture("sha3sum_b.db"), "--schema"),
        "7d9e280c34930be490454575aa13f7c10ea75ec8a66a7e786bf04a77"
    );
    assert_eq!(
        sha3sum_out(&fixture("sha3sum_b.db"), ""),
        "1a01ef4f93f808a1854b56ef2b8725ec58f8756319ead99ea4d13890"
    );
    assert_eq!(
        sha3sum_out(&fixture("sha3sum_b.db"), "--sha3-256"),
        "fa3346e72da0776283a797882d03c39612bc14099a5d692d692ab4f7296a4455"
    );
}

#[test]
fn sha3sum_golden_per_table() {
    assert_eq!(
        sha3sum_out(&fixture("sha3sum_a.db"), "t1"),
        "da6369fd84a7a7c7c6ceb49d681ef04894190756d2474b67d05b14f7|t1"
    );
    assert_eq!(
        sha3sum_out(&fixture("sha3sum_a.db"), "t2"),
        "98ee77cafe583b8bfbab4e24d9f5565ebe906caa023c84b8b5a38b24|t2"
    );
    assert_eq!(
        sha3sum_out(&fixture("sha3sum_a.db"), "--sha3-256 t2"),
        "812ba18e26489361c71290d4671dedd63acf1923deb526473718996728cbd0c6|t2"
    );
    assert_eq!(
        sha3sum_out(&fixture("sha3sum_a.db"), "mixed"),
        "8ef7048573e60eb0bdd683a18f827cc4d9cdd75fdc3a0f3e21f73ff6|mixed"
    );
}

#[test]
fn sha3sum_sqlite_pattern_and_quirks() {
    // A pattern matching `sqlite_*` flips schema mode on; the internal
    // tables hash separately (goldens from the real shell).
    assert_eq!(
        sha3sum_out(&fixture("sha3sum_b.db"), "sqlite_schema"),
        "150af811bd1820b3e95d6f380df749fb5713cddbdafcf605aa1e33b8|sqlite_schema"
    );
    assert_eq!(
        sha3sum_out(&fixture("sha3sum_b.db"), "sqlite_%"),
        "150af811bd1820b3e95d6f380df749fb5713cddbdafcf605aa1e33b8|sqlite_schema\n\
         c55f3580a431209c429f94719fa4e2c342e9b78fd5d1c99c5b9fd732|sqlite_sequence"
    );
    // No matching USER table → no rows and sqlite3's ".sha3sum failed."
    // tail on stderr (the reversible-text check's empty-table quirk).
    let script = ".mode list\n.headers off\n.sha3sum sqlite_schema\n.quit\n";
    let (out, err) = run_cli(&[&fixture("sha3sum_b.db")], script);
    assert!(out.contains("150af811bd1820b3e95d6f380df749fb5713cddbdafcf605aa1e33b8"));
    assert!(err.contains(".sha3sum failed."), "err: {err}");
    // A pattern matching nothing at all: NO hash output at all.
    let (out, err) = run_cli(&[&fixture("sha3sum_b.db")], ".sha3sum nosuch\n.quit\n");
    assert!(!out.contains("hash"), "no rows expected, got: {out}");
    assert!(err.contains(".sha3sum failed."));
}

#[test]
fn sha3sum_empty_database_quirks() {
    // Empty DB without --schema: no output, just the failure tail.
    let (out, err) = run_cli(&[&fixture("sha3sum_empty.db")], ".sha3sum\n.quit\n");
    let hash_lines: Vec<&str> = out
        .lines()
        .filter(|l| l.chars().all(|c| c.is_ascii_hexdigit() || c == '|'))
        .collect();
    assert!(hash_lines.is_empty(), "no hash expected, got {out:?}");
    assert!(err.contains(".sha3sum failed."));
    // With --schema: sqlite_schema still hashes (golden from the real
    // shell), and the failure tail STILL prints (no user tables).
    assert_eq!(
        sha3sum_out(&fixture("sha3sum_empty.db"), "--schema"),
        "0639733bd5a874a9d5450bfb6162356c198b5502eea4940ab303c94c"
    );
    let (_, err) = run_cli(
        &[&fixture("sha3sum_empty.db")],
        ".sha3sum --schema\n.quit\n",
    );
    assert!(err.contains(".sha3sum failed."));
}

#[test]
fn sha3_sql_functions_golden() {
    // All goldens produced by the real sqlite3 3.53.4 shell (shathree.c).
    let script = ".mode list\n.headers off\n\
        SELECT lower(hex(sha3('abc')));\n\
        SELECT lower(hex(sha3('abc',224)));\n\
        SELECT lower(hex(sha3('abc',384)));\n\
        SELECT lower(hex(sha3('abc',512)));\n\
        SELECT lower(hex(sha3(x'00ff')));\n\
        SELECT lower(hex(sha3(0)));\n\
        SELECT lower(hex(sha3(1)));\n\
        SELECT lower(hex(sha3(-1)));\n\
        SELECT lower(hex(sha3(1.5)));\n\
        SELECT lower(hex(sha3(1e300)));\n\
        SELECT typeof(sha3(NULL));\n\
        SELECT lower(hex(sha3_agg(v))) FROM (SELECT 1 AS v UNION ALL SELECT 'a' UNION ALL SELECT NULL UNION ALL SELECT 2.5 UNION ALL SELECT x'00ff');\n\
        SELECT typeof(sha3_agg(v)) FROM (SELECT 1 AS v) WHERE 0;\n\
        .quit\n";
    let (out, _err) = run_cli(&[":memory:"], script);
    let lines: Vec<String> = out
        .lines()
        .map(|l| l.trim_start_matches("rustqlite> ").trim().to_string())
        .filter(|l| !l.is_empty() && !l.starts_with("rustqlite ") && !l.starts_with("Type "))
        .collect();
    let want = [
        "3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532",
        "e642824c3f8cf24ad09234ee7d3c766fc9a3a5168d0c94ad73b46fdf",
        "ec01498288516fc926459f58e2c6ad8df9b473cb0fc08c2596da7cf0e49be4b298d88cea927ac7f539f1edf228376d25",
        "b751850b1a57168a5693cd924b6b096e08f621827444f70d884f5d0240d2712e10e116e9192af3c91a7ec57647e3934057340b4cf408d5a56592f8274eec53f0",
        "17709a2e0d4734ada82a5f7042e459c726ed979924216b5eedc769422d6558cf",
        // sha3(0)/sha3(1)/sha3(-1): the decimal TEXT of the integer feeds
        // the hash.
        "f9e2eaaa42d9fe9e558a9b8ef1bf366f190aacaa83bad2641ee106e9041096e4",
        "67b176705b46206614219f47a05aee7ae6a3edbe850bbbe214c536b989aea4d2",
        "28f061b4d9c2b1e35e8fbc5e339ccf7f0c99319b278ba50c4195a0a55d53d1e4",
        // sha3(1.5) — "1.5"; sha3(1e300) — "1.0e+300" (the 3.53
        // REAL→TEXT renderer feeding the hash).
        "331a267242613d2e77c52cbcfa51cf06a40b3a8f37ed2916d99253acedfff747",
        "be99f38abd9c9bff5978b8419c955ad9f223b19d55068a1d66fc09bfb49f2c32",
        "null",
        "c17ffdb5ab195de1922e8930360b6c1fde4d01a6edb1d3c636a1a90e5046d6ec",
        "null",
    ];
    assert_eq!(
        lines.len(),
        want.len(),
        "unexpected output:\n{}",
        lines.join("\n")
    );
    for (got, w) in lines.iter().zip(want) {
        assert_eq!(got, w);
    }
}

/// `.fullschema`: the schema lines + the ANALYZE sandwich + INSERT dumps,
/// byte-checked against the real shell on the parts the oracle build
/// produces (the engine additionally carries sqlite_stat4 rows, like a
/// SQLITE_ENABLE_STAT4 build).
#[test]
fn fullschema_analyze_sandwich() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("fs.db");
    let setup = "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);\n\
                 INSERT INTO t VALUES (1,'x'),(2,'y');\n\
                 CREATE INDEX ia ON t(b);\n\
                 ANALYZE;\n";
    let (out, _err) = run_cli(
        &[db.to_str().unwrap(), "--sqlite-format"],
        &format!("{}\n.quit\n", setup),
    );
    assert!(out.contains("OK"), "setup failed: {out}");
    let (out, _err) = run_cli(&[db.to_str().unwrap()], ".fullschema\n.quit\n");
    let body: Vec<&str> = out
        .lines()
        .map(|l| l.trim_start_matches("rustqlite> ").trim())
        .filter(|l| !l.is_empty() && !l.starts_with("rustqlite ") && !l.starts_with("Type "))
        .collect();
    let joined = body.join("\n");
    // The oracle (stat1-only build) prints exactly:
    //   CREATE TABLE t(...); CREATE INDEX ia ON t(b); ANALYZE
    //   sqlite_schema; INSERT INTO sqlite_stat1 VALUES('t','ia','2 1');
    //   ANALYZE sqlite_schema;
    assert!(joined.contains("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);\n"));
    assert!(joined.contains("CREATE INDEX ia ON t(b);\n"));
    assert!(joined
        .contains("ANALYZE sqlite_schema;\nINSERT INTO sqlite_stat1 VALUES('t','ia','2 1');\n"));
    // The stat dump is sandwiched between two ANALYZE lines.
    let first = joined.find("ANALYZE sqlite_schema;").unwrap();
    let last = joined.rfind("ANALYZE sqlite_schema;").unwrap();
    assert!(first < last, "sandwich expected: {joined}");
    assert!(
        joined[first..last].contains("INSERT INTO sqlite_stat1"),
        "stat rows inside the sandwich: {joined}"
    );
    // The engine's stat4 table joins the dump (a STAT4-enabled sqlite3
    // prints those too).
    assert!(joined.contains("INSERT INTO sqlite_stat4"), "{joined}");
}

/// `.fullschema` with no ANALYZE ever run: the honest marker.
#[test]
fn fullschema_no_stat_tables() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("fs.db");
    let _ = run_cli(&[db.to_str().unwrap()], "CREATE TABLE t(x);\n.quit\n");
    let (out, _) = run_cli(&[db.to_str().unwrap()], ".fullschema\n.quit\n");
    assert!(out.contains("/* No STAT tables available */"), "{out}");
}

/// `.once -x` / `.excel` / `.once -w` / `.once -e` temp-file captures —
/// content asserted through a PATH-injected fake opener (unix only: the
/// opener binary name differs per platform).
#[cfg(not(target_os = "windows"))]
#[test]
fn once_captures_and_opener() {
    let home = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let captures = home.path().join("captures");
    std::fs::create_dir_all(&captures).unwrap();
    // A fake opener that snapshots the file it was handed (keeping the
    // extension — double quotes so ${1##*.} expands). Both unix opener
    // names are provided (xdg-open on linux, open on macOS).
    let script_body = format!(
        "#!/bin/sh\ncp \"$1\" \"{}/captured.${{1##*.}}\"\nexit 0\n",
        captures.display()
    );
    for name in ["xdg-open", "open"] {
        let fake = bin.path().join(name);
        std::fs::write(&fake, &script_body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    // Run A: the non-plain browser capture (its own captures dir).
    let caps_a = home.path().join("caps_a");
    std::fs::create_dir_all(&caps_a).unwrap();
    run_with_fake_opener(
        ":memory:",
        ".mode list\n.headers off\n.once -w\nSELECT 1 AS a, 2 AS b;\n.quit\n",
        bin.path(),
        &caps_a,
    );
    let html = std::fs::read(caps_a.join("captured.html")).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&html),
        "<!DOCTYPE html>\n<HTML><BODY><PRE>\n</PRE>\n<TABLE border='1' cellspacing='0' cellpadding='2'>\n<TR>\n<TD>1\n<TD>2\n</TR>\n</TABLE>\n<PRE></PRE></BODY></HTML>\n",
        "the MODE_Www skeleton must match sqlite3 byte-for-byte"
    );

    // Run C: the multi-column excel capture — unquoted even when a
    // value CONTAINS the separator (sqlite3's .excel quirk; `.mode csv`
    // WOULD quote it).
    let caps_c = home.path().join("caps_c");
    std::fs::create_dir_all(&caps_c).unwrap();
    run_with_fake_opener(
        ":memory:",
        ".mode list\n.headers off\n.once -x\nSELECT 1 AS a, 'x,y' AS b, NULL AS c;\n.quit\n",
        bin.path(),
        &caps_c,
    );
    let csv = std::fs::read(caps_c.join("captured.csv")).unwrap();
    assert_eq!(csv, b"1,x,y,\r\n");

    // Run B: excel / editor / --plain captures.
    let caps_b = home.path().join("caps_b");
    std::fs::create_dir_all(&caps_b).unwrap();
    run_with_fake_opener(
        ":memory:",
        ".mode list\n.headers off\n\
         .once -x\nSELECT 1 AS a, 'x,y' AS b, NULL AS c;\n\
         .once -e\nSELECT 42 AS a;\n\
         .once -x\nSELECT 8 AS a;\n\
         .once -w --plain\nSELECT 7 AS a;\n\
         .quit\n",
        bin.path(),
        &caps_b,
    );
    // The excel capture: unquoted CSV with CRLF (sqlite3's own quirk).
    // (The second -x capture overwrote the first with the same shape.)
    let csv = std::fs::read(caps_b.join("captured.csv")).unwrap();
    assert_eq!(csv, b"8\r\n");
    // The editor capture: current mode (list).
    let txt = std::fs::read(caps_b.join("captured.txt")).unwrap();
    assert_eq!(txt, b"42\n");
    // The --plain browser capture: sqlite3's PLAINTEXT skeleton,
    // verified against the real shell.
    let plain = std::fs::read(caps_b.join("captured.html")).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&plain),
        "<!DOCTYPE html>\n<BODY>\n<PLAINTEXT>\n7\n"
    );
}

/// Drive the CLI with a PATH-injected fake opener that snapshots every
/// file it is handed into `caps` (keeping the extension).
#[cfg(not(target_os = "windows"))]
fn run_with_fake_opener(db: &str, script: &str, bin: &std::path::Path, caps: &std::path::Path) {
    let script_body = format!(
        "#!/bin/sh\ncp \"$1\" \"{}/captured.${{1##*.}}\"\nexit 0\n",
        caps.display()
    );
    for name in ["xdg-open", "open"] {
        let fake = bin.join(name);
        std::fs::write(&fake, &script_body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let mut cmd = Command::new(cli());
    cmd.arg(db)
        .env("HOME", std::env::temp_dir())
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn cli");
    let mut handle = child.stdin.take().unwrap();
    let text = script.to_string();
    std::thread::spawn(move || {
        let _ = handle.write_all(text.as_bytes());
    });
    let out = child.wait_with_output().expect("wait");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("Failed:"), "opener failed: {stderr}");
}

/// `.log` captures failed statements in sqlite3_log's format.
#[test]
fn log_stream_format() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("cli.log");
    let script = format!(
        ".log {}\nSELECT * FROM nosuch;\n.log off\nSELECT 1;\n.quit\n",
        log.display()
    );
    let (out, err) = run_cli(&[":memory:"], &script);
    assert!(err.contains("no such table: nosuch"), "{err}");
    assert!(out.contains("1"), "{out}");
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(
        logged.starts_with("(1) no such table: nosuch in \"SELECT * FROM nosuch;\""),
        "log content: {logged}"
    );
}

/// `.scanstats` is accepted with the honest not-available warning (like
/// a SQLite build without SQLITE_ENABLE_STMT_SCANSTATUS).
#[test]
fn scanstats_toggle_and_warning() {
    let (_, err) = run_cli(&[":memory:"], ".scanstats on\n.quit\n");
    assert!(
        err.contains("Warning: .scanstats not available in this build."),
        "{err}"
    );
    let (_, err) = run_cli(&[":memory:"], ".scanstats est\n.quit\n");
    assert!(
        err.contains("Warning: .scanstats not available in this build."),
        "{err}"
    );
    let (_, err) = run_cli(&[":memory:"], ".scanstats\n.quit\n");
    assert!(err.contains("Usage: .scanstats on|off|est"), "{err}");
}

/// The `list` and `html` modes render like sqlite3's (raw values joined
/// by `|` — no quoting; html cells with entity escaping, NULL as `null`).
#[test]
fn list_and_html_modes() {
    let script = ".headers off\n.mode list\nSELECT 1 AS a, 'x|y' AS b, NULL AS c;\n\
                  .mode html\nSELECT '<b>&' AS a, NULL AS b;\n\
                  .mode list\n.headers on\nSELECT 1 AS a, 2 AS b;\n\
                  .quit\n";
    let (out, _err) = run_cli(&[":memory:"], script);
    assert!(out.contains("1|x|y|"), "list raw join: {out}");
    assert!(
        out.contains("<TR>\n<TD>&lt;b&gt;&amp;\n<TD>null\n</TR>"),
        "html render: {out}"
    );
    assert!(out.contains("a|b"), "list headers: {out}");
}
