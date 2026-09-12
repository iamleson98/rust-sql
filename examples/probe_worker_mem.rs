//! Worker-thread RSS cost micro-benchmark: how much does each scoped
//! worker thread cost in resident memory (stack touch + mimalloc theap)?

fn rss() -> f64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<f64>()
                .unwrap()
                / 1024.0;
        }
    }
    0.0
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "alloc".into());
    let n: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);

    let base = rss();
    println!("base rss={base:.2}MB mode={mode} n={n}");

    // Each worker mimics a scan worker's allocation profile: a handful of
    // small buffers across several size classes + a 4KB page buffer.
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..n {
            handles.push(scope.spawn(|| {
                let mut bufs: Vec<Vec<u8>> = Vec::new();
                for i in 0..16 {
                    bufs.push(vec![0u8; 8 * (i % 8 + 1)]); // 8..64B classes
                }
                let mut vals: Vec<Vec<i64>> = Vec::new();
                for i in 0..8 {
                    vals.push(vec![0i64; i + 1]);
                }
                let page = vec![0u8; 4096];
                let fwalk: Vec<u64> = vec![0; 32];
                let sum: f64 = page.iter().map(|&b| b as f64).sum::<f64>()
                    + fwalk.iter().map(|&v| v as f64).sum::<f64>()
                    + bufs.len() as f64
                    + vals.len() as f64;
                if sum < 0.0 {
                    println!("impossible {sum}");
                }
                0u64
            }));
        }
        for h in handles {
            let _ = h.join();
        }
    });
    let after = rss();
    println!(
        "after {n} workers + join: rss={after:.2}MB delta={:+.2}MB ({:.0}KB/worker)",
        after - base,
        (after - base) * 1024.0 / n as f64
    );
}
