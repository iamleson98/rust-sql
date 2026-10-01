//! `.archive` / `.ar` — the sqlite3 shell's SQL-archive command
//! (ar.c's surface), riding the engine's real SQL machinery:
//! `fsdir()` walks the disk, `writefile()` extracts, the `zipfile()`
//! vtab mutates zip archives, and sqlar archives are SQLite-format
//! databases opened through the engine's interop container.
//!
//! Formats (oracle-pinned against the real 3.53.4 shell): an EXISTING
//! archive is sniffed by magic (`PK\x03\x04` → zip, `SQLite format 3`
//! → sqlar, anything else → treated as sqlar and failing with the
//! oracle's exact "database does not contain an 'sqlar' table"); a
//! NEW archive defaults to sqlar unless the filename ends in `.zip`.
//! (The oracle's own tar reader is gone in 3.53.4 — junk and tar both
//! land on the sqlar error; we match.)
//!
//! `-n/--dryrun` prints the exact SQL the oracle prints (byte-matched
//! except the random `zipXXXXXXXXXXXXXXXX` instance name, which the
//! oracle also randomizes per run).
//!
//! Divergences (documented in the README ledger): our zip members are
//! always STORED (the no-zlib sqlite3 build's discipline; deflated
//! members are READ byte-exactly through the engine's own inflater),
//! and `.ar -r` on a ZIP archive WORKS here (the oracle's own
//! zip-remove path dies with `sql error: near "WHERE": syntax error`
//! — replicating that bug serves nobody).

use crate::{Runner, ShellOut};
use rustqlite::Value;
use std::io::Write;

pub(crate) const HELP: &str = "\
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
      http://sqlite.org/cli.html#sqlite_archive_support
";

#[derive(Clone, Copy, PartialEq)]
enum ArCmd {
    Create,
    Update,
    Insert,
    Remove,
    List,
    Extract,
}

pub fn run_archive(runner: &mut Runner, args: &[&str], out: &mut ShellOut) -> Result<(), String> {
    if args.iter().any(|a| *a == "--help" || *a == "-help") {
        let _ = writeln!(out, "{}", HELP.trim_end_matches('\n'));
        return Ok(());
    }
    let mut cmd: Option<ArCmd> = None;
    let mut multiple = false;
    let mut verbose = false;
    let mut dryrun = false;
    let mut glob = false;
    let mut file: Option<String> = None;
    let mut directory: Option<String> = None;
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0usize;
    let mut bad: Option<String> = None;
    let mut missing: Option<char> = None;
    while i < args.len() {
        let a = args[i];
        if a == "--" {
            rest.extend(args[i + 1..].iter().map(|s| s.to_string()));
            break;
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, val) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            let takes_value = matches!(name, "file" | "append" | "directory");
            let v = if takes_value {
                match val {
                    Some(v) => Some(v),
                    None => {
                        i += 1;
                        args.get(i).map(|s| s.to_string())
                    }
                }
            } else {
                None
            };
            match name {
                "create" => set_cmd(&mut cmd, ArCmd::Create, &mut multiple),
                "update" => set_cmd(&mut cmd, ArCmd::Update, &mut multiple),
                "insert" => set_cmd(&mut cmd, ArCmd::Insert, &mut multiple),
                "remove" => set_cmd(&mut cmd, ArCmd::Remove, &mut multiple),
                "list" => set_cmd(&mut cmd, ArCmd::List, &mut multiple),
                "extract" => set_cmd(&mut cmd, ArCmd::Extract, &mut multiple),
                "verbose" => verbose = true,
                "dryrun" => dryrun = true,
                "glob" => glob = true,
                "file" | "append" => match v {
                    Some(v) => file = Some(v),
                    None => missing = Some('f'),
                },
                "directory" => match v {
                    Some(v) => directory = Some(v),
                    None => missing = Some('C'),
                },
                _ => bad = Some(name.to_string()),
            }
            if bad.is_some() || missing.is_some() || multiple {
                break;
            }
            i += 1;
            continue;
        }
        if let Some(shorts) = a.strip_prefix('-') {
            let chars: Vec<char> = shorts.chars().collect();
            let mut j = 0usize;
            while j < chars.len() {
                let c = chars[j];
                match c {
                    'c' => set_cmd(&mut cmd, ArCmd::Create, &mut multiple),
                    'u' => set_cmd(&mut cmd, ArCmd::Update, &mut multiple),
                    'i' => set_cmd(&mut cmd, ArCmd::Insert, &mut multiple),
                    'r' => set_cmd(&mut cmd, ArCmd::Remove, &mut multiple),
                    't' => set_cmd(&mut cmd, ArCmd::List, &mut multiple),
                    'x' => set_cmd(&mut cmd, ArCmd::Extract, &mut multiple),
                    'v' => verbose = true,
                    'n' => dryrun = true,
                    'g' => glob = true,
                    'f' | 'a' | 'C' => {
                        let inline: String = chars[j + 1..].iter().collect();
                        let v = if !inline.is_empty() {
                            Some(inline)
                        } else {
                            i += 1;
                            args.get(i).map(|s| s.to_string())
                        };
                        match v {
                            Some(v) => {
                                if c == 'C' {
                                    directory = Some(v);
                                } else {
                                    file = Some(v);
                                }
                            }
                            None => missing = Some(c),
                        }
                        j = chars.len();
                        continue;
                    }
                    _ => bad = Some(c.to_string()),
                }
                if bad.is_some() || multiple {
                    break;
                }
                j += 1;
            }
            if bad.is_some() || missing.is_some() || multiple {
                break;
            }
            i += 1;
            continue;
        }
        rest.push(a.to_string());
        i += 1;
    }
    if let Some(b) = bad {
        eprintln!("Error: unrecognized option: {}", b);
        eprintln!("Use \".archive --help\" for more help");
        return Ok(());
    }
    if let Some(c) = missing {
        eprintln!("Error: option requires an argument: {}", c);
        eprintln!("Use \".archive --help\" for more help");
        return Ok(());
    }
    if multiple {
        eprintln!("Error: multiple command options");
        eprintln!("Use \".archive --help\" for more help");
        return Ok(());
    }
    let cmd = match cmd {
        Some(c) => c,
        None => {
            eprint!("Required argument missing.  Usage:\n{}", HELP);
            return Ok(());
        }
    };

    // Remote sessions: the archive command needs local filesystem
    // access (fsdir/writefile) — refuse like the unsupported set.
    if runner.is_remote() {
        return Err(".archive requires a local session".to_string());
    }

    // shell_putsnl prints through the CLI's writer (ar.c's -v rides
    // the same stream the shell redirects). The sink is installed ONLY
    // for -v; without it shell_putsnl is a silent identity (the SQL
    // keeps its shape, nothing prints).
    let printed: std::sync::Arc<std::sync::Mutex<Vec<u8>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    if verbose {
        let pb = printed.clone();
        let sink: std::sync::Arc<rustqlite::plugin::filefns::PutsSink> =
            std::sync::Arc::new(std::sync::Mutex::new(Box::new(move |s: &str| {
                let mut b = pb.lock().unwrap_or_else(|e| e.into_inner());
                b.extend_from_slice(s.as_bytes());
                b.push(b'\n');
            })));
        rustqlite::plugin::filefns::set_puts_sink(Some(sink));
    }
    let result = match cmd {
        ArCmd::List => run_list(runner, &file, &rest, glob, out),
        ArCmd::Extract => run_extract(runner, &file, &rest, directory, dryrun, glob, out),
        ArCmd::Create | ArCmd::Update | ArCmd::Insert => {
            run_add(runner, &file, &rest, directory, cmd, dryrun, out)
        }
        ArCmd::Remove => run_remove(runner, &file, &rest, dryrun, out),
    };
    rustqlite::plugin::filefns::set_puts_sink(None);
    // Verbose output goes to the shell's stream (before command
    // errors, like the oracle's interleaved printing).
    {
        let buf = printed.lock().unwrap_or_else(|e| e.into_inner());
        if !buf.is_empty() {
            let _ = out.write_all(&buf);
            let _ = out.flush();
        }
    }
    result
}

fn set_cmd(slot: &mut Option<ArCmd>, c: ArCmd, multiple: &mut bool) {
    if slot.is_some() {
        *multiple = true;
        return;
    }
    *slot = Some(c);
}

// ============================================================
// Format sniffing + archive handles.

enum Format {
    Zip,
    Sqlar,
}

fn sniff(path: &str) -> Format {
    if let Ok(mut f) = std::fs::File::open(path) {
        use std::io::Read;
        let mut head = [0u8; 16];
        if f.read_exact(&mut head).is_ok() {
            if head[..4] == [0x50, 0x4b, 0x03, 0x04] {
                return Format::Zip;
            }
            if head[..15] == *b"SQLite format 3" {
                return Format::Sqlar;
            }
        }
    }
    // New archives: `.zip` creates zip; anything else sqlar.
    if path.to_ascii_lowercase().ends_with(".zip") {
        Format::Zip
    } else {
        Format::Sqlar
    }
}

/// Open a sqlar archive as its own database (SQLite-format interop),
/// with the CLI's function set registered (writefile & co).
fn open_sqlar(path: &str) -> Result<Runner, String> {
    let mut db = rustqlite::Database::open_sqlite_format(path).map_err(|_| {
        // The oracle opens lazily and fails at the missing sqlar
        // table for every non-archive — byte-match that surface.
        "database does not contain an 'sqlar' table".to_string()
    })?;
    let _ = rustqlite::plugin::filefns::register(&mut db);
    Ok(Runner::Local(Box::new(db)))
}

// ============================================================
// List

fn run_list(
    runner: &mut Runner,
    file: &Option<String>,
    rest: &[String],
    glob: bool,
    out: &mut ShellOut,
) -> Result<(), String> {
    match file {
        None => list_sqlar(runner, rest, glob, out),
        Some(f) => match sniff(f) {
            Format::Zip => {
                let filter = member_filter(rest, glob);
                let sql = format!(
                    "SELECT name, mode, mtime, sz FROM zipfile({}) WHERE 1{}",
                    quote_str(f),
                    filter
                );
                let (cols, rows) = runner
                    .query(&sql)
                    .map_err(|e| format!("SQL error: {}", e))?;
                emit_list(&cols, &rows, out);
                Ok(())
            }
            Format::Sqlar => {
                let mut db = open_sqlar(f)?;
                list_sqlar(&mut db, rest, glob, out)
            }
        },
    }
}

/// ` AND (name LIKE p OR ...)` / GLOB with -g; empty for no patterns.
fn member_filter(rest: &[String], glob: bool) -> String {
    if rest.is_empty() {
        return String::new();
    }
    let op = if glob { "GLOB" } else { "LIKE" };
    let pats: Vec<String> = rest.iter().map(|p| quote_str(p)).collect();
    format!(
        " AND (name {} {})",
        op,
        pats.join(&format!(" OR name {} ", op))
    )
}

fn list_sqlar(
    db: &mut Runner,
    rest: &[String],
    glob: bool,
    out: &mut ShellOut,
) -> Result<(), String> {
    let sql = format!(
        "SELECT name, mode, mtime, sz FROM sqlar WHERE 1{}",
        member_filter(rest, glob)
    );
    let (cols, rows) = db.query(&sql).map_err(|e| format!("SQL error: {}", e))?;
    emit_list(&cols, &rows, out);
    Ok(())
}

fn emit_list(_cols: &[String], rows: &[Vec<Value>], out: &mut ShellOut) {
    for r in rows {
        let mode = r[1].as_integer();
        let mtime = r[2].as_integer();
        let sz = r[3].as_integer();
        let name = r[0].to_string();
        let _ = writeln!(
            out,
            "{} {:>10}  {}  {}",
            rustqlite::plugin::filefns::ls_mode_pub(mode),
            sz,
            fmt_datetime(mtime),
            name
        );
    }
}

/// UTC `YYYY-MM-DD HH:MM:SS` (the oracle's list rendering).
fn fmt_datetime(t: i64) -> String {
    let days = t.div_euclid(86400);
    let secs = t.rem_euclid(86400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        y,
        m,
        d,
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

// ============================================================
// Extract

fn run_extract(
    runner: &mut Runner,
    file: &Option<String>,
    rest: &[String],
    dir: Option<String>,
    dryrun: bool,
    glob: bool,
    out: &mut ShellOut,
) -> Result<(), String> {
    let dir = dir.unwrap_or_else(|| ".".to_string());
    let (dryrun_lines, exec_sql, handle, src_table) = match file {
        None => {
            let d = extract_dryrun("sqlar", &dir, rest, glob, true);
            let x = extract_exec("sqlar", &dir, rest, glob);
            (d, x, None, "sqlar".to_string())
        }
        Some(f) => match sniff(f) {
            Format::Zip => {
                let src = format!("zipfile({})", quote_str(f));
                let d = extract_dryrun(&src, &dir, rest, glob, false);
                let x = extract_exec(&src, &dir, rest, glob);
                (d, x, None, src)
            }
            Format::Sqlar => {
                let d = extract_dryrun("sqlar", &dir, rest, glob, true);
                let x = extract_exec("sqlar", &dir, rest, glob);
                (d, x, Some(f.clone()), "sqlar".to_string())
            }
        },
    };
    if dryrun {
        if let Some(f) = &handle {
            let _ = writeln!(out, "-- open database '{}'", f);
        }
        for l in &dryrun_lines {
            let _ = writeln!(out, "{}", l);
        }
        // The oracle's extract dryrun then runs the statement with the
        // $dryrun ELSE arm, printing one line per member (its own
        // format output collapses to the member name) — the observable
        // contract: a blank line, then the names.
        let names_sql = {
            let filter = if rest.is_empty() {
                String::new()
            } else {
                member_filter(rest, glob)
            };
            match &handle {
                None => format!("SELECT name FROM {} WHERE (1){filter}", src_table),
                Some(_) => format!("SELECT name FROM sqlar WHERE (1){filter}"),
            }
        };
        let names: Vec<String> = match &handle {
            None => runner
                .query(&names_sql)
                .map(|(_, rows)| rows.iter().map(|r| r[0].to_string()).collect())
                .unwrap_or_default(),
            Some(f) => open_sqlar(f)
                .and_then(|db| db.query(&names_sql))
                .map(|(_, rows)| rows.iter().map(|r| r[0].to_string()).collect())
                .unwrap_or_default(),
        };
        let _ = writeln!(out);
        for n in &names {
            let _ = writeln!(out, "{}", n);
        }
        return Ok(());
    }
    match &handle {
        None => {
            let _ = runner
                .query(&exec_sql)
                .map_err(|e| format!("SQL error: {}", e))?;
        }
        Some(f) => {
            let db = open_sqlar(f)?;
            let _ = db
                .query(&exec_sql)
                .map_err(|e| format!("SQL error: {}", e))?;
        }
    }
    Ok(())
}

/// The oracle's extract dryrun script (byte-shape): the WITH dest(...)
/// CTE, the $-parameters, the symlink-pass split, and the path-safety
/// GLOB.
fn extract_dryrun(
    src: &str,
    _dir: &str,
    rest: &[String],
    glob: bool,
    is_sqlar: bool,
) -> Vec<String> {
    let uncompress = if is_sqlar {
        "sqlar_uncompress(data, sz)"
    } else {
        "data"
    };
    let filter = if rest.is_empty() {
        String::new()
    } else {
        let op = if glob { "GLOB" } else { "LIKE" };
        let pats: Vec<String> = rest.iter().map(|p| quote_str(p)).collect();
        format!(
            " AND (name {} {})",
            op,
            pats.join(&format!(" OR name {} ", op))
        )
    };
    vec![
        "WITH dest(dpath,dlen) AS (".to_string(),
        "  SELECT realpath($dir) || '/',".to_string(),
        "  1+length(realpath($dir))".to_string(),
        ")".to_string(),
        "SELECT".to_string(),
        "    ($dir || name),".to_string(),
        "    CASE $dryrun".to_string(),
        "      WHEN 0 THEN writefile($dir||name, data, mode, mtime)".to_string(),
        "      WHEN 1 THEN 0".to_string(),
        format!(
            "      ELSE shell_putsnl(format('writefile(%Q,%s,%0o,%d)',$dir||name,quote({uncompress}),mode,mtime)) IS NULL"
        ),
        "      END".to_string(),
        format!("  FROM dest CROSS JOIN {src}"),
        " WHERE (1)".to_string(),
        "   AND (CASE $pass WHEN 0 THEN (mode&0xf000)<>0xa000".to_string(),
        "                   WHEN 1 THEN (mode&0xf000)=0xa000".to_string(),
        "                   ELSE data IS NULL END)".to_string(),
        "   AND dpath=substr(realpath($dir||name),1,dlen)".to_string(),
        "   AND name NOT GLOB '*..[/\\]*'".to_string(),
    ]
    .into_iter()
    .chain(
        // Member filters land as extra conjuncts on the WHERE.
        if filter.is_empty() {
            Vec::new()
        } else {
            vec![format!("   AND {}",
                filter.trim_start_matches(" AND (").trim_end_matches(')'))]
        },
    )
    .collect()
}

/// The executed statement: the same shape with parameters inlined and
/// the two-pass symlink split run as two statements (pass 0 files,
/// pass 1 symlinks) — writefile does the work, and the surrounding
/// shell prints names via the puts sink when -v is on.
fn extract_exec(src: &str, dir: &str, rest: &[String], glob: bool) -> String {
    // Single statement, non-symlinks then symlinks handled by mode
    // filter in writefile's arguments (writefile creates files; a
    // symlink member's writefile would write a regular file — the
    // oracle makes real symlinks via its own pass; we write the
    // member bytes as a regular file, a documented simplification).
    let filter = if rest.is_empty() {
        String::new()
    } else {
        member_filter(rest, glob)
    };
    let uncompress = if src == "sqlar" {
        "sqlar_uncompress(data, sz)"
    } else {
        "data"
    };
    let dir_q = quote_str(&format!("{}/", dir.trim_end_matches('/')));
    format!(
        "SELECT shell_putsnl(name), writefile({dir_q} || name, {uncompress}, mode, mtime) \
         FROM {src} WHERE (1){filter} \
         AND name NOT GLOB '*..[/\\]*'"
    )
}

// ============================================================
// Create / Update / Insert

fn run_add(
    runner: &mut Runner,
    file: &Option<String>,
    rest: &[String],
    dir: Option<String>,
    cmd: ArCmd,
    dryrun: bool,
    out: &mut ShellOut,
) -> Result<(), String> {
    match file {
        None => {
            // The current database becomes/updates the sqlar archive.
            let (display, exec) = build_sqlar_script(rest, &dir, cmd, None);
            if dryrun {
                for s in &display {
                    let _ = writeln!(out, "{}", s);
                }
                return Ok(());
            }
            for stmt in exec {
                runner
                    .execute(&stmt)
                    .map_err(|e| format!("SQL error: {}", e))?;
            }
            Ok(())
        }
        Some(f) => match sniff(f) {
            Format::Zip => {
                let (display, exec) = build_zip_script(f, rest, &dir, cmd);
                if dryrun {
                    for s in &display {
                        let _ = writeln!(out, "{}", s);
                    }
                    return Ok(());
                }
                for stmt in exec {
                    runner
                        .execute(&stmt)
                        .map_err(|e| format!("SQL error: {}", e))?;
                }
                Ok(())
            }
            Format::Sqlar => {
                let (display, exec) = build_sqlar_script(rest, &dir, cmd, Some(f));
                if dryrun {
                    for s in &display {
                        let _ = writeln!(out, "{}", s);
                    }
                    return Ok(());
                }
                let mut db = open_sqlar(f)?;
                for stmt in exec {
                    db.execute(&stmt).map_err(|e| format!("SQL error: {}", e))?;
                }
                Ok(())
            }
        },
    }
}

const SQLAR_DDL: &str = "CREATE TABLE IF NOT EXISTS sqlar(\n  name TEXT PRIMARY KEY,  -- name of the file\n  mode INT,               -- access permissions\n  mtime INT,              -- last modification time\n  sz INT,                 -- original file size\n  data BLOB               -- compressed content\n)";

/// (dryrun display lines, executed statements). The display matches
/// the oracle's script; execution drops the PRAGMA (the interop
/// container owns its page size) and the comments-only statements.
fn build_sqlar_script(
    rest: &[String],
    dir: &Option<String>,
    cmd: ArCmd,
    file: Option<&str>,
) -> (Vec<String>, Vec<String>) {
    let mut display = Vec::new();
    let mut exec = Vec::new();
    if let Some(f) = file {
        display.push(format!("-- open database '{}'", f));
    }
    display.push("PRAGMA page_size=512".to_string());
    display.push("SAVEPOINT ar;".to_string());
    exec.push("SAVEPOINT ar".to_string());
    if cmd == ArCmd::Create {
        display.push("DROP TABLE IF EXISTS sqlar".to_string());
        display.push(SQLAR_DDL.to_string());
        exec.push("DROP TABLE IF EXISTS sqlar".to_string());
    }
    exec.push(SQLAR_DDL.to_string());
    display.push(SQLAR_DDL.to_string());
    for arg in rest {
        let d2 = match dir {
            Some(d) => quote_str(d),
            None => "NULL".to_string(),
        };
        let extra = if cmd == ArCmd::Create {
            String::new()
        } else {
            " AND NOT EXISTS(SELECT 1 FROM sqlar AS mem WHERE mem.name=disk.name AND mem.mtime=disk.mtime AND mem.mode=disk.mode)".to_string()
        };
        let stmt = format!(
            "REPLACE INTO sqlar(name,mode,mtime,sz,data)\n  SELECT\n    shell_putsnl(name),\n    mode,\n    mtime,\n    CASE substr(lsmode(mode),1,1)\n      WHEN '-' THEN length(data)\n      WHEN 'd' THEN 0\n      ELSE -1 END,\n    sqlar_compress(data)\n  FROM fsdir({}, {d2}) AS disk\n  WHERE lsmode(mode) NOT LIKE '?%'{extra};",
            quote_str(arg)
        );
        display.push(stmt.clone());
        exec.push(stmt);
    }
    display.push("RELEASE ar;".to_string());
    exec.push("RELEASE ar".to_string());
    (display, exec)
}

/// Zip scripts: the temp zipfile vtab machinery.
fn build_zip_script(
    file: &str,
    rest: &[String],
    dir: &Option<String>,
    cmd: ArCmd,
) -> (Vec<String>, Vec<String>) {
    let inst = format!("zip{:016x}", rand_hex16());
    let create = format!(
        "CREATE VIRTUAL TABLE temp.{inst} USING zipfile({})",
        quote_str(file)
    );
    let mut display = vec![
        "PRAGMA page_size=512".to_string(),
        "SAVEPOINT ar;".to_string(),
        create.clone(),
    ];
    let mut exec = vec!["SAVEPOINT ar".to_string(), create];
    for arg in rest {
        let d2 = match dir {
            Some(d) => quote_str(d),
            None => "NULL".to_string(),
        };
        let extra = if cmd == ArCmd::Create {
            String::new()
        } else {
            format!(
                " AND NOT EXISTS(SELECT 1 FROM {inst} AS mem WHERE mem.name=disk.name AND mem.mtime=disk.mtime AND mem.mode=disk.mode)"
            )
        };
        let stmt = format!(
            "REPLACE INTO {inst}(name,mode,mtime,data)\n  SELECT\n    shell_putsnl(name),\n    mode,\n    mtime,\n    data\n  FROM fsdir({}, {d2}) AS disk\n  WHERE lsmode(mode) NOT LIKE '?%'{extra};",
            quote_str(arg)
        );
        display.push(stmt.clone());
        exec.push(stmt);
    }
    display.push("RELEASE ar;".to_string());
    display.push(format!("DROP TABLE {inst}"));
    exec.push("RELEASE ar".to_string());
    exec.push(format!("DROP TABLE {inst}"));
    (display, exec)
}

// ============================================================
// Remove

fn run_remove(
    runner: &mut Runner,
    file: &Option<String>,
    rest: &[String],
    dryrun: bool,
    out: &mut ShellOut,
) -> Result<(), String> {
    if rest.is_empty() {
        eprint!("Required argument missing.  Usage:\n{}", HELP);
        return Ok(());
    }
    let predicate = |tbl: &str| -> String {
        let mut conds: Vec<String> = Vec::new();
        for n in rest {
            let q = quote_str(n);
            conds.push(format!(
                "(name = {q} OR (name GLOB '*/*' AND substr(name,1,{len}) = {q2}))",
                len = n.chars().count() + 1,
                q2 = quote_str(&format!("{}/", n)),
            ));
        }
        format!("DELETE FROM {} WHERE {} ;", tbl, conds.join(" OR "))
    };
    match file {
        None => {
            let sql = predicate("sqlar");
            if dryrun {
                let _ = writeln!(out, "{}", sql);
                return Ok(());
            }
            runner
                .execute(&sql)
                .map_err(|e| format!("SQL error: {}", e))?;
            Ok(())
        }
        Some(f) => match sniff(f) {
            Format::Zip => {
                // The oracle's zip-remove path errors (its generated
                // SQL is broken); ours works through the temp vtab.
                let inst = format!("zip{:016x}", rand_hex16());
                let create = format!(
                    "CREATE VIRTUAL TABLE temp.{inst} USING zipfile({})",
                    quote_str(f)
                );
                let del = predicate(&inst);
                if dryrun {
                    for s in [
                        "SAVEPOINT ar;".to_string(),
                        create.clone(),
                        del.to_string(),
                        "RELEASE ar;".to_string(),
                        format!("DROP TABLE {inst}"),
                    ] {
                        let _ = writeln!(out, "{}", s);
                    }
                    return Ok(());
                }
                for stmt in [
                    "SAVEPOINT ar".to_string(),
                    create,
                    del,
                    "RELEASE ar".to_string(),
                    format!("DROP TABLE {inst}"),
                ] {
                    runner
                        .execute(&stmt)
                        .map_err(|e| format!("SQL error: {}", e))?;
                }
                Ok(())
            }
            Format::Sqlar => {
                let sql = predicate("sqlar");
                if dryrun {
                    let _ = writeln!(out, "-- open database '{}'", f);
                    let _ = writeln!(out, "{}", sql);
                    return Ok(());
                }
                let mut db = open_sqlar(f)?;
                db.execute(&sql).map_err(|e| format!("SQL error: {}", e))?;
                Ok(())
            }
        },
    }
}

// ============================================================
// Helpers

fn quote_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn rand_hex16() -> u64 {
    // A per-run random instance name (the oracle uses
    // sqlite3_randomness; any 64-bit source serves).
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let p = std::process::id() as u64;
    t ^ (p << 32) ^ (t >> 17)
}
