//! Focused RSS probe: which battery query materializes memory at scale?
//! (Temporary diagnostic — run with MEGA_ROWS to control scale.)
use rustqlite::{Database, Value};

fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn rss_mb() -> f64 {
    if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                if let Some(kb) = rest.split_whitespace().next() {
                    return kb.parse::<u64>().unwrap_or(0) as f64 / 1024.0;
                }
            }
        }
    }
    0.0
}

fn peak_mb() -> f64 {
    if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("VmHWM:") {
                if let Some(kb) = rest.split_whitespace().next() {
                    return kb.parse::<u64>().unwrap_or(0) as f64 / 1024.0;
                }
            }
        }
    }
    0.0
}

#[test]
fn rss_probe() {
    let rows: u64 = std::env::var("MEGA_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000_000);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("probe.db");
    let t0 = std::time::Instant::now();
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("PRAGMA journal_mode = WAL", []).unwrap();
        db.execute("PRAGMA synchronous = NORMAL", []).unwrap();
        db.execute(
            "CREATE TABLE events (id INTEGER PRIMARY KEY, k INTEGER, note TEXT)",
            [],
        )
        .unwrap();
        db.execute("CREATE INDEX ix_events_k ON events (k)", [])
            .unwrap();
        let batch = 5000u64;
        let mut next = 0u64;
        while next < rows {
            let n = batch.min(rows - next);
            let mut sql = String::with_capacity(48 * n as usize + 48);
            sql.push_str("INSERT INTO events (id, k, note) VALUES ");
            for j in 0..n {
                let i = next + j + 1;
                let h = splitmix64(i);
                let note = format!("n{:08x}", (h >> 20) & 0xffff_ffff);
                if j > 0 {
                    sql.push(',');
                }
                sql.push_str(&format!("({}, {}, '{}')", i, i % 1_000_000, note));
            }
            db.execute("BEGIN", []).unwrap();
            db.execute(&sql, []).unwrap();
            db.execute("COMMIT", []).unwrap();
            next += n;
        }
        println!(
            "[probe] build {rows} in {:.1}s rss={:.0}MB",
            t0.elapsed().as_secs_f64(),
            rss_mb()
        );
    }
    let queries: Vec<(&str, String)> = vec![
        ("count-bare", "SELECT count(*) FROM events".to_string()),
        (
            "count-filter",
            format!("SELECT count(*) FROM events WHERE id <= {rows}"),
        ),
        (
            "agg-4",
            format!("SELECT count(*), sum(k), min(k), max(k) FROM events WHERE id <= {rows}"),
        ),
        (
            "group97",
            format!(
                "SELECT k % 97, count(*) FROM events WHERE id <= {rows} GROUP BY k % 97 ORDER BY 1"
            ),
        ),
        (
            "top25",
            format!("SELECT k FROM events WHERE id <= {rows} ORDER BY k DESC LIMIT 25"),
        ),
        (
            "like",
            "SELECT count(*) FROM events WHERE note LIKE 'n0000%'".to_string(),
        ),
        (
            "probe",
            "SELECT k, note FROM events WHERE id = 12345".to_string(),
        ),
    ];
    {
        let db = Database::open(&path).unwrap();
        for (name, sql) in &queries {
            let before = rss_mb();
            let peak_before = peak_mb();
            let t = std::time::Instant::now();
            let got = db.query(sql, []).unwrap_or_default();
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            println!(
                "[probe] {name:14} {ms:8.0}ms rows={:5} rss {before:6.0} -> {:6.0}MB (peak {:.0}MB)",
                got.len(),
                rss_mb(),
                peak_mb().max(peak_before)
            );
        }
        println!("[probe] --- variant bisection ---");
        let variants: Vec<(&str, String)> = vec![
            (
                "sum-only",
                format!("SELECT sum(k) FROM events WHERE id <= {rows}"),
            ),
            (
                "sum+minmax",
                format!("SELECT sum(k), min(k), max(k) FROM events WHERE id <= {rows}"),
            ),
            (
                "count+sum",
                format!("SELECT count(*), sum(k) FROM events WHERE id <= {rows}"),
            ),
            (
                "agg4-nofilter",
                "SELECT count(*), sum(k), min(k), max(k) FROM events".to_string(),
            ),
            ("sum-nofilter", "SELECT sum(k) FROM events".to_string()),
            (
                "agg4-kfilter",
                format!("SELECT count(*), sum(k), min(k), max(k) FROM events WHERE k <= {rows}"),
            ),
            (
                "avg-only",
                format!("SELECT avg(k) FROM events WHERE id <= {rows}"),
            ),
        ];
        for (name, sql) in &variants {
            let peak_before = peak_mb();
            let t = std::time::Instant::now();
            let got = db.query(sql, []).unwrap_or_default();
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            let nrows = got.len();
            let pd = peak_mb() - peak_before;
            println!("[probe] {name:14} {ms:8.0}ms rows={nrows:5} peak-delta {pd:+7.0}MB");
        }
        for q in [
            format!("EXPLAIN QUERY PLAN SELECT count(*), sum(k), min(k), max(k) FROM events WHERE id <= {rows}"),
            format!("EXPLAIN QUERY PLAN SELECT sum(k) FROM events WHERE id <= {rows}"),
            format!("EXPLAIN QUERY PLAN SELECT k FROM events WHERE id <= {rows} ORDER BY k DESC LIMIT 25"),
            format!("EXPLAIN QUERY PLAN SELECT k % 97, count(*) FROM events WHERE id <= {rows} GROUP BY k % 97"),
        ] {
            match db.query(&q, []) {
                Ok(rows) => {
                    for r in &rows {
                        println!("[probe] EQP: {:?}", r.iter().map(|v| v.as_text()).collect::<Vec<_>>());
                    }
                }
                Err(e) => println!("[probe] EQP err: {e}"),
            }
        }
        drop(db);
    }
    // parallel_scan explicitly off
    let mut db = Database::open(&path).unwrap();
    let _ = db.execute("PRAGMA parallel_scan = off", []);
    println!("[probe] --- parallel_scan OFF ---");
    for (name, sql) in &queries {
        let before = rss_mb();
        let t = std::time::Instant::now();
        let got = db.query(sql, []).unwrap_or_default();
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        println!(
            "[probe] {name:14} {ms:8.0}ms rows={:5} rss {before:6.0} -> {:6.0}MB",
            got.len(),
            rss_mb()
        );
    }
    let _ = Value::Integer(0);
}
