// RSS cost per chunk size in mimalloc: how much does a 64KB/16KB/8KB
// allocation actually cost in resident pages?
fn cur_mb() -> f64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap();
    for l in s.lines() {
        if if let Some(rest) = l.strip_prefix("VmRSS:") {
            let kb: f64 = rest.trim().trim_end_matches(" kB").parse().unwrap();
            return kb / 1024.0;
        } else {
            false
        } {
            unreachable!()
        }
    }
    0.0
}

fn main() {
    for sz in [65536usize, 16384, 8192, 4096] {
        let n = 40; // 40 chunks ~ the 10k-group workload's chunk count
        let base = cur_mb();
        let mut v: Vec<Box<[u8]>> = Vec::with_capacity(n);
        for _ in 0..n {
            v.push(vec![0u8; sz].into());
        }
        let grown = cur_mb() - base;
        println!(
            "{sz:>6}B x {n:>3} = {:>7}B data -> RSS +{grown:.2}MB ({:.0}B/block amplification)",
            sz * n,
            grown * 1024.0 * 1024.0 / n as f64
        );
        drop(v);
        println!("         dropped: RSS {:+.2}MB vs base", cur_mb() - base);
    }
}
