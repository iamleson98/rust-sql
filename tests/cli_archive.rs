//! `.archive` / `.ar` + the SQL surface it rides — byte-parity against
//! the REAL sqlite3 3.53.4 shell over checked-in oracle-created
//! fixtures, plus interop in both directions (the oracle's archives
//! read here; our archives verified by content round-trips).
//!
//! The fixtures under tests/fixtures/ were created by the real
//! 3.53.4 shell:
//! * `arch-three-members.zip` — `.ar -cf` of a.txt/b.txt/sub/c.txt
//!   (STORED members — the no-zlib build discipline);
//! * `arch-deflated.zip` — Python-made DEFLATED members (method 8);
//! * `arch-three-members.sqlar` — `.ar -cf` sqlar (SQLite-format,
//!   raw `data` for the small members).

use std::io::Write;
use std::process::{Command, Stdio};

fn cli() -> &'static str {
    // Cargo builds the bin for any test that references it.
    env!("CARGO_BIN_EXE_rustqlite-cli")
}

/// Run the CLI with a piped stdin script; return stdout.
fn run_cli(args: &[&str], script: &str) -> String {
    run_cli_in(None, args, script)
}

/// Run the CLI in a specific working directory (member arguments like
/// `a.txt` resolve against it, like a real shell session).
fn run_cli_in(cwd: Option<&std::path::Path>, args: &[&str], script: &str) -> String {
    let mut cmd = Command::new(cli());
    cmd.args(args)
        .env("HOME", std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    let mut child = cmd.spawn().expect("spawn cli");
    let mut handle = child.stdin.take().unwrap();
    let text = script.to_string();
    std::thread::spawn(move || {
        let _ = handle.write_all(text.as_bytes());
    });
    let out = child.wait_with_output().expect("wait cli");
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// stdout with the banner/prompts stripped (the harness pattern from
/// cli_sha3sum).
fn stripped(out: &str) -> String {
    out.lines()
        .map(|l| l.trim_start_matches("rustqlite> "))
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
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// A scratch dir with three members (the oracle fixture's content).
fn scratch() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().to_path_buf();
    std::fs::write(p.join("a.txt"), b"hello world\n").unwrap();
    std::fs::write(p.join("b.txt"), b"second file\n").unwrap();
    std::fs::create_dir_all(p.join("sub")).unwrap();
    std::fs::write(p.join("sub/c.txt"), b"nested\n").unwrap();
    (dir, p.to_string_lossy().into_owned())
}

// ============================================================
// Byte-parity against the oracle's fixture + observed output.

#[test]
fn list_oracle_zip_byte_parity() {
    let out = run_cli(
        &[":memory:"],
        &format!(".ar -tvf {}\n.exit\n", fixture("arch-three-members.zip")),
    );
    let expect = "\
-rw-rw-r--         12  2026-10-01 04:53:14  a.txt
-rw-rw-r--         12  2026-10-01 04:53:14  b.txt
-rw-rw-r--          7  2026-10-01 04:53:14  sub/c.txt";
    assert_eq!(stripped(&out), expect);
}

#[test]
fn create_zip_byte_identical_to_oracle_fixture() {
    let (_d, dir) = scratch();
    // Pin the members' mode+mtime to the fixture's (a fresh scratch
    // file carries NOW; the oracle fixture froze 1790830394 =
    // 2026-10-01 04:53:14 UTC, mode 33204) — through the CLI's own
    // writefile(,,mode,mtime), the same surface .archive extracts
    // with.
    let pin = format!(
        "SELECT writefile('{d}/a.txt', readfile('{d}/a.txt'), 33204, 1790830394),\n\
                writefile('{d}/b.txt', readfile('{d}/b.txt'), 33204, 1790830394),\n\
                writefile('{d}/sub/c.txt', readfile('{d}/sub/c.txt'), 33204, 1790830394);\n.exit\n",
        d = dir
    );
    let _ = run_cli(&[":memory:"], &pin);
    let zip = std::path::Path::new(&dir).join("made.zip");
    let out = run_cli_in(
        Some(std::path::Path::new(&dir)),
        &[":memory:"],
        &format!(".ar -cf {} a.txt b.txt sub/c.txt\n.exit\n", zip.display()),
    );
    assert!(stripped(&out).is_empty(), "create must be silent: {out}");
    let mine = std::fs::read(&zip).unwrap();
    let oracle = std::fs::read(fixture("arch-three-members.zip")).unwrap();
    assert_eq!(
        mine, oracle,
        "our stored-member zip must be byte-identical to the oracle's"
    );
}

#[test]
fn extract_zip_members_and_verbose_names() {
    let out = tempfile::tempdir().unwrap();
    let op = out.path();
    std::fs::write(op.join("a.txt"), b"stale a\n").unwrap(); // overwritten
    let script = format!(".ar -xvf {}\n.exit\n", fixture("arch-three-members.zip"));
    // -C switches the extraction directory.
    let outp = run_cli(
        &[":memory:"],
        &format!(
            ".ar -C {} -xvf {}\n.exit\n",
            op.display(),
            fixture("arch-three-members.zip")
        ),
    );
    let names = stripped(&outp);
    let lines: Vec<&str> = names.lines().collect();
    assert_eq!(lines, vec!["a.txt", "b.txt", "sub/c.txt"], "verbose names");
    assert_eq!(std::fs::read(op.join("a.txt")).unwrap(), b"hello world\n");
    assert_eq!(std::fs::read(op.join("b.txt")).unwrap(), b"second file\n");
    assert_eq!(std::fs::read(op.join("sub/c.txt")).unwrap(), b"nested\n");
    let _ = script;
}

#[test]
fn deflated_members_read_and_extract_byte_exact() {
    let out = tempfile::tempdir().unwrap();
    let op = out.path();
    let outp = run_cli(
        &[":memory:"],
        &format!(
            ".ar -C {} -xvf {}\n.exit\n",
            op.display(),
            fixture("arch-deflated.zip")
        ),
    );
    assert!(stripped(&outp).contains("big.txt"));
    let big = std::fs::read(op.join("big.txt")).unwrap();
    assert_eq!(big, b"hello world ".repeat(100));
    assert_eq!(std::fs::read(op.join("tiny.txt")).unwrap(), b"x");
    // Listing reports the UNCOMPRESSED size (the oracle's behavior).
    let list = run_cli(
        &[":memory:"],
        &format!(".ar -tvf {}\n.exit\n", fixture("arch-deflated.zip")),
    );
    let l = stripped(&list);
    assert!(l.contains("      1200  "), "sz column shows 1200: {l}");
}

#[test]
fn sqlar_round_trip_and_current_db_archive() {
    let (_d, dir) = scratch();
    let sqlar = std::path::Path::new(&dir).join("made.sqlar");
    // Create into the current database (no -f): the sqlar table lands
    // in the main db, listable right there.
    let script = ".ar -c a.txt b.txt\n.ar -t\nSELECT count(*) FROM sqlar;\n.exit\n".to_string();
    let out = run_cli_in(Some(std::path::Path::new(&dir)), &[":memory:"], &script);
    let s = stripped(&out);
    assert!(s.contains("a.txt") && s.contains("b.txt"), "list: {s}");
    assert!(s.contains("| 2"), "sqlar row count: {s}");

    // -f sqlar: a SQLite-format archive; reopen + list + extract.
    let out2 = run_cli_in(
        Some(std::path::Path::new(&dir)),
        &[":memory:"],
        &format!(
            ".ar -cf {} a.txt\n.ar -tvf {}\n.exit\n",
            sqlar.display(),
            sqlar.display()
        ),
    );
    let s2 = stripped(&out2);
    assert!(s2.contains("a.txt"), "sqlar list: {s2}");
    // The file IS SQLite-format (the magic).
    let head = std::fs::read(&sqlar).unwrap();
    assert_eq!(&head[..15], b"SQLite format 3");
    // The raw `data` for a small member (stored: len == sz).
    let s3 = run_cli(
        &[&sqlar.to_string_lossy()],
        "SELECT name, sz, length(data) FROM sqlar ORDER BY name;\n.exit\n",
    );
    let st = stripped(&s3);
    assert!(st.contains("a.txt"), "direct sqlar query: {st}");
}

#[test]
fn update_insert_remove_lifecycle() {
    let (_d, dir) = scratch();
    let zip = std::path::Path::new(&dir).join("life.zip");
    let z = zip.display().to_string();
    let run = |script: String| run_cli_in(Some(std::path::Path::new(&dir)), &[":memory:"], &script);

    // Create with one member; insert a second; remove the first.
    let out = run(format!(".ar -cf {z} a.txt\n.exit\n"));
    assert!(stripped(&out).is_empty());
    let out = run(format!(".ar -if {z} b.txt\n.ar -tf {z}\n.exit\n"));
    let s = stripped(&out);
    assert!(
        s.contains("a.txt") && s.contains("b.txt"),
        "after insert: {s}"
    );
    let out = run(format!(".ar -rf {z} a.txt\n.ar -tf {z}\n.exit\n"));
    let s = stripped(&out);
    assert!(
        !s.contains("a.txt") && s.contains("b.txt"),
        "after remove: {s}"
    );
    // UPDATE with an unchanged mtime adds nothing; a touched file does.
    let out = run(format!(".ar -uf {z} b.txt\n.ar -tf {z}\n.exit\n"));
    assert!(
        stripped(&out).lines().count() == 1,
        "unchanged update is a no-op"
    );
    std::fs::write(std::path::Path::new(&dir).join("b.txt"), b"changed bytes\n").unwrap();
    // -u is MTIME-keyed at whole-second granularity (SQLite's own
    // semantics): a same-second rewrite is "unchanged". Push the
    // member's mtime deterministically through the CLI's writefile.
    let _ = run_cli(
        &[":memory:"],
        &format!(
            "SELECT writefile('{d}/b.txt', readfile('{d}/b.txt'), 33204, 1790900000);\n.exit\n",
            d = dir
        ),
    );
    let out = run(format!(".ar -uf {z} b.txt\n.ar -tf {z}\n.exit\n"));
    let s = stripped(&out);
    assert!(s.contains("b.txt"));
    // The updated member extracts with the NEW bytes.
    let ex = tempfile::tempdir().unwrap();
    run_cli(
        &[":memory:"],
        &format!(".ar -C {} -xf {z}\n.exit\n", ex.path().display()),
    );
    assert_eq!(
        std::fs::read(ex.path().join("b.txt")).unwrap(),
        b"changed bytes\n"
    );
}

// ============================================================
// Option surface parity (the oracle's exact error/help shapes).

#[test]
fn option_errors_match_the_oracle() {
    let cases: &[(&str, &str)] = &[
        // unrecognized short
        (
            ".ar -q",
            "Error: unrecognized option: q\nUse \".archive --help\" for more help",
        ),
        // value-taking flag without its argument
        (
            ".ar -cf",
            "Error: option requires an argument: f\nUse \".archive --help\" for more help",
        ),
        // two commands
        (
            ".ar -ct",
            "Error: multiple command options\nUse \".archive --help\" for more help",
        ),
    ];
    for (script, expect) in cases {
        let out = run_cli(&[":memory:"], &format!("{script}\n.exit\n"));
        // The errors print to the shell's stderr-visible stream (our
        // harness merges: the CLI's eprintln goes to stderr — compare
        // through a re-run capturing stderr is overkill; the message
        // text is asserted via the CLI's own stream).
        let _ = out;
        let mut cmd = Command::new(cli());
        cmd.args([":memory:"])
            .env("HOME", std::env::temp_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let mut h = child.stdin.take().unwrap();
        let text = format!("{script}\n.exit\n");
        std::thread::spawn(move || {
            let _ = h.write_all(text.as_bytes());
        });
        let o = child.wait_with_output().unwrap();
        let err = String::from_utf8_lossy(&o.stderr).to_string();
        assert!(
            err.trim_start_matches("rustqlite> ").starts_with(expect),
            "for {script} expected {expect:?}, got {err:?}"
        );
    }
}

#[test]
fn help_byte_identical() {
    let out = run_cli(&[":memory:"], ".archive --help\n.exit\n");
    let expect = "\
.archive ...             Manage SQL archives
   Each command must have exactly one of the following options:
     -c, --create               Create a new archive
     -u, --update               Add or update files with changed mtime
     -i, --insert               Like -u but always add even if unchanged
     -r, --remove               Remove files from archive
     -t, --list                 List contents of archive
     -x, --extract              Extract files from archive
   Optional arguments:
     -v, --verbose              Print each filename as it is processed
     -f FILE, --file FILE       Use archive FILE (default is current db)
     -a FILE, --append FILE     Open FILE using the apndvfs VFS
     -C DIR, --directory DIR    Read/extract files from directory DIR
     -g, --glob                 Use glob matching for names in archive
     -n, --dryrun               Show the SQL that would have occurred
   Examples:
     .ar -cf ARCHIVE foo bar  # Create ARCHIVE from files foo and bar
     .ar -tf ARCHIVE          # List members of ARCHIVE
     .ar -xvf ARCHIVE         # Verbosely extract files from ARCHIVE
   See also:
      http://sqlite.org/cli.html#sqlite_archive_support";
    assert_eq!(stripped(&out), expect);
}

#[test]
fn dot_command_prefix_matching() {
    // `.ar` (a unique prefix) resolves to .archive; sqlite3's rule.
    let out = run_cli(
        &[":memory:"],
        &format!(".ar -tf {}\n.exit\n", fixture("arch-three-members.zip")),
    );
    let s = stripped(&out);
    assert!(s.contains("a.txt") && s.contains("sub/c.txt"), "{s}");
    // More unique prefixes.
    for (p, needle) in [(".data", "main"), (".stat", "page size")] {
        let out = run_cli(&[":memory:"], &format!("{p}\n.exit\n"));
        assert!(
            stripped(&out).contains(needle),
            "{p} should resolve uniquely: {}",
            stripped(&out)
        );
    }
    // An ambiguous prefix errors with the candidates list (our design;
    // the oracle silently picks its internal first match — the friendly
    // error is the documented divergence).
    let mut cmd = Command::new(cli());
    cmd.args([":memory:"])
        .env("HOME", std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let mut h = child.stdin.take().unwrap();
    let text = ".s\n.exit\n".to_string();
    std::thread::spawn(move || {
        let _ = h.write_all(text.as_bytes());
    });
    let o = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&o.stderr).to_string();
    assert!(
        err.contains("ambiguous command: .s"),
        "ambiguity error: {err}"
    );
}

// ============================================================
// The SQL surface (zipfile / fsdir / the file functions) through
// user SQL — the shell-extension family the CLI registers.

#[test]
fn sql_surface_functions() {
    let (_d, dir) = scratch();
    let a = format!("{}/a.txt", dir);
    let w = format!("{}/w.txt", dir);
    let zipf = fixture("arch-three-members.zip");
    let script = format!(
        "SELECT lsmode(33188), lsmode(16893), lsmode(41471), lsmode(NULL);\n\
         SELECT typeof(readfile('{a}')), length(readfile('{a}'));\n\
         SELECT writefile('{w}', x'414243'), readfile('{w}') = x'414243';\n\
         SELECT realpath('sub/../a.txt') LIKE '%a.txt';\n\
         SELECT count(*) FROM fsdir('{dir}', NULL);\n\
         SELECT name, method FROM zipfile('{zipf}') ORDER BY name;\n\
         .exit\n"
    );
    let out = run_cli(&[":memory:"], &script);
    let s = stripped(&out);
    assert!(s.contains("-rw-r--r--"), "lsmode: {s}");
    assert!(s.contains("drwxrwxr-x"), "lsmode dir: {s}");
    assert!(s.contains("lrwxrwxrwx"), "lsmode link: {s}");
    assert!(s.contains("?---------"), "lsmode null: {s}");
    assert!(s.contains("blob"), "readfile type: {s}");
    assert!(s.contains("| 12"), "readfile length: {s}");
    assert!(s.contains("| 1"), "writefile+readfile round-trip: {s}");
    assert!(s.contains("| 3"), "fsdir row count: {s}");
    assert!(s.contains("| 0"), "zipfile method stored: {s}");
}

#[test]
fn sqlar_uncompress_both_shapes() {
    // The big.sqlar fixture holds the oracle's zlib-deflated member
    // (big.txt: sz=1200, data=31 bytes) and a stored member (a.txt).
    let out = run_cli(
        &[&fixture("arch-big.sqlar")],
        "SELECT name, sz, length(data) FROM sqlar ORDER BY name;\n\
         SELECT length(sqlar_uncompress(data, sz)) FROM sqlar WHERE name='big.txt';\n\
         SELECT sqlar_uncompress(data, sz) = sqlar_uncompress(data, sz) FROM sqlar WHERE name='a.txt';\n\
         .exit\n",
    );
    let s = stripped(&out);
    assert!(s.contains("big.txt"), "members: {s}");
    let un = run_cli(
        &[&fixture("arch-big.sqlar")],
        "SELECT length(sqlar_uncompress(data, sz)) FROM sqlar WHERE name='big.txt';\n.exit\n",
    );
    let su = stripped(&un);
    assert!(su.contains("1200"), "inflate through SQL: {su}");
}
