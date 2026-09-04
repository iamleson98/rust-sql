// Counting allocator probe: WHERE does the GROUP BY query allocate?
// Runs with --no-default-features so mimalloc is out of the picture;
// the counting allocator records size-class histograms per phase.
// (The `global_allocator` here conflicts with the crate's mimalloc, so
// this example is for local diagnosis only — run it explicitly.)
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

#[global_allocator]
static COUNTING: Counting = Counting;

struct Counting;

/// Fixed 64-bucket histogram (size class = next_pow2, bucket index =
/// trailing zeros) — NO allocation inside the allocator hooks.
struct Hist {
    counts: [AtomicU64; 64],
    live: AtomicUsize,
    peak: AtomicUsize,
}

macro_rules! zero_counts {
    () => {{
        let a: [AtomicU64; 64] = [
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
        ];
        a
    }};
}

static HIST: Hist = Hist {
    counts: zero_counts!(),
    live: AtomicUsize::new(0),
    peak: AtomicUsize::new(0),
};

#[inline]
fn bucket_of(sz: usize) -> usize {
    let p = sz.max(8).next_power_of_two();
    p.trailing_zeros() as usize
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let sz = layout.size();
        let l = HIST.live.fetch_add(sz, Ordering::Relaxed) + sz;
        let mut p = HIST.peak.load(Ordering::Relaxed);
        while l > p {
            match HIST
                .peak
                .compare_exchange_weak(p, l, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(x) => p = x,
            }
        }
        HIST.counts[bucket_of(sz)].fetch_add(1, Ordering::Relaxed);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        HIST.live.fetch_sub(layout.size(), Ordering::Relaxed);
        System.dealloc(ptr, layout)
    }
}

fn snapshot() -> [u64; 64] {
    let mut out = [0u64; 64];
    for (i, c) in HIST.counts.iter().enumerate() {
        out[i] = c.load(Ordering::Relaxed);
    }
    out
}

fn bucket_size(i: usize) -> u64 {
    (1u64 << i).max(8)
}

fn diff(a: &[u64; 64], b: &[u64; 64]) -> Vec<(usize, u64)> {
    (0..64)
        .filter(|&i| b[i] > a[i])
        .map(|i| (bucket_size(i) as usize, b[i] - a[i]))
        .collect()
}

fn show(label: &str, d: &[(usize, u64)]) {
    let total_count: u64 = d.iter().map(|&(_, n)| n).sum();
    let total_bytes: u64 = d.iter().map(|&(sz, n)| (sz as u64) * n).sum();
    println!(
        "{label:24} allocs {total_count:9}  ~{total_bytes:10}B  live {} peak {}",
        HIST.live.load(Ordering::Relaxed),
        HIST.peak.load(Ordering::Relaxed)
    );
    let mut d = d.to_vec();
    d.sort_by(|a, b| (b.0 as u64 * b.1).cmp(&(a.0 as u64 * a.1)));
    for &(sz, n) in d.iter().take(6) {
        println!("      {:>7}B x {:>9} = {:>10}B", sz, n, (sz as u64) * n);
    }
}

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
    let rows: i64 = std::env::var("PROBE_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30_000);
    println!("start: {:.1}MB", cur_mb());
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", [])
        .unwrap();
    db.execute("BEGIN", []).unwrap();
    {
        let mut stmt = db.prepare("INSERT INTO t (id, val) VALUES (?, ?)").unwrap();
        for i in 1..=rows {
            stmt.bind(1, rustqlite::Value::Integer(i)).unwrap();
            stmt.bind(2, rustqlite::Value::Integer(i as i64 * 7 % 100_000))
                .unwrap();
            let _ = stmt.step();
            stmt.reset();
        }
    }
    db.execute("COMMIT", []).unwrap();

    println!("after build: {:.1}MB", cur_mb());
    let s0 = snapshot();
    let sql = "SELECT val/10, COUNT(*) FROM t GROUP BY val/10";
    let out = db.query(sql, []).unwrap();
    let s1 = snapshot();
    show("GROUP BY iter 0", &diff(&s0, &s1));
    println!("      -> {} groups | rss {:.1}MB", out.len(), cur_mb());
    drop(out);
    let out = db.query(sql, []).unwrap();
    let s2 = snapshot();
    show("GROUP BY iter 1", &diff(&s1, &s2));
    drop(out);
}
