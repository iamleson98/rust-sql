fn main() {
    let path = "/home/z/rust-sql/probe-tmp/probe-engine-engine-delete-auto.db";
    let conn = rusqlite::Connection::open(path).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    let (pages, rows, rev): (i64, i64, i64) = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM pages), (SELECT COUNT(*) FROM rows), (SELECT COALESCE(SUM(rev),0) FROM pages)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    let ic: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    println!(
        "real-sqlite sees: journal_mode={mode} pages={pages} rows={rows} rev_sum={rev} integrity_check={ic}"
    );
}
