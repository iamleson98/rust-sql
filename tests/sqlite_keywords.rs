//! rustqlite's copy of SQLite's keyword list (identifiers spelled like one
//! are quoted in generated DDL) must equal the bundled SQLite's.

#[test]
fn keyword_list_matches_sqlite() {
    let n = unsafe { libsqlite3_sys::sqlite3_keyword_count() };
    let mut theirs: Vec<String> = (0..n)
        .map(|i| {
            let mut p: *const std::os::raw::c_char = std::ptr::null();
            let mut len: std::os::raw::c_int = 0;
            unsafe {
                libsqlite3_sys::sqlite3_keyword_name(i, &mut p, &mut len);
                String::from_utf8(std::slice::from_raw_parts(p as *const u8, len as usize).to_vec())
                    .unwrap()
            }
        })
        .collect();
    theirs.sort();
    assert_eq!(theirs, rustqlite::sql::lexer::SQLITE_KEYWORDS);
}
