//! Probe: what does REAL SQLite's `VACUUM INTO` write?
//!
//! Checks the output file's header fields (page size, journal-mode bytes
//! 18/19, change counter, schema cookie, encoding) so the engine's
//! native→SQLite `VACUUM INTO` export can match SQLite's own shape.
use rusqlite::Connection;

fn main() {
    let src = std::env::temp_dir().join("rsql_probe_vac_src.db");
    let dst = std::env::temp_dir().join("rsql_probe_vac_out.db");
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);

    let conn = Connection::open(&src).unwrap();
    conn.execute_batch(
        "PRAGMA page_size=8192;
         CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);
         INSERT INTO t(b) VALUES ('x'),('y');
         PRAGMA journal_mode=WAL;",
    )
    .unwrap();
    conn.execute("VACUUM INTO ?1", [dst.to_str().unwrap()])
        .unwrap();

    let bytes = std::fs::read(&dst).unwrap();
    assert_eq!(&bytes[0..16], b"SQLite format 3\0", "magic");
    let ps_be = u16::from_be_bytes([bytes[16], bytes[17]]);
    // Header field 1: big-endian page size; value 1 means 65536.
    let ps = if ps_be == 1 { 65536u32 } else { ps_be as u32 };
    let write_v = u16::from_be_bytes([bytes[18], bytes[19]]);
    println!("source journal_mode = wal");
    println!("output page_size    = {ps} (source was 8192)");
    println!("output header 18/19 = {write_v} (1 = rollback/delete, 2 = wal)");
    println!(
        "output change_ctr   = {}",
        u32::from_be_bytes([bytes[24], bytes[25], bytes[26], bytes[27]])
    );
    println!(
        "output schema_cookie= {}",
        u32::from_be_bytes([bytes[40], bytes[41], bytes[42], bytes[43]])
    );
    println!(
        "output encoding     = {}",
        u32::from_be_bytes([bytes[56], bytes[57], bytes[58], bytes[59]])
    );

    // Re-open the output with SQLite and integrity-check it.
    let check = Connection::open(&dst).unwrap();
    let ok: String = check
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    println!("integrity_check     = {ok}");
    let mode: String = check
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    println!("journal_mode (open) = {mode}");
}
