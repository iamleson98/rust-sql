//! Probe (not committed as a test): establish REAL SQLite 3.x semantics for
//! `PRAGMA auto_vacuum` that the engine must mirror:
//!   1. read value on a fresh db (0?)
//!   2. write on an EMPTY db (round-trip? what does the read return?)
//!   3. write on a NON-EMPTY db (error? silent ignore? value unchanged?)
//!   4. invalid values (error? ignored?)
//!   5. `PRAGMA auto_vacuum = FULL` header bytes 52..56 / 64..68 afterward
//!   6. what INCREMENTAL writes into offset 64
//!   7. does `integrity_check` require largest-root <= pagecount? create
//!      table after our future dump shape (largest_root == n_pages).

use rusqlite::Connection;

fn main() {
    let in_mem = Connection::open_in_memory().unwrap();
    let av: i64 = in_mem
        .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
        .unwrap();
    println!("1. fresh in-memory auto_vacuum = {av}");

    let av: i64 = in_mem
        .query_row("PRAGMA auto_vacuum = FULL", [], |r| r.get(0))
        .unwrap_or(-1);
    println!("2. write form on empty db RETURNS row = {av} (rusqlite: rows changed)");
    let av: i64 = in_mem
        .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
        .unwrap();
    println!("   read-after-write = {av}");
    in_mem.execute("CREATE TABLE t(a)", []).unwrap();
    let av: i64 = in_mem
        .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
        .unwrap();
    println!("   read after CREATE TABLE = {av}");

    // 3. write on NON-empty db.
    let r: Result<(), rusqlite::Error> = in_mem.execute_batch("PRAGMA auto_vacuum = NONE;");
    println!("3. execute_batch non-empty set NONE -> {r:?}");
    let av: i64 = in_mem
        .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
        .unwrap();
    println!("   value after attempted set NONE = {av}");

    // 4. invalid value.
    let r = in_mem.execute_batch("PRAGMA auto_vacuum = 9;");
    println!("4. invalid value 9 -> {r:?}");
    let r = in_mem.execute_batch("PRAGMA auto_vacuum = BANANA;");
    println!("   invalid word BANANA -> {r:?}");

    // 5/6. header bytes for FULL vs INCREMENTAL on file dbs.
    for (mode, name) in [("FULL", "full"), ("INCREMENTAL", "incr"), ("NONE", "none")] {
        let mut p = std::env::temp_dir();
        p.push(format!("rsql_avp_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_file(&p);
        let conn = Connection::open(&p).unwrap();
        conn.execute_batch(&format!("PRAGMA auto_vacuum = {mode};"))
            .unwrap();
        conn.execute("CREATE TABLE t(a TEXT, b INT)", []).unwrap();
        conn.execute("INSERT INTO t VALUES ('x', 1)", []).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap();
        let ck: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        let av: i64 = conn
            .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
            .unwrap();
        drop(conn);
        let data = std::fs::read(&p).unwrap();
        let be32 = |o: usize| u32::from_be_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);
        println!(
            "5. {mode}: rows={n} integrity={ck} pragma={av} header52={} header64={} npages_hdr={} file_pages={}",
            be32(52),
            be32(64),
            be32(28),
            data.len() / 4096
        );
        let _ = std::fs::remove_file(&p);
    }

    // 7. simulate our dump shape: largest_root = n_pages, then CREATE TABLE.
    let mut p = std::env::temp_dir();
    p.push(format!("rsql_avp_shape_{}", std::process::id()));
    let _ = std::fs::remove_file(&p);
    {
        let conn = Connection::open(&p).unwrap();
        conn.execute_batch("PRAGMA auto_vacuum = FULL;").unwrap();
        conn.execute_batch("CREATE TABLE t(a); INSERT INTO t VALUES (1),(2),(3);")
            .unwrap();
    }
    let mut data = std::fs::read(&p).unwrap();
    let npages = data.len() / 4096;
    // Patch header 52 to n_pages (our planned "safe largest root") and
    // bump the change counter + version-valid-for so SQLite re-reads it.
    data[52..56].copy_from_slice(&(npages as u32).to_be_bytes());
    data[24..28].copy_from_slice(&999u32.to_be_bytes());
    data[92..96].copy_from_slice(&999u32.to_be_bytes());
    std::fs::write(&p, &data).unwrap();
    {
        let conn = Connection::open(&p).unwrap();
        let ck: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        println!("7. patched largest_root={npages}: integrity_check = {ck}");
        let r = conn.execute_batch("CREATE TABLE u(b); INSERT INTO u VALUES (42);");
        println!("   subsequent CREATE TABLE + INSERT -> {r:?}");
        let ck: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        let v: i64 = conn
            .query_row("SELECT COUNT(*) FROM u", [], |r| r.get(0))
            .unwrap();
        println!("   integrity after = {ck}, rows in u = {v}");
    }
    let _ = std::fs::remove_file(&p);
    println!("done");
}
