//! The S2 fold-work probe: the EXACT limit_million_row_file shape with
//! per-batch commit timing and WAL instrumentation (RSQL_WAL_TIMING=1).
//! Usage: cargo run --release --example wal_fold_probe -- [rows] [batch]
use rustqlite::Database;
use std::io::Write;
use std::time::Instant;

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

fn gen_row(i: u64) -> (i64, String) {
    let h = splitmix64(i + 1);
    let k = (h % 1_000_000) as i64;
    let note = format!("n{:08x}", (h >> 20) & 0xffff_ffff);
    (k, note)
}

fn main() {
    let rows: u64 = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000);
    let batch: u64 = std::env::args()
        .nth(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(5_000);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.db");
    let mut db = Database::open(&path).unwrap();
    db.execute(
        "CREATE TABLE events (id INTEGER PRIMARY KEY, k INTEGER, note TEXT)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX ix_events_k ON events (k)", [])
        .unwrap();

    let mut commit_ms: Vec<f64> = Vec::new();
    let mut batch_ms: Vec<f64> = Vec::new();
    let build = Instant::now();
    let mut next_id: u64 = 0;
    while next_id < rows {
        let n = batch.min(rows - next_id);
        let mut sql = String::with_capacity(64 * n as usize + 64);
        sql.push_str("INSERT INTO events (id, k, note) VALUES ");
        for j in 0..n {
            let (k, note) = gen_row(next_id + j);
            if j > 0 {
                sql.push(',');
            }
            sql.push_str(&format!("({}, {}, '{}')", next_id + j + 1, k, note));
        }
        db.execute("BEGIN", []).unwrap();
        let t0 = Instant::now();
        db.execute(&sql, []).unwrap();
        batch_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        let t1 = Instant::now();
        db.execute("COMMIT", []).unwrap();
        commit_ms.push(t1.elapsed().as_secs_f64() * 1000.0);
        next_id += n;
    }
    let build_s = build.elapsed().as_secs_f64();

    let med = |v: &[f64]| {
        let mut s = v.to_vec();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        s[s.len() / 2]
    };
    let n10 = (commit_ms.len() / 10).max(1);
    println!(
        "[fold-probe] rows={rows} batch={batch} batches={} build={build_s:.2}s rows/s={:.0}",
        commit_ms.len(),
        rows as f64 / build_s.max(1e-9),
    );
    println!(
        "[fold-probe] batch-ms  first10%={:.2} med last10%={:.2} med (x{:.1})",
        med(&batch_ms[..n10]),
        med(&batch_ms[batch_ms.len() - n10..]),
        med(&batch_ms[batch_ms.len() - n10..]) / med(&batch_ms[..n10]).max(1e-9),
    );
    println!(
        "[fold-probe] commit-ms first10%={:.2} med last10%={:.2} med (x{:.1})",
        med(&commit_ms[..n10]),
        med(&commit_ms[commit_ms.len() - n10..]),
        med(&commit_ms[commit_ms.len() - n10..]) / med(&commit_ms[..n10]).max(1e-9),
    );
    // per-decile commit medians: the growth curve
    let dec = commit_ms.len() / 10;
    let mut out = String::new();
    for d in 0..10 {
        let lo = d * dec;
        let hi = if d == 9 {
            commit_ms.len()
        } else {
            (d + 1) * dec
        };
        if hi > lo {
            out.push_str(&format!(" d{}={:.1}", d + 1, med(&commit_ms[lo..hi])));
        }
    }
    println!("[fold-probe] commit-ms deciles:{out}");
    let _ = std::io::stderr().flush();
}
