// Scratch probe (untracked): where does the S09 blob-insert time go?
// In-memory DB, single txn, 1000 rows x 64KB BLOB (the S09 shape), plus
// control shapes that isolate the memcpy share.
use rustqlite::{Database, Value};
use std::time::Instant;

fn run(label: &str, blob: Vec<u8>, rows: i64, cols: usize) {
    let mut db = Database::open_in_memory().unwrap();
    let cols_sql: Vec<String> = (0..cols).map(|i| format!("c{i} BLOB")).collect();
    db.execute(
        &format!(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, {})",
            cols_sql.join(", ")
        ),
        [],
    )
    .unwrap();
    let one = blob.len() / cols;
    let vals: Vec<Vec<u8>> = (0..cols)
        .map(|i| blob[i * one..(i + 1) * one].to_vec())
        .collect();
    let placeholders: Vec<String> = (0..cols).map(|i| format!("?{}", i + 2)).collect();
    let sql = format!(
        "INSERT INTO t (id, {}) VALUES (?1, {})",
        (0..cols)
            .map(|i| format!("c{i}"))
            .collect::<Vec<_>>()
            .join(", "),
        placeholders.join(", ")
    );
    let mut engine_ms = 0.0f64;
    let t = Instant::now();
    {
        let t0 = Instant::now();
        db.execute("BEGIN", []).unwrap();
        engine_ms += t0.elapsed().as_secs_f64() * 1000.0;
        for i in 1..=rows {
            let mut p: Vec<Value> = Vec::with_capacity(cols + 1);
            p.push(Value::Integer(i));
            for v in &vals {
                p.push(Value::Blob(v.clone()));
            }
            let t0 = Instant::now();
            db.execute(&sql, p).unwrap();
            engine_ms += t0.elapsed().as_secs_f64() * 1000.0;
        }
        let t0 = Instant::now();
        db.execute("COMMIT", []).unwrap();
        engine_ms += t0.elapsed().as_secs_f64() * 1000.0;
    }
    let ms = engine_ms;
    let wall = t.elapsed().as_secs_f64() * 1000.0;
    let total_mb = (rows as f64 * blob.len() as f64) / 1024.0 / 1024.0;
    println!(
        "{label:44} {ms:8.2} ms engine (wall {wall:6.2})  ({total_mb:.0} MB -> {:.2} GB/s engine)",
        total_mb / ms / 1000.0 * 1000.0
    );
}

fn main() {
    // Baseline: raw memcpy throughput for 128 MB.
    {
        let src = vec![0xABu8; 65_536];
        let mut dst = vec![0u8; 65_536];
        let t = Instant::now();
        let mut n = 0u64;
        while t.elapsed().as_secs_f64() < 0.25 {
            dst.copy_from_slice(&src);
            n += 65_536;
        }
        let gbps = n as f64 / t.elapsed().as_secs_f64() / 1024.0 / 1024.0 / 1024.0;
        println!("{:<44} {gbps:8.2} GB/s (memcpy floor)", "");
        let _ = dst;
    }
    run("1000 x 64KB blob (S09 shape)", vec![b'b'; 65_536], 1000, 1);
    run("1000 x 64KB as 4 x 16KB cols", vec![b'b'; 65_536], 1000, 4);
    run("4000 x 16KB blob (same MB)", vec![b'b'; 16_384], 4000, 1);
    run(
        "1000 x 8KB blob (fits ~2 pages)",
        vec![b'b'; 8_192],
        1000,
        1,
    );
    run("1000 x 512B blob (inline)", vec![b'b'; 512], 1000, 1);
}
