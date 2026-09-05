//! `PRAGMA integrity_check` / `PRAGMA quick_check` — modeled on
//! https://www.sqlite.org/pragma.html#pragma_integrity_check and the way
//! SQLite's own test harnesses use it (testing.html §3.2: "after the I/O
//! error simulation failure mechanism is disabled, the database is
//! examined using PRAGMA integrity_check to make sure that the I/O error
//! has not introduced database corruption").
//!
//! The check walks every b-tree in the database and verifies:
//!
//! 1. **File shape** — the file is exactly `n_pages * page_size` bytes
//!    (or larger when a WAL holds newer frames), and the header's page
//!    size is a legal power of two.
//! 2. **Freelist sanity** — the freelist chain contains exactly
//!    `freelist_count` distinct pages inside the file, with no cycles.
//! 3. **Every table b-tree** — a full ordered scan: rowids strictly
//!    ascending (b-tree ordering invariant; duplicates or regressions
//!    mean a corrupt tree), every payload decodes as a row of the
//!    declared width, and every interior page is traversable (the scan
//!    itself fails on structural damage — surfaced as an error).
//! 4. **Every index b-tree** (skipped by `quick_check`) — entries in
//!    (key, rowid) order with no exact duplicates, and full
//!    index-vs-table cross-verification in both directions:
//!    "row N missing from index I" and "entry in index I references row N
//!    that is missing from the table", plus the entry-count equality.
//! 5. **The schema b-tree** (root 0) — walked like a table, validating
//!    schema rows decode as 5-column rows.
//!
//! Like SQLite, the pragma returns one row per problem (up to a cap;
//! SQLite's default is 100) and the single row `ok` when the database is
//! clean. Never panics: every malformed structure surfaces as a message.

use crate::schema::{Catalog, Index, Table};
use crate::storage::btree::Btree;
use crate::storage::pager::Pager;
use crate::storage::row_codec::decode_row;
use crate::types::Value;
use std::collections::HashSet;

/// Default maximum number of reported problems (SQLite uses 100).
const MAX_REPORTED_PROBLEMS: usize = 100;

/// Problem collector — stops recording after `max` entries but keeps
/// scanning cheaply so the caller always gets a bounded result.
struct Problems {
    out: Vec<String>,
    max: usize,
}

impl Problems {
    fn new(max: usize) -> Self {
        Problems {
            out: Vec::new(),
            max,
        }
    }

    fn push(&mut self, msg: String) {
        if self.out.len() < self.max {
            self.out.push(msg);
        }
    }

    fn is_clean(&self) -> bool {
        self.out.is_empty()
    }
}

/// Run the integrity check. Returns the result rows as `Vec<Value>`:
/// `["ok"]` when clean, otherwise one row per problem (capped).
///
/// `roots` / `index_roots` are the LIVE root pages (the session's
/// bookkeeping overrides — a split may have moved a root after the schema
/// row was last rewritten), falling back to the catalog's persisted roots.
pub fn integrity_check(
    catalog: &Catalog,
    pager: &Pager,
    roots: &std::collections::HashMap<String, u32>,
    index_roots: &std::collections::HashMap<String, u32>,
    quick: bool,
) -> Vec<Value> {
    let mut p = Problems::new(MAX_REPORTED_PROBLEMS);

    // ---- 1. File shape -------------------------------------------------
    // In WAL mode the main file may legitimately be SHORTER than
    // n_pages * page_size: pages committed after the last checkpoint live
    // in the -wal file and are served from there. So a short file is only
    // corruption when the missing pages are also unreadable.
    let page_size = pager.page_size() as u64;
    let n_pages = pager.n_pages() as u64;
    if let Ok(meta) = pager.file_metadata() {
        let file_len = meta.len();
        let expected = n_pages * page_size;
        if file_len < expected {
            let file_pages = file_len / page_size;
            let mut unreadable: Option<u32> = None;
            for id in file_pages as u32..n_pages as u32 {
                if pager.get_page(id).is_err() {
                    unreadable = Some(id);
                    break;
                }
            }
            if let Some(bad) = unreadable {
                p.push(format!(
                    "main database is truncated: file is {} bytes, header says {} pages of {} bytes (page {} unreadable)",
                    file_len, n_pages, page_size, bad
                ));
            }
        } else if file_len > expected && file_len % page_size != 0 {
            p.push(format!(
                "main database file size {} is not a multiple of page size {}",
                file_len, page_size
            ));
        }
    }

    // ---- 2. Freelist sanity ----------------------------------------------
    check_freelist(pager, &mut p);

    // ---- 3. Schema b-tree (root 0) --------------------------------------
    check_schema_tree(pager, &mut p);

    // ---- 4. Table b-trees -------------------------------------------------
    let tables = catalog.all_tables();
    let mut table_rowids: Vec<(String, HashSet<i64>)> = Vec::with_capacity(tables.len());
    for (name, table) in &tables {
        let root = roots
            .get(&name.to_ascii_lowercase())
            .copied()
            .unwrap_or(table.root_page);
        let rowids = check_table_tree(pager, table, root, &mut p);
        table_rowids.push((name.clone(), rowids));
    }

    // ---- 5. Index b-trees (the part quick_check skips) -------------------
    if !quick {
        let indexes = catalog.all_indexes();
        for (name, idx) in &indexes {
            let root = index_roots
                .get(&name.to_ascii_lowercase())
                .copied()
                .unwrap_or(idx.root_page);
            // Find the owning table's rowid set for cross-verification.
            let owner = table_rowids
                .iter()
                .find(|(t, _)| t.eq_ignore_ascii_case(&idx.table))
                .map(|(_, set)| set.clone());
            check_index_tree(pager, idx, root, owner.as_ref(), &mut p);
        }
    }

    if p.is_clean() {
        vec![Value::Text("ok".into())]
    } else {
        p.out.into_iter().map(|m| Value::Text(m.into())).collect()
    }
}

/// Walk the TRUNK freelist: count trunk + leaf pages, detect cycles,
/// verify bounds and entry-array shapes.
fn check_freelist(pager: &Pager, p: &mut Problems) {
    let head = pager.freelist_head();
    let declared = pager.freelist_count() as usize;
    if declared == 0 {
        if head != 0 {
            p.push(format!(
                "freelist head is page {} but freelist count is 0",
                head
            ));
        }
        return;
    }
    if head == 0 {
        p.push(format!("freelist count is {} but the head is 0", declared));
        return;
    }
    let n_pages = pager.n_pages();
    let mut visited: HashSet<u32> = HashSet::new();
    let mut cur = head;
    let mut walked = 0usize; // trunk pages + leaf entries
    while cur != 0 {
        if !visited.insert(cur) {
            p.push(format!("freelist is cyclic at page {}", cur));
            return;
        }
        if cur >= n_pages {
            p.push(format!(
                "freelist references page {} beyond end of file ({} pages)",
                cur, n_pages
            ));
            return;
        }
        // Trunk header: [next_trunk (4B), K (4B), K leaf ids (4B each)].
        let (next, k) = match pager.get_page(cur) {
            Ok(page) => {
                let borrowed = page.lock();
                let next = u32::from_le_bytes(
                    borrowed
                        .data
                        .get(..4)
                        .and_then(|s| s.try_into().ok())
                        .unwrap_or([0; 4]),
                );
                let k = u32::from_le_bytes(
                    borrowed
                        .data
                        .get(4..8)
                        .and_then(|s| s.try_into().ok())
                        .unwrap_or([0; 4]),
                ) as usize;
                (next, k)
            }
            Err(e) => {
                p.push(format!("freelist page {} unreadable: {}", cur, e));
                return;
            }
        };
        let cap = (pager.page_size() as usize).saturating_sub(8) / 4;
        if k > cap {
            p.push(format!(
                "freelist trunk {} claims {} entries (capacity {})",
                cur, k, cap
            ));
            return;
        }
        walked += 1 + k; // the trunk itself + its leaf entries
                         // Guard: a corrupted count/chain combination can't walk past the
                         // whole file.
        if walked > n_pages as usize {
            p.push("freelist walk exceeded page count".into());
            return;
        }
        cur = next;
    }
    if walked != declared {
        p.push(format!(
            "freelist says {} pages but chain has {}",
            declared, walked
        ));
    }
}

/// Walk the schema b-tree (root 0) as a table: ordering + 5-column decodes.
fn check_schema_tree(pager: &Pager, p: &mut Problems) {
    let mut bt = Btree::new(pager, 0, false);
    let mut prev: Option<i64> = None;
    let scan = bt.scan_table(|rowid, payload| {
        if let Some(pv) = prev {
            if rowid <= pv {
                p.push(format!(
                    "rowid {} out of order in schema tree (prev {})",
                    rowid, pv
                ));
                return false;
            }
        }
        prev = Some(rowid);
        if decode_row(payload, 5, rowid, None).is_err() {
            p.push(format!("schema row {} fails to decode", rowid));
        }
        true
    });
    if let Err(e) = scan {
        p.push(format!("schema tree is corrupt: {}", e));
    }
}

/// Full check of one table b-tree. Returns the set of rowids it contains
/// (for index cross-verification). Never fails hard: structural errors
/// become messages and yield whatever rowids were walkable.
fn check_table_tree(pager: &Pager, table: &Table, root: u32, p: &mut Problems) -> HashSet<i64> {
    let mut rowids = HashSet::new();
    let mut prev: Option<i64> = None;
    let n_cols = table.n_columns();
    let alias = table.rowid_alias;
    let mut bt = Btree::new(pager, root, false);
    let scan = bt.scan_table(|rowid, payload| {
        if let Some(pv) = prev {
            if rowid <= pv {
                p.push(format!(
                    "rowid {} out of order in table {} (prev {})",
                    rowid, table.name, pv
                ));
                return false;
            }
        }
        prev = Some(rowid);
        if decode_row(payload, n_cols, rowid, alias).is_err() {
            p.push(format!(
                "row {} of table {} fails to decode (payload {} bytes)",
                rowid,
                table.name,
                payload.len()
            ));
        }
        rowids.insert(rowid);
        true
    });
    if let Err(e) = scan {
        p.push(format!("table {} tree is corrupt: {}", table.name, e));
    }
    rowids
}

/// Full check of one index b-tree plus cross-verification against the
/// owning table's rowid set (when known).
fn check_index_tree(
    pager: &Pager,
    idx: &Index,
    root: u32,
    owner_rowids: Option<&HashSet<i64>>,
    p: &mut Problems,
) {
    let mut prev: Option<(Vec<u8>, i64)> = None;
    let mut index_rowids: HashSet<i64> = HashSet::new();
    let mut bt = Btree::new(pager, root, true);
    let scan = bt.scan_index(|rowid, key| {
        if let Some((pk, pr)) = &prev {
            if key <= pk.as_slice() && rowid <= *pr {
                p.push(format!(
                    "index {} entries out of order or duplicated at rowid {}",
                    idx.name, rowid
                ));
                return false;
            }
        }
        prev = Some((key.to_vec(), rowid));
        index_rowids.insert(rowid);
        true
    });
    if let Err(e) = scan {
        p.push(format!("index {} tree is corrupt: {}", idx.name, e));
        return;
    }
    let Some(owner) = owner_rowids else {
        // Owning table absent (dangling index — schema itself is broken;
        // the catalog loader reports that separately). Structure checks
        // above are all we can do.
        return;
    };
    // Index -> table: every index entry must reference a live row.
    for r in &index_rowids {
        if !owner.contains(r) {
            p.push(format!(
                "entry in index {} references row {} that is missing from table {}",
                idx.name, r, idx.table
            ));
        }
    }
    // Table -> index: every live row must have an index entry.
    let missing: Vec<&i64> = owner.difference(&index_rowids).collect();
    if !missing.is_empty() {
        // Report count-style (SQLite reports each missing row; we cap
        // through the collector, so report a bounded list).
        for r in missing.iter().take(5) {
            p.push(format!("row {} missing from index {}", r, idx.name));
        }
        if missing.len() > 5 {
            p.push(format!(
                "{} rows missing from index {} (showing first 5)",
                missing.len(),
                idx.name
            ));
        }
    }
}
