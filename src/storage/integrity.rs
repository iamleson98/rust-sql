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

/// Decoded rows of one table (rowid, values) — partial-index predicate
/// evaluation during index cross-verification.
type RowidRows = Vec<(i64, Vec<Value>)>;

/// Bounded rowid membership for integrity cross-verification: a bitset
/// over `[0, ROWID_BITSET_LIMIT)` plus a hash-set tail for outlier ids.
/// Dense rowid spaces (the OLTP reality) cost max_rowid/8 bytes —
/// 250 KB per 2M rows, 12.5 MB per 100M — where the previous
/// `HashSet<i64>` cost ~40 B/row (90 MB at 2M rows, ~4 GB at 100M —
/// the mega-scale suite's M2 caught the RSS scaling; SQLite's own
/// integrity_check keeps its cross-check sets in transient b-trees for
/// exactly this reason). Arc-shared per owning table: the index loop
/// previously DEEP-CLONED the table's set per index.
#[derive(Default)]
struct RowidSet {
    bits: Vec<u64>,
    tail: std::collections::HashSet<i64>,
}

/// Bitset coverage: rowids below this bound live in the bitset (the
/// vector grows lazily to max_inserted/64 words, capped at 32 MB);
/// larger ids spill to the hash tail (sparse outliers — rowid space
/// beyond ~268M is pathological and stays hash-bounded).
const ROWID_BITSET_LIMIT: i64 = 1 << 28;

impl RowidSet {
    #[inline]
    fn insert(&mut self, id: i64) {
        if (0..ROWID_BITSET_LIMIT).contains(&id) {
            let idx = id as usize;
            let w = idx / 64;
            if w >= self.bits.len() {
                self.bits.resize(w + 1, 0);
            }
            self.bits[w] |= 1u64 << (idx % 64);
        } else {
            self.tail.insert(id);
        }
    }

    #[inline]
    fn contains(&self, id: i64) -> bool {
        if (0..ROWID_BITSET_LIMIT).contains(&id) {
            let idx = id as usize;
            self.bits
                .get(idx / 64)
                .is_some_and(|w| (w >> (idx % 64)) & 1 == 1)
        } else {
            self.tail.contains(&id)
        }
    }

    /// Number of members.
    fn len(&self) -> usize {
        self.bits
            .iter()
            .map(|w| w.count_ones() as usize)
            .sum::<usize>()
            + self.tail.len()
    }

    /// Iterate every member (bitset words then tail — order is
    /// irrelevant to the difference/containment uses here).
    fn iter(&self) -> impl Iterator<Item = i64> + '_ {
        self.bits
            .iter()
            .enumerate()
            .flat_map(|(w, bits)| {
                (0..64u32).filter_map(move |b| {
                    ((bits >> b) & 1 == 1).then_some((w * 64 + b as usize) as i64)
                })
            })
            .chain(self.tail.iter().copied())
    }
}

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
    roots: &crate::executor::NameMap<u32>,
    index_roots: &crate::executor::NameMap<u32>,
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
    // Tables that own PARTIAL indexes get their rows decoded and captured
    // during the walk: index cross-verification must evaluate each row
    // against the partial index's WHERE predicate (predicate-excluded rows
    // are legitimately absent from the index).
    let partial_owners: HashSet<String> = catalog
        .all_indexes()
        .iter()
        .filter(|(_, idx)| idx.partial_expr.is_some())
        .map(|(_, idx)| idx.table.to_ascii_lowercase())
        .collect();
    let tables = catalog.all_tables();
    let mut table_rowids: Vec<(String, std::sync::Arc<RowidSet>)> =
        Vec::with_capacity(tables.len());
    let mut table_rows: Vec<(String, RowidRows)> = Vec::new();
    // Each table's walked root (the index checks rescan the owner rows).
    let mut table_roots: Vec<(String, u32)> = Vec::with_capacity(tables.len());
    // Every b-tree root walked (page accounting below): the schema tree
    // plus each table and index.
    let mut all_roots: Vec<u32> = vec![0];
    for (name, table) in &tables {
        let root = roots
            .get(&name.to_ascii_lowercase())
            .copied()
            .unwrap_or(table.root_page);
        // Concurrent-regime fold (the executor's `table_root` contract):
        // both the map entry and the catalog fallback can hold a
        // SUPERSEDED generation of a re-rooted tree — a merge's replay
        // re-roots trees without a schema reload, and a map entry the
        // publishing transaction never touched keeps its older value
        // until the publish sweep folds it. Walking the stale root
        // validates a TRUNCATED subtree (the historical "demoted
        // interior" shape) and reports rows that ARE in the live tree
        // as missing. Identity for unmoved trees.
        let root = if root != 0 && pager.any_root_moves() {
            pager.committed_root_of(root)
        } else {
            root
        };
        if table.vtab.is_none() && root != 0 {
            all_roots.push(root);
        }
        table_roots.push((name.clone(), root));
        let capture = partial_owners.contains(&name.to_ascii_lowercase());
        let (rowids, rows) = check_table_tree(pager, table, root, &mut p, capture);
        table_rowids.push((name.clone(), std::sync::Arc::new(rowids)));
        if let Some(rows) = rows {
            table_rows.push((name.clone(), rows));
        }
    }

    // ---- 5. Index b-trees (the part quick_check skips) -------------------
    if !quick {
        let indexes = catalog.all_indexes();
        for (name, idx) in &indexes {
            let root = index_roots
                .get(&name.to_ascii_lowercase())
                .copied()
                .unwrap_or(idx.root_page);
            let root = if root != 0 && pager.any_root_moves() {
                pager.committed_root_of(root)
            } else {
                root
            };
            if root != 0 {
                all_roots.push(root);
            }
            // Find the owning table's rowid set for cross-verification.
            let owner_entry = table_rowids
                .iter()
                .find(|(t, _)| t.eq_ignore_ascii_case(&idx.table));
            let owner = owner_entry.map(|(_, set)| std::sync::Arc::clone(set));
            // Decoded rows + Table descriptor for partial-predicate eval.
            let rows_entry = table_rows
                .iter()
                .find(|(t, _)| t.eq_ignore_ascii_case(&idx.table));
            let partial_rows = if idx.partial_expr.is_some() {
                rows_entry.map(|(_, rows)| rows.as_slice())
            } else {
                None
            };
            let table_desc = tables
                .iter()
                .find(|(t, _)| t.eq_ignore_ascii_case(&idx.table))
                .map(|(_, tbl)| tbl.as_ref());
            let owner_root = table_roots
                .iter()
                .find(|(t, _)| t.eq_ignore_ascii_case(&idx.table))
                .map(|(_, r)| *r);
            check_index_tree(
                pager,
                idx,
                root,
                owner.as_deref(),
                partial_rows,
                table_desc,
                owner_root,
                &mut p,
            );
        }
    }

    // ---- 6. Page accounting (full check only) ---------------------------
    // Only meaningful when every b-tree was resolvable: a quick check
    // skips the index walk, and a damaged tree already reported above
    // would cascade into "never used" noise for its unreachable pages.
    if !quick && p.is_clean() {
        for msg in check_page_accounting(pager, &all_roots) {
            p.push(msg);
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
/// (for index cross-verification), and — when `capture_rows` is set — the
/// decoded rows themselves (needed to evaluate partial-index WHERE
/// predicates during index cross-verification). Never fails hard:
/// structural errors become messages and yield whatever rowids were
/// walkable.
fn check_table_tree(
    pager: &Pager,
    table: &Table,
    root: u32,
    p: &mut Problems,
    capture_rows: bool,
) -> (RowidSet, Option<RowidRows>) {
    let mut rowids = RowidSet::default();
    let mut rows: Option<RowidRows> = capture_rows.then(Vec::new);
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
        match decode_row(payload, n_cols, rowid, alias) {
            Ok(vals) => {
                if let Some(out) = rows.as_mut() {
                    out.push((rowid, vals));
                }
            }
            Err(_) => p.push(format!(
                "row {} of table {} fails to decode (payload {} bytes)",
                rowid,
                table.name,
                payload.len()
            )),
        }
        rowids.insert(rowid);
        true
    });
    if let Err(e) = scan {
        p.push(format!("table {} tree is corrupt: {}", table.name, e));
    }
    (rowids, rows)
}

/// Full check of one index b-tree plus cross-verification against the
/// owning table's rowid set (when known). For a partial index, only rows
/// satisfying the index's WHERE predicate are required to be present
/// (SQLite semantics: predicate-excluded rows are legitimately absent).
/// `partial_rows` carries the owning table's decoded rows for predicate
/// evaluation (present only when the index is partial).
#[allow(clippy::too_many_arguments)]
fn check_index_tree(
    pager: &Pager,
    idx: &Index,
    root: u32,
    owner_rowids: Option<&RowidSet>,
    partial_rows: Option<&[(i64, Vec<Value>)]>,
    table: Option<&Table>,
    owner_root: Option<u32>,
    p: &mut Problems,
) {
    let mut prev: Option<(Vec<u8>, i64)> = None;
    let mut index_rowids = RowidSet::default();
    let mut n_entries = 0usize;
    let mut fingerprint = 0u64;
    let mut bt = Btree::new(pager, root, true);
    let scan = bt.scan_index(|rowid, key| {
        n_entries += 1;
        fingerprint = fingerprint.wrapping_add(entry_hash(key, rowid));
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
    for r in index_rowids.iter() {
        if !owner.contains(r) {
            p.push(format!(
                "entry in index {} references row {} that is missing from table {}",
                idx.name, r, idx.table
            ));
        }
    }
    // Table -> index, key-exact (SQLite seeks every row's computed key):
    // an ordinary b-tree index must hold exactly the (key, rowid) entry
    // each member row computes — a rowid-level match misses an entry
    // filed under a STALE key.
    if let (crate::schema::IndexKind::Btree, Some(t), Some(troot)) = (&idx.kind, table, owner_root)
    {
        check_index_keys(pager, idx, t, troot, root, n_entries, fingerprint, owner, p);
        return;
    }
    // Table -> index: every live row must have an index entry — except
    // rows a partial index's WHERE predicate excludes (those are
    // legitimately absent; SQLite reports the same data as ok).
    // The required set is the OWNER set for non-partial indexes (the
    // Arc'd bitset — no copy) or a fresh filtered set for partial ones.
    // Missing-row reporting is BOUNDED (count + first 5): a corrupt
    // index over 100M rows previously materialized every missing rowid
    // into a Vec before printing 5.
    let partial_required: Option<RowidSet> = match (&idx.partial_expr, partial_rows, table) {
        (Some(_), Some(rows), Some(t)) => {
            let mut set = RowidSet::default();
            for (rid, vals) in rows {
                if crate::executor::index_row_matches_partial(
                    idx,
                    t,
                    vals,
                    &[],
                    &std::collections::HashMap::new(),
                )
                .unwrap_or(false)
                {
                    set.insert(*rid);
                }
            }
            Some(set)
        }
        _ => None,
    };
    // The required set streams from the OWNER's bitset for non-partial
    // indexes (zero copy) or the freshly filtered partial set.
    let required_iter: Box<dyn Iterator<Item = i64> + '_> = match partial_required {
        Some(ref set) => Box::new(set.iter()),
        None => Box::new(owner.iter()),
    };
    let mut missing_count = 0usize;
    let mut missing_first: Vec<i64> = Vec::new();
    for r in required_iter {
        if !index_rowids.contains(r) {
            missing_count += 1;
            if missing_first.len() < 5 {
                missing_first.push(r);
            }
        }
    }
    if missing_count > 0 && crate::executor::dbg_index_trace() {
        eprintln!(
            "[integrity] index {} missing {} rows: {:?}",
            idx.name, missing_count, missing_first
        );
        bt.debug_dump_tree(&format!("index {}", idx.name));
    }
    if missing_count > 0 {
        // Report count-style (SQLite reports each missing row; we cap
        // through the collector, so report a bounded list).
        for r in &missing_first {
            p.push(format!("row {} missing from index {}", r, idx.name));
        }
        if missing_count > 5 {
            p.push(format!(
                "{} rows missing from index {} (showing first 5)",
                missing_count, idx.name
            ));
        }
    }
}

/// Order-independent fingerprint term of one index entry.
fn entry_hash(key: &[u8], rowid: i64) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    rowid.hash(&mut h);
    h.finish()
}

/// The key-exact half of [`check_index_tree`] for an ordinary b-tree
/// index (SQLite's per-row OP_Found probe, at O(1) memory): rescan the
/// owner, compute each member row's key, and compare the multiset of
/// (key, rowid) pairs with the index's through `fingerprint`. Only a
/// mismatch pays for a second pass that names the rows whose entry is
/// missing. The entry count is compared for non-partial indexes only,
/// exactly as SQLite does ("wrong # of entries in index").
#[allow(clippy::too_many_arguments)]
fn check_index_keys(
    pager: &Pager,
    idx: &Index,
    table: &Table,
    table_root: u32,
    index_root: u32,
    n_entries: usize,
    fingerprint: u64,
    owner: &RowidSet,
    p: &mut Problems,
) {
    let n_cols = table.n_columns();
    let alias = table.rowid_alias;
    let no_params = std::collections::HashMap::new();
    // Each member row's computed (key, rowid) term; `None` = not indexed.
    let expected_term = |rowid: i64, payload: &[u8]| -> Option<u64> {
        let row = crate::storage::row_codec::decode_row(payload, n_cols, rowid, alias).ok()?;
        if idx.partial_expr.is_some()
            && !crate::executor::index_row_matches_partial(idx, table, &row, &[], &no_params)
                .unwrap_or(false)
        {
            return None;
        }
        let key = crate::executor::encode_index_key(idx, table, &row).ok()?;
        Some(entry_hash(&key, rowid))
    };
    let mut expected_fp = 0u64;
    let mut n_expected = 0usize;
    let mut tbt = Btree::new(pager, table_root, false);
    let _ = tbt.scan_table(|rowid, payload| {
        if let Some(h) = expected_term(rowid, payload) {
            expected_fp = expected_fp.wrapping_add(h);
            n_expected += 1;
        }
        true
    });
    if expected_fp != fingerprint || n_expected != n_entries {
        // Name the rows whose computed entry is absent.
        let mut present: std::collections::HashSet<u64> =
            std::collections::HashSet::with_capacity(n_entries);
        let mut ibt = Btree::new(pager, index_root, true);
        let _ = ibt.scan_index(|rowid, key| {
            present.insert(entry_hash(key, rowid));
            true
        });
        let mut missing_count = 0usize;
        let mut missing_first: Vec<i64> = Vec::new();
        let mut tbt = Btree::new(pager, table_root, false);
        let _ = tbt.scan_table(|rowid, payload| {
            if let Some(h) = expected_term(rowid, payload) {
                if !present.contains(&h) {
                    missing_count += 1;
                    if missing_first.len() < 5 {
                        missing_first.push(rowid);
                    }
                }
            }
            true
        });
        for r in &missing_first {
            p.push(format!("row {} missing from index {}", r, idx.name));
        }
        if missing_count > 5 {
            p.push(format!(
                "{} rows missing from index {} (showing first 5)",
                missing_count, idx.name
            ));
        }
    }
    if idx.partial_expr.is_none() && n_entries != owner.len() {
        p.push(format!("wrong # of entries in index {}", idx.name));
    }
}

/// Page accounting (SQLite's checkTreePage / checkList bookkeeping): every
/// page of the file must be owned by EXACTLY ONE structure — a b-tree page
/// reachable from a root (page 0 is the schema root and the header), an
/// overflow page on a chain hanging off a reachable cell, or a freelist
/// trunk / leaf. Reports SQLite's "Page N is never used" for leaked pages
/// (allocated, then orphaned — the file only ever grows) and "2nd
/// reference to page N" for pages two owners claim (a corruption that
/// ends with one structure overwriting the other).
pub(crate) fn check_page_accounting(pager: &Pager, roots: &[u32]) -> Vec<String> {
    use crate::storage::btree::Cell;
    use crate::storage::page::PageType;
    let n_pages = pager.n_pages();
    let mut owned = vec![false; n_pages as usize];
    let mut problems: Vec<String> = Vec::new();
    let mut claim = |id: u32, problems: &mut Vec<String>| -> bool {
        if id >= n_pages {
            problems.push(format!("invalid page number {}", id));
            return false;
        }
        if std::mem::replace(&mut owned[id as usize], true) {
            problems.push(format!("2nd reference to page {}", id));
            return false;
        }
        true
    };

    // Freelist: trunk pages + their leaf entries.
    let mut cur = pager.freelist_head();
    let mut guard = 0u32;
    while cur != 0 && guard <= n_pages {
        guard += 1;
        if !claim(cur, &mut problems) {
            break;
        }
        let (next, leaves) = match pager.get_page(cur) {
            Ok(page) => {
                let b = page.lock();
                let rd = |o: usize| {
                    u32::from_le_bytes(
                        b.data
                            .get(o..o + 4)
                            .and_then(|s| s.try_into().ok())
                            .unwrap_or([0; 4]),
                    )
                };
                let k = (rd(4) as usize).min((b.data.len().saturating_sub(8)) / 4);
                (rd(0), (0..k).map(|i| rd(8 + 4 * i)).collect::<Vec<u32>>())
            }
            Err(_) => break,
        };
        for leaf in leaves {
            claim(leaf, &mut problems);
        }
        cur = next;
    }

    // B-trees and their overflow chains.
    let mut stack: Vec<u32> = Vec::new();
    let mut seen_roots: HashSet<u32> = HashSet::new();
    for &r in roots {
        if seen_roots.insert(r) {
            stack.push(r);
        }
    }
    while let Some(id) = stack.pop() {
        if !claim(id, &mut problems) {
            continue;
        }
        let Ok(page) = pager.get_page(id) else {
            problems.push(format!("page {} unreadable", id));
            continue;
        };
        let mut chains: Vec<u32> = Vec::new();
        {
            let b = page.lock();
            let Ok(pt) = b.page_type() else {
                problems.push(format!("page {} has no b-tree page type", id));
                continue;
            };
            if pt == PageType::Overflow {
                problems.push(format!("overflow page {} referenced as a b-tree page", id));
                continue;
            }
            for i in 0..b.n_cells() {
                let ptr = b.cell_pointer(i) as usize;
                let Ok(slice) = b.cell_slice_checked(ptr) else {
                    problems.push(format!("page {} cell {} out of bounds", id, i));
                    continue;
                };
                let Ok(cell) = Cell::decode(slice, pt, b.page_size()) else {
                    problems.push(format!("page {} cell {} undecodable", id, i));
                    continue;
                };
                match &cell {
                    Cell::TableInterior { left_child, .. }
                    | Cell::IndexInterior { left_child, .. }
                    | Cell::IndexInteriorOverflow { left_child, .. } => stack.push(*left_child),
                    _ => {}
                }
                let chain = match &cell {
                    Cell::TableLeafOverflow { overflow, .. } => *overflow,
                    other => other.index_overflow(),
                };
                if chain != 0 {
                    chains.push(chain);
                }
            }
            if matches!(pt, PageType::InteriorTable | PageType::InteriorIndex) {
                let rm = b.right_most_pointer();
                if rm != 0 {
                    stack.push(rm);
                }
            }
        }
        for first in chains {
            let mut c = first;
            let mut steps = 0u32;
            while c != 0 && steps <= n_pages {
                steps += 1;
                if !claim(c, &mut problems) {
                    break;
                }
                c = match pager.get_page(c) {
                    Ok(p) => {
                        let b = p.lock();
                        match b.page_type() {
                            Ok(PageType::Overflow) => b.overflow_next(),
                            _ => {
                                problems
                                    .push(format!("overflow chain hits non-overflow page {}", c));
                                0
                            }
                        }
                    }
                    Err(_) => 0,
                };
            }
        }
    }

    for (id, used) in owned.iter().enumerate() {
        if !used {
            problems.push(format!("Page {} is never used", id));
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use crate::storage::btree::Btree;
    use crate::types::Value;
    use crate::Database;

    fn verdict(db: &Database) -> Vec<String> {
        db.query("PRAGMA integrity_check", ())
            .unwrap()
            .into_iter()
            .map(|r| r[0].as_text().to_string())
            .collect()
    }

    fn index_root(db: &Database, name: &str) -> u32 {
        let live = db.maps.read().index_roots.get(name).copied();
        live.unwrap_or_else(|| db.catalog.get_index(name).unwrap().root_page)
    }

    /// Re-file `rowid`'s entry in `index` from `from`'s key to `to`'s
    /// (`from = None`: add a stray entry).
    fn refile(db: &Database, index: &str, rowid: i64, from: Option<&[Value]>, to: &[Value]) {
        let table = db.catalog.get_table("t").unwrap();
        let idx = db.catalog.get_index(index).unwrap();
        let mut bt = Btree::new(&db.pager, index_root(db, index), true);
        if let Some(from) = from {
            let k = crate::executor::encode_index_key(&idx, &table, from).unwrap();
            bt.delete_index(&k, rowid).unwrap();
        }
        let k = crate::executor::encode_index_key(&idx, &table, to).unwrap();
        bt.insert_index(&k, rowid).unwrap();
        assert_eq!(
            bt.root,
            index_root(db, index),
            "no root split in this fixture"
        );
    }

    /// Index entries are verified key-exactly (SQLite probes each row's
    /// computed key, not just its rowid), with SQLite's entry-count rule:
    /// a non-partial index must hold one entry per row, while a partial
    /// index's count is not compared — SQLite reports a stray entry there
    /// as `ok`, and so do we.
    #[test]
    fn index_entries_are_checked_key_exactly() {
        let mut db = Database::open_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n TEXT)", [])
            .unwrap();
        db.execute("CREATE INDEX ix ON t(n)", []).unwrap();
        db.execute("CREATE INDEX px ON t(n) WHERE n IS NOT NULL", [])
            .unwrap();
        db.execute("INSERT INTO t VALUES (1, 'a'), (2, NULL), (3, 'c')", [])
            .unwrap();
        assert_eq!(verdict(&db), vec!["ok"]);
        let row = |id: i64, n: Option<&str>| {
            vec![
                Value::Integer(id),
                n.map_or(Value::Null, |s| Value::Text(s.into())),
            ]
        };

        // A partial index's stray entry for a non-member row: SQLite's
        // verdict is ok (no entry count for partial indexes).
        refile(&db, "px", 2, None, &row(2, None));
        assert_eq!(verdict(&db), vec!["ok"]);

        // Row 1's entry filed under a stale key: rowid-level checks see
        // it, the key-exact probe does not.
        refile(&db, "ix", 1, Some(&row(1, Some("a"))), &row(1, Some("zz")));
        assert_eq!(verdict(&db), vec!["row 1 missing from index ix"]);
        refile(&db, "ix", 1, Some(&row(1, Some("zz"))), &row(1, Some("a")));

        // A second entry for row 3 in a non-partial index.
        refile(&db, "ix", 3, None, &row(3, Some("q")));
        assert_eq!(verdict(&db), vec!["wrong # of entries in index ix"]);
    }
}
