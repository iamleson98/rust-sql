//! FORENSIC: walk every tree + the freelist of a SQLite-format file and
//! report ownership anomalies (double-owned pages, orphans, bad counts).
use rustqlite::Database;

fn main() {
    let path = std::path::PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "/tmp/rsql_repro_full.db".into()),
    );
    let db = Database::open(&path).unwrap();
    let schema = db
        .query("SELECT type, name, rootpage FROM sqlite_schema", [])
        .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let ps = 4096usize;
    let n_pages = bytes.len() / ps;
    println!("file: {} pages", n_pages);

    // ---- freelist walk ----
    let be32 = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
    let mut free = std::collections::BTreeSet::new();
    let mut head = be32(32);
    let declared_count = be32(36);
    let mut guard = 0;
    while head != 0 {
        guard += 1;
        if guard > 100000 {
            println!("freelist chain loop!");
            break;
        }
        let off = (head as usize - 1) * ps;
        let next = be32(off);
        let k = be32(off + 4) as usize;
        if 8 + k * 4 > ps {
            println!("trunk {} overfull (k={k})", head);
            break;
        }
        if !free.insert(head) {
            println!("freelist trunk {} twice!", head);
        }
        for i in 0..k {
            let leaf = be32(off + 8 + i * 4);
            if !free.insert(leaf) {
                println!("freelist leaf {} twice!", leaf);
            }
        }
        head = next;
    }
    println!("freelist: declared {declared_count}, walked {}", free.len());
    if declared_count as usize != free.len() {
        println!(
            "!! freelist COUNT MISMATCH ({} != {})",
            declared_count,
            free.len()
        );
    }

    // ---- tree walk ----
    let mut owned = std::collections::BTreeSet::new();
    let walk = |root: u32, name: &str, owned: &mut std::collections::BTreeSet<u32>| {
        let mut stack = vec![root];
        let mut seen = std::collections::BTreeSet::new();
        while let Some(p) = stack.pop() {
            if p == 0 || p as usize > n_pages {
                println!("{name}: page {p} OUT OF RANGE");
                continue;
            }
            if !seen.insert(p) {
                println!("{name}: page {p} visited twice (cycle?)");
                continue;
            }
            owned.insert(p);
            let off = (p as usize - 1) * ps;
            let ptype = bytes[off];
            let n = u16::from_be_bytes([bytes[off + 3], bytes[off + 4]]) as usize;
            let array: usize = if matches!(ptype, 0x05 | 0x02) { 12 } else { 8 };
            for i in 0..n {
                let cp = u16::from_be_bytes([
                    bytes[off + array + i * 2],
                    bytes[off + array + i * 2 + 1],
                ]) as usize;
                if matches!(ptype, 0x05 | 0x02) {
                    stack.push(u32::from_be_bytes(
                        bytes[off + cp..off + cp + 4].try_into().unwrap(),
                    ));
                }
            }
            if matches!(ptype, 0x05 | 0x02) {
                let rm = u32::from_be_bytes(bytes[off + 8..off + 12].try_into().unwrap());
                stack.push(rm);
            }
            // freeblock chain sanity
            let fb = u16::from_be_bytes([bytes[off + 1], bytes[off + 2]]);
            let mut f = fb as usize;
            let mut fguard = 0;
            while f != 0 {
                fguard += 1;
                if fguard > 1000 || off + f + 4 > bytes.len() {
                    println!("{name}: page {p} freeblock chain broken at {f}");
                    break;
                }
                let nxt = u16::from_be_bytes([bytes[off + f], bytes[off + f + 1]]);
                f = nxt as usize;
            }
        }
    };
    walk(1, "schema", &mut owned);
    for r in schema.iter() {
        let name = match r.get(1) {
            Some(rustqlite::Value::Text(t)) => t.to_string(),
            _ => "?".to_string(),
        };
        let root = match r.get(2) {
            Some(rustqlite::Value::Integer(i)) => *i,
            _ => 0,
        };
        if root > 0 {
            walk(root as u32, &name, &mut owned);
        }
    }
    println!("tree-owned pages: {}", owned.len());
    // Dump the schema + ix4's exact tree shape.
    {
        let rows = db
            .query("SELECT type, name, rootpage FROM sqlite_schema", [])
            .unwrap();
        for r in rows.iter() {
            println!("SCHEMA: {:?}", r);
        }
        let dump_tree = |root: u32, name: &str| {
            let mut stack = vec![(root, 0u32)];
            let mut seen = std::collections::BTreeSet::new();
            while let Some((p, _parent)) = stack.pop() {
                if !seen.insert(p) || p == 0 || p as usize > n_pages {
                    continue;
                }
                let off = (p as usize - 1) * ps;
                let ptype = bytes[off];
                let n = u16::from_be_bytes([bytes[off + 3], bytes[off + 4]]) as usize;
                let mut kids = Vec::new();
                if matches!(ptype, 0x05 | 0x02) {
                    let rm = u32::from_be_bytes(bytes[off + 8..off + 12].try_into().unwrap());
                    kids.push(rm);
                    for i in 0..n {
                        let cp = u16::from_be_bytes([
                            bytes[off + 12 + i * 2],
                            bytes[off + 12 + i * 2 + 1],
                        ]) as usize;
                        kids.push(u32::from_be_bytes(
                            bytes[off + cp..off + cp + 4].try_into().unwrap(),
                        ));
                    }
                }
                println!(
                    "  {name} page {p} type 0x{:02x} cells {n} kids {:?}",
                    ptype, kids
                );
                for k in kids {
                    stack.push((k, p));
                }
            }
        };
        let rows = db.query("SELECT name, rootpage FROM sqlite_schema WHERE name='ix4' OR name='ix2' OR name='ix1'", []).unwrap();
        for r in rows.iter() {
            let name = match r.first() {
                Some(rustqlite::Value::Text(t)) => t.to_string(),
                _ => continue,
            };
            let root = match r.get(1) {
                Some(rustqlite::Value::Integer(i)) => *i,
                _ => continue,
            };
            if root > 0 {
                dump_tree(root as u32, &name);
            }
        }
    }
    let overlap: Vec<_> = owned.intersection(&free).collect();
    if !overlap.is_empty() {
        println!("!! DOUBLE-OWNED (tree + freelist): {:?}", overlap);
        // Name the referencing tree + the parent page/cell pointing at
        // each double-owned page.
        let schema2 = db
            .query("SELECT type, name, rootpage FROM sqlite_schema", [])
            .unwrap();
        let probe = |root: u32, name: &str| {
            let mut stack = vec![(root, 0u32)];
            let mut seen = std::collections::BTreeSet::new();
            while let Some((p, _parent)) = stack.pop() {
                if !seen.insert(p) || p == 0 || p as usize > n_pages {
                    continue;
                }
                let off = (p as usize - 1) * ps;
                let ptype = bytes[off];
                if matches!(ptype, 0x05 | 0x02) {
                    let n = u16::from_be_bytes([bytes[off + 3], bytes[off + 4]]) as usize;
                    for i in 0..n {
                        let cp = u16::from_be_bytes([
                            bytes[off + 12 + i * 2],
                            bytes[off + 12 + i * 2 + 1],
                        ]) as usize;
                        let child =
                            u32::from_be_bytes(bytes[off + cp..off + cp + 4].try_into().unwrap());
                        if overlap.iter().any(|o| **o == child) {
                            println!(
                                "  {} -> parent page {p} cell {i} (cp {cp}) references {child}",
                                name
                            );
                        }
                        stack.push((child, p));
                    }
                    let rm = u32::from_be_bytes(bytes[off + 8..off + 12].try_into().unwrap());
                    if overlap.iter().any(|o| **o == rm) {
                        println!("  {} -> parent page {p} RIGHT-MOST references {rm}", name);
                    }
                    stack.push((rm, p));
                }
            }
        };
        probe(1, "schema");
        for r in schema2.iter() {
            let name = match r.get(1) {
                Some(rustqlite::Value::Text(t)) => t.to_string(),
                _ => "?".to_string(),
            };
            let root = match r.get(2) {
                Some(rustqlite::Value::Integer(i)) => *i,
                _ => 0,
            };
            if root > 0 {
                probe(root as u32, &name);
            }
        }
    }
    let all: std::collections::BTreeSet<u32> = (1..=n_pages as u32).collect();
    let orphan: Vec<_> = all
        .difference(&owned)
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        .difference(&free)
        .cloned()
        .collect::<Vec<_>>();
    // account pointer-map pages (auto-vacuum) as ok
    if !orphan.is_empty() {
        println!("ORPHAN pages (neither tree nor free): {:?}", orphan);
    }
    let page2 = &bytes[ps..ps + 100];
    let auto_vac = u32::from_be_bytes(page2[52..56].try_into().unwrap());
    println!("auto_vacuum header: {auto_vac}");
}
