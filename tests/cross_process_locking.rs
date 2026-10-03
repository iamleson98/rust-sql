//! Cross-process read/write concurrency for WAL-mode native files
//! (storage::xlock).
//!
//! Every test spawns REAL child processes (this test binary re-exec'd
//! with `--exact xproc_helper_dispatch`), because the whole point of the
//! xlock protocol is behavior that no in-process test can observe: OS
//! byte-range locks, kernel lock release on process death, and
//! committed-WAL visibility across open file descriptions.
//!
//! Coordination is filesystem-based (portable): the parent creates a
//! tempdir, the child (role via env) writes marker files the parent
//! polls for.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use rustqlite::{Database, Value};

// ---------------------------------------------------------------------------
// helper plumbing
// ---------------------------------------------------------------------------

const HELPER_VAR: &str = "RSQL_XPROC_HELPER";
const DIR_VAR: &str = "RSQL_XPROC_DIR";
const DB_NAME: &str = "xproc.db";

fn marker(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.marker"))
}

fn write_marker(dir: &Path, name: &str) {
    std::fs::write(marker(dir, name), b"ok").unwrap();
}

fn wait_marker(dir: &Path, name: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if marker(dir, name).exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    false
}

fn wait_gone(dir: &Path, name: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !marker(dir, name).exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    false
}

/// Spawn this test binary in helper mode running ONLY the dispatcher.
/// Under `RSQL_XPROC_TRACE=1` the helpers INHERIT stderr instead of
/// null-ing it, so a failing run's captured output tells BOTH sides'
/// story (the reader's probe decisions in-process, the writer's
/// checkpoint deferrals from the child) — see `xlock_trace!` in
/// storage/pager.rs.
fn spawn_helper(dir: &Path, role: &str) -> Child {
    let exe = std::env::current_exe().unwrap();
    let trace = std::env::var("RSQL_XPROC_TRACE").is_ok();
    if trace {
        eprintln!(
            "[harness pid{}] spawning helper {role} (stdio inherited)",
            std::process::id()
        );
    }
    let stdio = || {
        if trace {
            std::process::Stdio::inherit()
        } else {
            std::process::Stdio::null()
        }
    };
    Command::new(exe)
        .env(HELPER_VAR, role)
        .env(DIR_VAR, dir)
        .env_remove("RUST_BACKTRACE")
        .arg("xproc_helper_dispatch")
        .arg("--exact")
        .arg("--test-threads=1")
        .stdout(stdio())
        .stderr(stdio())
        .spawn()
        .expect("spawn helper")
}

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rsql-xproc-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn open_wal(db_path: &Path) -> Database {
    let mut db = Database::open(db_path).unwrap();
    db.execute("PRAGMA journal_mode=WAL", []).unwrap();
    db
}

/// One-line state dump for the hammer test's failure messages: the
/// writer child's liveness, its highest `p1-count-N` marker (committed
/// progress), and the sidecar's path-stat — enough to tell a DEAD
/// writer (helper panic — its stderr is inherited only under
/// RSQL_XPROC_TRACE) from a FROZEN reader (markers advancing, the
/// reader's count stuck).
fn hammer_state(dir: &Path, writer: &mut std::process::Child) -> String {
    let mut max_marker = 0i64;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if let Some(n) = name
                .strip_prefix("p1-count-")
                // marker() appends ".marker" to every name.
                .and_then(|s| s.trim_end_matches(".marker").parse::<i64>().ok())
            {
                max_marker = max_marker.max(n);
            }
        }
    }
    let sidecar = dir.join(format!("{DB_NAME}-wal"));
    let sc = std::fs::metadata(&sidecar)
        .map(|m| format!("len={}", m.len()))
        .unwrap_or_else(|e| format!("MISSING ({e})"));
    let child = match writer.try_wait() {
        Ok(Some(st)) => format!("EXITED ({st})"),
        Ok(None) => "alive".to_string(),
        Err(e) => format!("try_wait error: {e}"),
    };
    format!("writer={child}, max p1-count marker={max_marker}, sidecar {sc}")
}

// ---------------------------------------------------------------------------
// the dispatcher test (children run ONLY this)
// ---------------------------------------------------------------------------

#[test]
fn xproc_helper_dispatch() {
    let Some(role) = std::env::var(HELPER_VAR).ok() else {
        return;
    };
    let dir = PathBuf::from(std::env::var(DIR_VAR).unwrap());
    let db_path = dir.join(DB_NAME);
    match role.as_str() {
        "writer_solo" => {
            // Create, commit, hold the writer open, wait for the parent's
            // signal, commit more, wait again, exit cleanly.
            let mut db = open_wal(&db_path);
            db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
                .unwrap();
            db.execute("INSERT INTO t (v) VALUES ('one')", []).unwrap();
            write_marker(&dir, "p1-ready-1");
            assert!(wait_marker(&dir, "go-1", Duration::from_secs(30)));
            db.execute("INSERT INTO t (v) VALUES ('two')", []).unwrap();
            write_marker(&dir, "p1-ready-2");
            assert!(wait_gone(&dir, "p1-ready-2", Duration::from_secs(60)));
        }
        "writer_hammer" => {
            // Take the writer role and commit continuously until the
            // parent says stop (drives auto-checkpoint thresholds too).
            let mut db = open_wal(&db_path);
            db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
                .unwrap();
            let mut i = 0i64;
            write_marker(&dir, "p1-ready-1");
            while !marker(&dir, "stop").exists() {
                db.execute(
                    "INSERT INTO t (v) VALUES (?)",
                    [Value::Text(format!("v{i}").into())],
                )
                .unwrap();
                i += 1;
                if i % 25 == 0 {
                    write_marker(&dir, &format!("p1-count-{i}"));
                }
            }
            write_marker(&dir, "p1-done");
        }
        "crash_writer" => {
            // Take the writer role, signal, then sleep forever — the
            // parent SIGKILLs us (kernel lock release is the test).
            let mut db = open_wal(&db_path);
            db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
                .unwrap();
            db.execute("INSERT INTO t (v) VALUES ('committed')", [])
                .unwrap();
            drop(db);
            let mut db2 = open_wal(&db_path);
            db2.execute("INSERT INTO t (v) VALUES ('held')", [])
                .unwrap();
            write_marker(&dir, "p1-ready-1");
            std::thread::sleep(Duration::from_secs(300));
        }
        "plain_reader" => {
            // Open read-mostly, hold the file open until told to go.
            let db = open_wal(&db_path);
            let _ = db.query("SELECT COUNT(*) FROM sqlite_master", []);
            write_marker(&dir, "p2-ready");
            assert!(wait_gone(&dir, "p2-ready", Duration::from_secs(60)));
        }
        _ => panic!("unknown role {role}"),
    }
    // Helper exit: markers already written; lock bytes die with the
    // process (or release on drop for the clean ones).
    std::process::exit(0);
}

// ---------------------------------------------------------------------------
// real tests (parents)
// ---------------------------------------------------------------------------

/// A second PROCESS sees the first process's committed writes, live —
/// the freshness probe picks up sidecar growth at statement boundaries.
#[test]
fn two_process_reader_sees_committed_writes() {
    let dir = tempdir("live-read");
    let db_path = dir.join(DB_NAME);
    let mut child = spawn_helper(&dir, "writer_solo");
    assert!(
        wait_marker(&dir, "p1-ready-1", Duration::from_secs(60)),
        "child never created the table"
    );

    // Second process, open handle held across the child's next commit.
    let db2 = open_wal(&db_path);
    let n: i64 = db2.query("SELECT COUNT(*) FROM t", []).unwrap()[0]
        .first()
        .unwrap()
        .as_integer();
    assert_eq!(n, 1, "second process must see the first commit at open");

    std::fs::remove_file(marker(&dir, "go-1")).ok();
    write_marker(&dir, "go-1");
    assert!(wait_marker(&dir, "p1-ready-2", Duration::from_secs(30)));

    // SAME handle, NEW statement: the freshness probe must absorb the
    // child's second commit without any reopen.
    let n2: i64 = db2.query("SELECT COUNT(*) FROM t", []).unwrap()[0]
        .first()
        .unwrap()
        .as_integer();
    assert_eq!(n2, 2, "freshness probe must see the foreign commit");
    let vals: Vec<String> = db2
        .query("SELECT v FROM t ORDER BY id", [])
        .unwrap()
        .iter()
        .map(|r| r.first().unwrap().as_text())
        .collect();
    assert_eq!(vals, vec!["one".to_string(), "two".to_string()]);

    std::fs::remove_file(marker(&dir, "p1-ready-2")).unwrap();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// While another process holds the WRITE byte, a second process's writes
/// fail with the busy class — never corruption. After the writer exits,
/// the second process writes.
#[test]
fn second_process_writer_gets_busy_not_corruption() {
    let dir = tempdir("busy");
    let db_path = dir.join(DB_NAME);
    let mut child = spawn_helper(&dir, "writer_solo");
    assert!(wait_marker(&dir, "p1-ready-1", Duration::from_secs(60)));

    let mut db2 = open_wal(&db_path);
    // Reads flow freely.
    let n: i64 = db2.query("SELECT COUNT(*) FROM t", []).unwrap()[0]
        .first()
        .unwrap()
        .as_integer();
    assert_eq!(n, 1);

    // Writes: busy-class rejection (cross-process writer lease).
    let err = db2
        .execute("INSERT INTO t (v) VALUES ('tres')", [])
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("SQLITE_BUSY"),
        "expected SQLITE_BUSY, got: {msg}"
    );
    assert!(
        msg.contains("another process") || msg.contains("ANOTHER process"),
        "the busy error must point at the cross-process writer: {msg}"
    );

    // Writer exits; the second process's write now succeeds.
    std::fs::remove_file(marker(&dir, "go-1")).ok();
    write_marker(&dir, "go-1");
    assert!(wait_marker(&dir, "p1-ready-2", Duration::from_secs(30)));
    std::fs::remove_file(marker(&dir, "p1-ready-2")).unwrap();
    let _ = child.wait();

    db2.execute("INSERT INTO t (v) VALUES ('tres')", [])
        .unwrap();
    let n: i64 = db2.query("SELECT COUNT(*) FROM t", []).unwrap()[0]
        .first()
        .unwrap()
        .as_integer();
    assert_eq!(n, 3);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A SIGKILLed writer's locks evaporate (kernel-owned) and the next
/// process takes over cleanly: no busy-forever, no corruption, the
/// committed prefix survives, the torn tail (if any) is discarded.
#[test]
fn killed_writer_releases_locks_and_state_is_consistent() {
    let dir = tempdir("crash");
    let db_path = dir.join(DB_NAME);
    let mut child = spawn_helper(&dir, "crash_writer");
    assert!(wait_marker(&dir, "p1-ready-1", Duration::from_secs(60)));

    // The killed process holds WRITE + LIVE at kill time.
    child.kill().unwrap();
    let _ = child.wait();

    // Next process: opens, writes, reads — everything must just work.
    let mut db = open_wal(&db_path);
    let n: i64 = db.query("SELECT COUNT(*) FROM t", []).unwrap()[0]
        .first()
        .unwrap()
        .as_integer();
    assert_eq!(n, 2, "both pre-kill commits survive (WAL recovery)");
    db.execute("INSERT INTO t (v) VALUES ('after-crash')", [])
        .unwrap();
    db.execute("PRAGMA integrity_check", []).unwrap();
    let n: i64 = db.query("SELECT COUNT(*) FROM t", []).unwrap()[0]
        .first()
        .unwrap()
        .as_integer();
    assert_eq!(n, 3);
    drop(db);

    // And a THIRD open (fresh) sees the same state.
    let db3 = open_wal(&db_path);
    let n: i64 = db3.query("SELECT COUNT(*) FROM t", []).unwrap()[0]
        .first()
        .unwrap()
        .as_integer();
    assert_eq!(n, 3);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Continuous writer process + continuous reader process: the reader's
/// count is a snapshot of committed state at each statement (>= every
/// count it observed before, and the final count matches after the
/// writer drains).
///
/// The growth floor keys on the writer's OWN `p1-count-25` marker, not
/// a wall-clock: on a loaded shared runner (the oom-injection config
/// runs the whole suite on the System allocator, and this binary runs
/// up to ~20 processes — a parent and hammering helper per test — on
/// 2 cores) the writer's commit pace collapses to single digits per
/// second; a fixed "25 rows in 3 seconds" threshold is a runner-load
/// lottery, not a correctness claim. The marker wait keeps the REAL
/// assertions — monotonicity on every statement, freshness under
/// hammering, final-state agreement — load-independent.
#[test]
fn concurrent_reader_never_sees_torn_or_lost_state() {
    let dir = tempdir("hammer");
    let db_path = dir.join(DB_NAME);
    let mut writer = spawn_helper(&dir, "writer_hammer");
    assert!(wait_marker(&dir, "p1-ready-1", Duration::from_secs(60)));

    let mut db2 = open_wal(&db_path);
    let mut last = 0i64;
    // Phase 1 — read continuously until the writer's 25-row marker lands
    // (up to 60s on a starved runner): every statement's count must be >=
    // the previous one (the torn/lost-state contract).
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(60) {
        if std::env::var("RSQL_SLOW_READER").is_ok() {
            std::thread::sleep(Duration::from_millis(30));
        }
        let n: i64 = db2.query("SELECT COUNT(*) FROM t", []).unwrap()[0]
            .first()
            .unwrap()
            .as_integer();
        assert!(n >= last, "reader count went backwards: {n} < {last}");
        last = n;
        if last >= 25 && marker(&dir, "p1-count-25").exists() {
            break;
        }
    }
    assert!(
        last >= 25,
        "reader should have observed growth, saw {last} [{}]",
        hammer_state(&dir, &mut writer)
    );
    // Phase 2 — one more second of overlap reading (monotonicity under
    // continued hammering), then drain.
    let t1 = Instant::now();
    while t1.elapsed() < Duration::from_secs(1) {
        let n: i64 = db2.query("SELECT COUNT(*) FROM t", []).unwrap()[0]
            .first()
            .unwrap()
            .as_integer();
        assert!(n >= last, "reader count went backwards: {n} < {last}");
        last = n;
    }

    write_marker(&dir, "stop");
    assert!(
        wait_marker(&dir, "p1-done", Duration::from_secs(60)),
        "writer never drained [{}]",
        hammer_state(&dir, &mut writer)
    );
    let _ = writer.wait();

    let final_n: i64 = db2.query("SELECT COUNT(*) FROM t", []).unwrap()[0]
        .first()
        .unwrap()
        .as_integer();
    assert!(
        final_n >= last,
        "final {final_n} < last observed {last} [{}]",
        hammer_state(&dir, &mut writer)
    );
    db2.execute("PRAGMA integrity_check", []).unwrap();

    // Fresh open: same count (the sidecar or the folded main file —
    // either way one consistent committed state).
    let db3 = open_wal(&db_path);
    let n3: i64 = db3.query("SELECT COUNT(*) FROM t", []).unwrap()[0]
        .first()
        .unwrap()
        .as_integer();
    assert_eq!(n3, final_n);
    let _ = std::fs::remove_dir_all(&dir);
}

/// While a READER process holds the file open, the writer's close-time
/// fold DEFERS (the sole-handle gate): the sidecar survives, the data
/// stays correct through the reader's handle, and the next sole open
/// folds it.
#[test]
fn close_time_checkpoint_defers_while_reader_open() {
    let dir = tempdir("defer");
    let db_path = dir.join(DB_NAME);
    let mut writer = spawn_helper(&dir, "writer_hammer");
    assert!(wait_marker(&dir, "p1-ready-1", Duration::from_secs(60)));

    let mut reader = spawn_helper(&dir, "plain_reader");
    assert!(wait_marker(&dir, "p2-ready", Duration::from_secs(60)));

    // Stop the writer: its close-time fold must DEFER (reader holds the
    // LIVE byte), leaving committed frames durable in the sidecar.
    write_marker(&dir, "stop");
    assert!(wait_marker(&dir, "p1-done", Duration::from_secs(60)));
    let _ = writer.wait();
    std::thread::sleep(Duration::from_millis(200));
    let sidecar = {
        let mut p = db_path.as_os_str().to_os_string();
        p.push("-wal");
        PathBuf::from(p)
    };
    assert!(
        sidecar.exists(),
        "deferred fold must leave the sidecar (frames durable)"
    );

    // The reader still reads consistent state through its open handle.
    // (It will exit when its marker is removed.)
    std::fs::remove_file(marker(&dir, "p2-ready")).unwrap();
    let _ = reader.wait();

    // Next sole open: recovery replays the sidecar; the close-time fold
    // lands; the state is identical.
    let mut db = open_wal(&db_path);
    let n: i64 = db.query("SELECT COUNT(*) FROM t", []).unwrap()[0]
        .first()
        .unwrap()
        .as_integer();
    assert!(n > 0);
    db.execute("PRAGMA integrity_check", []).unwrap();
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// In-process second handle on a WAL file: reads track the first
/// handle's commits (the freshness probe's in-process bonus), and the
/// second handle's WRITE still fails with the in-process busy class.
#[test]
fn in_process_second_handle_reads_track_first_handle_commits() {
    let dir = tempdir("inproc");
    let db_path = dir.join(DB_NAME);
    let mut h1 = open_wal(&db_path);
    h1.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    h1.execute("INSERT INTO t (v) VALUES ('a')", []).unwrap();

    let mut h2 = open_wal(&db_path);
    let n: i64 = h2.query("SELECT COUNT(*) FROM t", []).unwrap()[0]
        .first()
        .unwrap()
        .as_integer();
    assert_eq!(n, 1);

    h1.execute("INSERT INTO t (v) VALUES ('b')", []).unwrap();
    let n: i64 = h2.query("SELECT COUNT(*) FROM t", []).unwrap()[0]
        .first()
        .unwrap()
        .as_integer();
    assert_eq!(n, 2, "second handle freshness must track h1's commit");

    // Second handle writes: busy (in-process writer lease).
    let err = h2
        .execute("INSERT INTO t (v) VALUES ('c')", [])
        .unwrap_err();
    assert!(format!("{err}").contains("SQLITE_BUSY"));

    // First handle keeps working; after it closes, h2 may write.
    drop(h1);
    h2.execute("INSERT INTO t (v) VALUES ('c')", []).unwrap();
    let n: i64 = h2.query("SELECT COUNT(*) FROM t", []).unwrap()[0]
        .first()
        .unwrap()
        .as_integer();
    assert_eq!(n, 3);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A sole handle's explicit `PRAGMA wal_checkpoint` succeeds (no other
/// handles open anywhere).
#[test]
fn sole_checkpoint_succeeds() {
    let dir = tempdir("ckpt");
    let db_path = dir.join(DB_NAME);
    let mut db = open_wal(&db_path);
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 0..200 {
        db.execute(
            "INSERT INTO t (v) VALUES (?)",
            [Value::Text(format!("v{i}").into())],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db.execute("PRAGMA wal_checkpoint", []).unwrap();
    let n: i64 = db.query("SELECT COUNT(*) FROM t", []).unwrap()[0]
        .first()
        .unwrap()
        .as_integer();
    assert_eq!(n, 200);
    db.execute("PRAGMA integrity_check", []).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A sidecar that RESET (shrank below the reader's horizon) under a live
/// reader must never freeze that reader at its stale horizon — the
/// length-only freshness probe read the post-reset, still-regrowing log
/// as "no growth" forever (the macOS concurrent_reader freeze: reader
/// stuck at 88 rows while a fresh open saw 5473).
///
/// The reset is produced the LEGITIMATE way — folded main file + fresh
/// 32-byte header — by a sole checkpoint on a COPY of the database, then
/// transplanted over the live reader's files (exactly the state a foreign
/// checkpoint leaves behind when the sole gate cannot block it). The
/// regrowth after the transplant is deliberately SMALLER than the
/// pre-reset log so the post-reset file length stays below the reader's
/// stale expected length — the exact freeze shape: the old probe's
/// `len <= expected` returned "no growth" on every statement for the
/// rest of the handle's life.
#[test]
fn reader_survives_foreign_sidecar_reset_with_smaller_regrowth() {
    let dir = tempdir("resetfreeze");
    let db_path = dir.join(DB_NAME);
    let sidecar = {
        let mut p = db_path.as_os_str().to_os_string();
        p.push("-wal");
        PathBuf::from(p)
    };

    // Writer fills the log (150 autocommit INSERTs -> ~300 frames), and
    // the reader absorbs a first view: horizon == the full log.
    let mut h1 = open_wal(&db_path);
    h1.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    for i in 0..150 {
        h1.execute(
            "INSERT INTO t (v) VALUES (?)",
            [Value::Text(format!("v{i}").into())],
        )
        .unwrap();
    }
    let mut h2 = open_wal(&db_path);
    let n: i64 = h2.query("SELECT COUNT(*) FROM t", []).unwrap()[0]
        .first()
        .unwrap()
        .as_integer();
    assert_eq!(n, 150);
    drop(h1); // close-time fold DEFERS (h2 live): the log keeps ~300 frames.

    // The foreign checkpoint, run the only legitimate way: on a COPY
    // with no other handles. The copy's fold lands in ITS main file and
    // its sidecar resets to a fresh 32-byte header (new salt).
    let copy_dir = dir.join("copy");
    std::fs::create_dir_all(&copy_dir).unwrap();
    let copy_db = copy_dir.join(DB_NAME);
    let copy_sidecar = {
        let mut p = copy_db.as_os_str().to_os_string();
        p.push("-wal");
        PathBuf::from(p)
    };
    std::fs::copy(&db_path, &copy_db).unwrap();
    std::fs::copy(&sidecar, &copy_sidecar).unwrap();
    {
        let mut hc = open_wal(&copy_db);
        hc.execute("PRAGMA wal_checkpoint", []).unwrap();
        let n: i64 = hc.query("SELECT COUNT(*) FROM t", []).unwrap()[0]
            .first()
            .unwrap()
            .as_integer();
        assert_eq!(n, 150, "the copy must fold to the full state");
        // The transplant happens while hc still holds the copy open: a
        // sole handle's close-time teardown REMOVES its (already reset)
        // sidecar, and the transplant needs those fresh-header bytes.
        std::fs::copy(&copy_db, &db_path).unwrap();
        std::fs::copy(&copy_sidecar, &sidecar).unwrap();
        drop(hc);
    }
    assert!(
        std::fs::metadata(&sidecar).unwrap().len() <= 4096,
        "post-reset sidecar must be the fresh-header shape, not the old log"
    );

    // Regrowth SMALLER than the pre-reset log: the post-reset file stays
    // below the reader's stale expected length — the freeze shape.
    {
        let mut h3 = open_wal(&db_path);
        for i in 0..30 {
            h3.execute(
                "INSERT INTO t (v) VALUES (?)",
                [Value::Text(format!("w{i}").into())],
            )
            .unwrap();
        }
        let n: i64 = h3.query("SELECT COUNT(*) FROM t", []).unwrap()[0]
            .first()
            .unwrap()
            .as_integer();
        assert_eq!(n, 180);
        drop(h3);
    }

    // THE CONTRACT: the live reader sees the post-reset truth. The old
    // probe served the stale memoized 150 here (its expected length was
    // the pre-reset log's, the regrown file never passed it, and no
    // reopen ever happened).
    let n2: i64 = h2.query("SELECT COUNT(*) FROM t", []).unwrap()[0]
        .first()
        .unwrap()
        .as_integer();
    assert_eq!(n2, 180, "a reset sidecar must not freeze a live reader");
    // And stays consistent on the next statement.
    let n2b: i64 = h2.query("SELECT COUNT(*) FROM t", []).unwrap()[0]
        .first()
        .unwrap()
        .as_integer();
    assert_eq!(n2b, 180);
    h2.execute("PRAGMA integrity_check", []).unwrap();

    // A fresh open agrees.
    let h4 = open_wal(&db_path);
    let n4: i64 = h4.query("SELECT COUNT(*) FROM t", []).unwrap()[0]
        .first()
        .unwrap()
        .as_integer();
    assert_eq!(n4, 180);
    drop(h4);
    drop(h2);
    let _ = std::fs::remove_dir_all(&dir);
}
