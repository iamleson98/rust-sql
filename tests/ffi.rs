//! C ABI tests: the `rustqlite_*` FFI family and dynamic extension
//! loading (C, C++, Zig, and Rust-compiled .so plugins).
//!
//! The extension binaries are built by `tests/build_plugins.sh` (also run
//! in CI before cargo test). Missing binaries skip their tests.

use rustqlite::ffi::*;
use rustqlite::Database;

fn plugin_path(name: &str) -> Option<std::path::PathBuf> {
    let candidates = [
        format!("plugins/{name}"),
        format!("../plugins/{name}"),
        format!("../../plugins/{name}"),
    ];
    candidates
        .into_iter()
        .map(std::path::PathBuf::from)
        .find(|p| p.exists())
}

// ---------------------------------------------------------------------------
// rustqlite_* C API
// ---------------------------------------------------------------------------

#[test]
fn ffi_open_exec_prepare_step() {
    unsafe {
        let mut db: *mut RqlConn = std::ptr::null_mut();
        assert_eq!(rustqlite_open(c":memory:".as_ptr(), &mut db), RQL_OK);
        assert!(!db.is_null());

        assert_eq!(
            rustqlite_exec(
                db,
                c"CREATE TABLE t (id INTEGER PRIMARY KEY, x TEXT)".as_ptr()
            ),
            RQL_OK
        );
        assert_eq!(
            rustqlite_exec(db, c"INSERT INTO t (x) VALUES ('a'), ('b'), ('c')".as_ptr()),
            RQL_OK
        );

        let mut stmt: *mut RqlStmt = std::ptr::null_mut();
        let rc = rustqlite_prepare_v2(
            db,
            c"SELECT id, x FROM t".as_ptr(),
            -1,
            &mut stmt,
            std::ptr::null_mut(),
        );
        assert_eq!(rc, RQL_OK);
        assert!(!stmt.is_null());
        assert_eq!(rustqlite_column_count(stmt), 2);

        let mut saw = Vec::new();
        loop {
            let rc = rustqlite_step(stmt);
            if rc == RQL_ROW {
                let id = rustqlite_column_int64(stmt, 0);
                let txt = rustqlite_column_text(stmt, 1);
                let s = std::ffi::CStr::from_ptr(txt as *const std::ffi::c_char)
                    .to_string_lossy()
                    .into_owned();
                saw.push((id, s));
            } else {
                assert_eq!(rc, RQL_DONE);
                break;
            }
        }
        assert_eq!(saw.len(), 3);
        assert_eq!(saw[0], (1, "a".to_string()));
        assert_eq!(saw[2], (3, "c".to_string()));
        assert_eq!(rustqlite_finalize(stmt), RQL_OK);
        assert_eq!(rustqlite_close(db), RQL_OK);
    }
}

#[test]
fn ffi_bind_and_types() {
    unsafe {
        let mut db: *mut RqlConn = std::ptr::null_mut();
        rustqlite_open(c":memory:".as_ptr(), &mut db);
        rustqlite_exec(
            db,
            c"CREATE TABLE t (i INTEGER, r REAL, s TEXT, b BLOB)".as_ptr(),
        );
        let mut stmt: *mut RqlStmt = std::ptr::null_mut();
        rustqlite_prepare_v2(
            db,
            c"INSERT INTO t VALUES (?, ?, ?, ?)".as_ptr(),
            -1,
            &mut stmt,
            std::ptr::null_mut(),
        );
        assert_eq!(rustqlite_bind_parameter_count(stmt), 4);
        rustqlite_bind_int64(stmt, 1, 42);
        rustqlite_bind_double(stmt, 2, 2.5);
        rustqlite_bind_text(stmt, 3, c"hello".as_ptr(), -1, None);
        let blob: [u8; 3] = [1, 2, 3];
        rustqlite_bind_blob(stmt, 4, blob.as_ptr() as *const std::ffi::c_void, 3, None);
        assert_eq!(rustqlite_step(stmt), RQL_DONE); // 101
        rustqlite_reset(stmt);
        rustqlite_finalize(stmt);

        let mut q: *mut RqlStmt = std::ptr::null_mut();
        rustqlite_prepare_v2(
            db,
            c"SELECT i, r, s, b FROM t".as_ptr(),
            -1,
            &mut q,
            std::ptr::null_mut(),
        );
        assert_eq!(rustqlite_step(q), RQL_ROW);
        assert_eq!(rustqlite_column_type(q, 0), RQL_INTEGER);
        assert_eq!(rustqlite_column_int64(q, 0), 42);
        assert_eq!(rustqlite_column_type(q, 1), RQL_FLOAT);
        assert!((rustqlite_column_double(q, 1) - 2.5).abs() < 1e-12);
        assert_eq!(rustqlite_column_type(q, 2), RQL_TEXT);
        let n = rustqlite_column_bytes(q, 2);
        assert_eq!(n, 5);
        let blob_ptr = rustqlite_column_blob(q, 3);
        let blob_len = rustqlite_column_bytes(q, 3);
        let got = std::slice::from_raw_parts(blob_ptr as *const u8, blob_len as usize);
        assert_eq!(got, &[1, 2, 3]);
        rustqlite_finalize(q);
        rustqlite_close(db);
    }
}

#[test]
fn ffi_close_refuses_live_statements() {
    unsafe {
        let mut db: *mut RqlConn = std::ptr::null_mut();
        rustqlite_open(c":memory:".as_ptr(), &mut db);
        rustqlite_exec(db, c"CREATE TABLE t (x)".as_ptr());
        let mut stmt: *mut RqlStmt = std::ptr::null_mut();
        rustqlite_prepare_v2(
            db,
            c"SELECT x FROM t".as_ptr(),
            -1,
            &mut stmt,
            std::ptr::null_mut(),
        );
        // Statement alive → close returns MISUSE (SQLite's SQLITE_BUSY).
        assert_eq!(rustqlite_close(db), RQL_MISUSE);
        rustqlite_finalize(stmt);
        assert_eq!(rustqlite_close(db), RQL_OK);
    }
}

#[test]
fn ffi_libversion() {
    unsafe {
        let v = std::ffi::CStr::from_ptr(rustqlite_libversion())
            .to_string_lossy()
            .into_owned();
        assert_eq!(v, env!("CARGO_PKG_VERSION"));
    }
}

// ---------------------------------------------------------------------------
// Dynamic extension loading (C / C++ / Zig / Rust .so plugins)
// ---------------------------------------------------------------------------

#[test]
fn load_c_extension() {
    let Some(so) = plugin_path("c/rot13.so") else {
        eprintln!("skipping: plugins/c/rot13.so not built");
        return;
    };
    let mut db = Database::open_in_memory().unwrap();
    db.load_extension(&so, None).unwrap();
    // Scalar function registered by the C plugin.
    let rows = db.query("SELECT rot13('hello')", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "uryyb");
    // Aggregate registered by the C plugin (sum of squares).
    db.execute("CREATE TABLE t (x INTEGER)", []).unwrap();
    db.execute("INSERT INTO t (x) VALUES (1), (2), (3)", [])
        .unwrap();
    let rows = db.query("SELECT sumsq(x) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_real(), 14.0);
    // Collation.
    let rows = db.query("SELECT 'nop' < 'abc' COLLATE ROT13", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 1); // rot13('nop')='abc' < rot13('abc')='nop'
                                            // Virtual table from the C plugin.
    db.execute("CREATE VIRTUAL TABLE s USING series(5)", [])
        .unwrap();
    let rows = db.query("SELECT count(*) FROM s", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 6);
}

#[test]
fn load_cpp_extension() {
    let Some(so) = plugin_path("cpp/example.so") else {
        eprintln!("skipping: plugins/cpp/example.so not built");
        return;
    };
    let mut db = Database::open_in_memory().unwrap();
    db.load_extension(&so, None).unwrap();
    // Scalar.
    let rows = db.query("SELECT shout('hello')", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "HELLO!");
    // Aggregate with std::deque state.
    db.execute("CREATE TABLE t (x INTEGER)", []).unwrap();
    db.execute("INSERT INTO t (x) VALUES (2), (4), (9)", [])
        .unwrap();
    let rows = db.query("SELECT movavg(x) FROM t", []).unwrap();
    assert!(
        (rows[0][0].as_real() - 5.0).abs() < 1e-12,
        "movavg = {}",
        rows[0][0].as_real()
    );
    // Collation: numeric-aware ordering (2 < 10 numerically, but
    // '10' < '2' in BINARY byte order).
    let rows = db.query("SELECT '2' < '10' COLLATE NUMERIC", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 1);
    let rows = db.query("SELECT '2' < '10'", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 0); // BINARY: '10' < '2'
                                            // Writable vtab from C++.
    db.execute("CREATE VIRTUAL TABLE kv USING kvstore()", [])
        .unwrap();
    db.execute("INSERT INTO kv (k, v) VALUES ('a', '1'), ('b', '2')", [])
        .unwrap();
    let rows = db.query("SELECT v FROM kv WHERE k = 'b'", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "2");
    db.execute("UPDATE kv SET v = '20' WHERE k = 'b'", [])
        .unwrap();
    let rows = db.query("SELECT v FROM kv WHERE k = 'b'", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "20");
    db.execute("DELETE FROM kv WHERE k = 'a'", []).unwrap();
    let rows = db.query("SELECT count(*) FROM kv", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 1);
}

#[test]
fn load_zig_extension() {
    let Some(so) = plugin_path("zig/librot13.so") else {
        eprintln!("skipping: plugins/zig/librot13.so not built");
        return;
    };
    let mut db = Database::open_in_memory().unwrap();
    db.load_extension(&so, None).unwrap();
    let rows = db.query("SELECT rot13('hello')", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "uryyb");
    // Aggregate.
    db.execute("CREATE TABLE t (x INTEGER)", []).unwrap();
    db.execute("INSERT INTO t (x) VALUES (1), (NULL), (3)", [])
        .unwrap();
    let rows = db.query("SELECT zcount(x) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 2);
    // Collation.
    let rows = db.query("SELECT 'ba' < 'ab' COLLATE ZREVERSE", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 1); // reversed: 'ab' vs 'ba' → 'ab' < 'ba' → 'ba' sorts first → 'ba' < 'ab' = true
                                            // Virtual table with args.
    db.execute("CREATE VIRTUAL TABLE rng USING zrange(7)", [])
        .unwrap();
    let rows = db.query("SELECT count(*) FROM rng", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 7);
}

#[test]
fn load_rust_extension() {
    let Some(so) = plugin_path("rust/target/release/librustext.so") else {
        eprintln!("skipping: plugins/rust/target/release/librustext.so not built");
        return;
    };
    let mut db = Database::open_in_memory().unwrap();
    db.load_extension(&so, None).unwrap();
    // Variadic scalar: revsum(1,2,3) = 6 → reversed "6" = 6.
    let rows = db.query("SELECT revsum(1, 2, 3)", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 6);
    // revsum(12, 30) = 42 → "24".
    let rows = db.query("SELECT revsum(12, 30)", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 24);
    // Aggregate.
    db.execute("CREATE TABLE t (x INTEGER)", []).unwrap();
    db.execute("INSERT INTO t (x) VALUES (2), (3), (4)", [])
        .unwrap();
    let rows = db.query("SELECT product(x) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_real(), 24.0);
    // vtab.
    db.execute("CREATE VIRTUAL TABLE m USING mirror(5)", [])
        .unwrap();
    let rows = db.query("SELECT count(*) FROM m", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 5);
    let rows = db.query("SELECT label FROM m WHERE n = 2", []).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].as_text(), "mirror-2");
}

#[test]
fn load_extension_bad_path_and_entry() {
    let mut db = Database::open_in_memory().unwrap();
    let err = db
        .load_extension("/nonexistent/plugin.so", None)
        .unwrap_err();
    assert!(err.to_string().contains("load_extension"));
    // Valid file, missing entry point.
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("fake.so");
    std::fs::write(&fake, b"not a shared library").unwrap();
    assert!(db.load_extension(&fake, None).is_err());
}
