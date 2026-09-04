// RSS after engine init + build, mimalloc vs System (via --no-default-features).
fn cur_mb() -> f64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap();
    for l in s.lines() {
        if let Some(rest) = l.strip_prefix("VmRSS:") {
            let kb: f64 = rest.trim().trim_end_matches(" kB").parse().unwrap();
            return kb / 1024.0;
        }
    }
    0.0
}

fn main() {
    println!("after load   : {:.2}MB", cur_mb());
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    println!("after open   : {:.2}MB", cur_mb());
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", [])
        .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=150_000i64 {
        db.execute(
            "INSERT INTO t (val) VALUES (?)",
            [rustqlite::Value::Integer(i)],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    println!("after build  : {:.2}MB", cur_mb());
    let out = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    println!("after count  : {:.2}MB ({:?})", cur_mb(), out[0][0]);
}
