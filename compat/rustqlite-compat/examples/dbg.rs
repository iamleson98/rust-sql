// Minimal repro: bind + step through the C ABI
use std::ffi::{c_char, c_int, CString};
use std::ptr;

extern crate sqlite3 as compat;

extern "C" {
    fn sqlite3_open_v2(filename: *const c_char, ppdb: *mut *mut compat::sqlite3, flags: c_int, z: *const c_char) -> c_int;
    fn sqlite3_close(db: *mut compat::sqlite3) -> c_int;
    fn sqlite3_exec(db: *mut compat::sqlite3, sql: *const c_char, cb: *const u8, a: *const u8, e: *const u8) -> c_int;
    fn sqlite3_prepare_v3(db: *mut compat::sqlite3, s: *const c_char, n: c_int, f: c_int, pp: *mut *mut compat::sqlite3_stmt, t: *mut *const c_char) -> c_int;
    fn sqlite3_finalize(s: *mut compat::sqlite3_stmt) -> c_int;
    fn sqlite3_bind_int64(s: *mut compat::sqlite3_stmt, i: c_int, v: i64) -> c_int;
    fn sqlite3_step(s: *mut compat::sqlite3_stmt) -> c_int;
    fn sqlite3_column_text(s: *mut compat::sqlite3_stmt, i: c_int) -> *const u8;
    fn sqlite3_errmsg(db: *mut compat::sqlite3) -> *const c_char;
    fn sqlite3_column_count(s: *mut compat::sqlite3_stmt) -> c_int;
}

fn main() {
    unsafe {
        let name = CString::new(":memory:").unwrap();
        let mut db: *mut compat::sqlite3 = ptr::null_mut();
        sqlite3_open_v2(name.as_ptr(), &mut db, 0x6, ptr::null());
        let sql = CString::new("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT); INSERT INTO t (name) VALUES ('a'),('b'),('c')").unwrap();
        let rc = sqlite3_exec(db, sql.as_ptr(), ptr::null(), ptr::null(), ptr::null());
        println!("exec rc={}", rc);

        let qs = CString::new("SELECT name FROM t WHERE id = ?").unwrap();
        let mut stmt: *mut compat::sqlite3_stmt = ptr::null_mut();
        let mut tail: *const c_char = ptr::null();
        let rc = sqlite3_prepare_v3(db, qs.as_ptr(), -1, 0, &mut stmt, &mut tail);
        println!("prepare rc={} cols={}", rc, sqlite3_column_count(stmt));
        let rc = sqlite3_bind_int64(stmt, 1, 2);
        println!("bind rc={}", rc);
        let rc = sqlite3_step(stmt);
        println!("step rc={} (ROW=100)", rc);
        if rc == 100 {
            let p = sqlite3_column_text(stmt, 0);
            let s = std::ffi::CStr::from_ptr(p as *const c_char).to_string_lossy();
            println!("col0 = {}", s);
        } else {
            let e = sqlite3_errmsg(db);
            println!("err: {}", std::ffi::CStr::from_ptr(e).to_string_lossy());
        }
        sqlite3_finalize(stmt);
        sqlite3_close(db);
    }
}
