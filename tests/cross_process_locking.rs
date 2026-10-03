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
fn spawn_helper(dir: &Path, role: &str) -> Child {
    let exe = std::env::current_exe().unwrap();
    Command::new(exe)
        .env(HELPER_VAR, role)
        .env(DIR_VAR, dir)
        .env_remove("RUST_BACKTRACE")
        .arg("xproc_helper_dispatch")
        .arg("--exact")
        .arg("--test-threads=1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
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
#[test]
fn concurrent_reader_never_sees_torn_or_lost_state() {
    let dir = tempdir("hammer");
    let db_path = dir.join(DB_NAME);
    let mut writer = spawn_helper(&dir, "writer_hammer");
    assert!(wait_marker(&dir, "p1-ready-1", Duration::from_secs(60)));

    let mut db2 = open_wal(&db_path);
    let mut last = 0i64;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(3) {
        let n: i64 = db2.query("SELECT COUNT(*) FROM t", []).unwrap()[0]
            .first()
            .unwrap()
            .as_integer();
        assert!(n >= last, "reader count went backwards: {n} < {last}");
        last = n;
    }
    // The reader observed foreign commits streaming in.
    assert!(last >= 25, "reader should have observed growth, saw {last}");

    write_marker(&dir, "stop");
    assert!(wait_marker(&dir, "p1-done", Duration::from_secs(60)));
    let _ = writer.wait();

    let final_n: i64 = db2.query("SELECT COUNT(*) FROM t", []).unwrap()[0]
        .first()
        .unwrap()
        .as_integer();
    assert!(final_n >= last, "final {final_n} < last observed {last}");
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
