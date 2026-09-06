//! Isolate mimalloc's retention behavior for the insert-storm churn shape:
//! 1M transient ~20B strings + 40k transient 4KB vecs (the Page churn).
fn main() {
    let mut keep = 0usize;
    for i in 0..1_000_000usize {
        let s = format!("payload-{:08}", i);
        keep += s.len();
        drop(s);
    }
    for _ in 0..40_000usize {
        let v = vec![0u8; 4096];
        if v[0] == 99 {
            keep += 1;
        }
        drop(v);
    }
    let s = std::fs::read_to_string("/proc/self/status").unwrap();
    let hwm = s.lines().find(|l| l.starts_with("VmHWM")).unwrap();
    let rss = s.lines().find(|l| l.starts_with("VmRSS")).unwrap();
    eprintln!("keep={keep} {hwm} {rss}");
}

fn _unused() {}
