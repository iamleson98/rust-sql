//! SQLite's query-planner cost model, mirrored where a PLAN CHOICE changes
//! an ANSWER.
//!
//! A rowid-alias key compared with the boundary REAL -9.2233720368547758e18
//! equals the i64::MIN row under the general comparison but finds nothing
//! through an INTEGER PRIMARY KEY seek (OP_SeekRowid's ticket-#3922 gate).
//! Which one a statement gets is SQLite's plan choice — so reproducing the
//! answer means reproducing the choice: whereLoopAddBtree's loops, their
//! LogEst costs (sqlite_stat1 / STAT4, row widths), whereLoopInsert's
//! dominance and subset cost adjustment, and wherePathSolver's passes,
//! sorting costs and tie-breaks. Every rule here was verified loop by loop
//! against the bundled SQLite 3.53.4 built with WHERETRACE.

use super::{is_rowid_spelling, ScanColumnUse};
use crate::schema::{Catalog, Index, Table};
use crate::sql::ast::Expr;
use std::sync::Arc;

/// sqlite3LogEst: 10*log2(x) in SQLite's integer approximation.
pub(crate) fn log_est(x: u64) -> i32 {
    const A: [i32; 8] = [0, 2, 3, 5, 6, 7, 8, 9];
    let mut x = x;
    let mut y: i32 = 40;
    if x < 8 {
        if x < 2 {
            return 0;
        }
        while x < 8 {
            y -= 10;
            x <<= 1;
        }
    } else {
        let i = 60 - x.leading_zeros() as i32;
        y += i * 10;
        x >>= i;
    }
    A[(x & 7) as usize] + y - 10
}

/// sqlite3LogEstAdd: the LogEst of the sum of two LogEst quantities.
fn log_est_add(a: i32, b: i32) -> i32 {
    const X: [i32; 32] = [
        10, 10, 9, 9, 8, 8, 7, 7, 7, 6, 6, 6, 5, 5, 5, 4, 4, 4, 4, 3, 3, 3, 3, 3, 3, 2, 2, 2, 2, 2,
        2, 2,
    ];
    let (hi, lo) = if a >= b { (a, b) } else { (b, a) };
    if hi > lo + 49 {
        hi
    } else if hi > lo + 31 {
        hi + 1
    } else {
        hi + X[(hi - lo) as usize]
    }
}

/// where.c estLog: the LogEst of log2 of a LogEst row count.
fn est_log(n: i32) -> i32 {
    if n <= 10 {
        0
    } else {
        log_est(n as u64) - 33
    }
}

/// whereUsablePartialIndex for the loops these models build: a partial
/// index qualifies when its WHERE is `key IS NOT NULL` on the column the
/// statement constrains by `=` / `IN`, which implies it
/// (exprImpliesNotNull). A full index always qualifies.
fn partial_usable(idx: &Index, key: Option<&str>) -> bool {
    match &idx.partial_expr {
        None => true,
        Some(Expr::IsNull {
            expr,
            negated: true,
        }) => match (expr.as_ref(), key) {
            (Expr::Column { name, .. }, Some(k)) => name.eq_ignore_ascii_case(k),
            _ => false,
        },
        Some(_) => false,
    }
}

/// sqlite3DefaultRowEst's a[0] for an index without stat1 data: the
/// table's estimate (already floored at 1000 rows by the same routine),
/// halved for a partial index.
fn default_index_rows(r_size: i32, idx: &Index) -> i32 {
    r_size.max(99) - if idx.partial_expr.is_some() { 10 } else { 0 }
}

/// Column::szEst (sqlite3AddColumn / sqlite3AffinityType): the field-size
/// estimate in units of an integer's size.
fn column_sz_est(declared_type: &str) -> i32 {
    let t = declared_type.trim();
    if t.is_empty() {
        return 1;
    }
    // The standard type names (exact, case-insensitive): ANY/INT/INTEGER/
    // REAL weigh 1, BLOB/TEXT 5.
    for (name, sz) in [
        ("ANY", 1),
        ("BLOB", 5),
        ("INT", 1),
        ("INTEGER", 1),
        ("REAL", 1),
        ("TEXT", 5),
    ] {
        if t.eq_ignore_ascii_case(name) {
            return sz;
        }
    }
    // sqlite3AffinityType's rolling 4-byte hash over the declared type.
    let bytes = t.as_bytes();
    let mut h: u32 = 0;
    let mut text_or_blob = false;
    let mut size_from: Option<usize> = None;
    let mut real = false;
    for (i, &b) in bytes.iter().enumerate() {
        h = (h << 8).wrapping_add(b.to_ascii_lowercase() as u32);
        let tail = |s: &[u8; 4]| h == u32::from_be_bytes(*s);
        if tail(b"char") {
            text_or_blob = true;
            size_from = Some(i + 1);
        } else if tail(b"clob") || tail(b"text") {
            text_or_blob = true;
        } else if tail(b"blob") && !text_or_blob {
            text_or_blob = true;
            if bytes.get(i + 1) == Some(&b'(') {
                size_from = Some(i + 1);
            }
        } else if (tail(b"real") || tail(b"floa") || tail(b"doub")) && !text_or_blob {
            real = true;
        } else if h & 0x00FF_FFFF == u32::from_be_bytes(*b"\0int") {
            return 1;
        }
    }
    let _ = real;
    let v: i64 = if text_or_blob {
        match size_from {
            Some(from) => {
                let digits: String = t[from..]
                    .chars()
                    .skip_while(|c| !c.is_ascii_digit())
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                digits.parse::<i64>().unwrap_or(0)
            }
            None => 16,
        }
    } else {
        0
    };
    (v / 4 + 1).min(255) as i32
}

/// Table::szTabRow (estimateTableWidth).
fn table_width_logest(table: &Table) -> i32 {
    let mut w: i64 = table
        .columns
        .iter()
        .map(|c| column_sz_est(&c.declared_type) as i64)
        .sum();
    if table.rowid_alias.is_none() {
        w += 1;
    }
    log_est((w * 4) as u64)
}

/// Index::szIdxRow (estimateIndexWidth): the key columns plus the rowid.
fn index_width_logest(table: &Table, idx: &Index) -> i32 {
    let mut w: i64 = 1;
    for ic in &idx.columns {
        w += table
            .find_column(&ic.name)
            .map(|i| column_sz_est(&table.columns[i].declared_type) as i64)
            .unwrap_or(1);
    }
    log_est((w * 4) as u64)
}

/// Table::nRowLogEst as SQLite holds it in memory — see
/// `Catalog::sqlite_row_logest`.
fn sqlite_table_row_logest(catalog: &Catalog, table: &Table) -> i32 {
    catalog.sqlite_row_logest(&table.name)
}

/// SQLite's access-path choice for `rowid-alias IN (list of n members)`
/// on a single table — whereLoopAddBtree / whereLoopAddBtreeIndex's loop
/// costs fed through wherePathSolver, LogEst units: true when the
/// INTEGER PRIMARY KEY IN-loop (seek semantics) is the chosen path; false
/// when a full table scan or a COVERING index wins (an IN-loop on an index
/// led by the alias column, or a covering full-index scan for a SELECT) —
/// the IN then evaluates with the general comparison.
///
/// Loop costs (`rRun`) and row estimates (`nOut`):
/// * IPK IN-loop: `LogEstAdd(estLog(N), 16) + LogEst(n)`, nOut LogEst(n)
///   (the IPK probe has no stat1, so the IN-vs-scan heuristic never drops
///   it);
/// * full scan: `N + 16` (`N + 14` once the table has STAT4 samples),
///   nOut N - 1 (the unused IN term's truth-probability guess);
/// * covering index IN-loop: `LogEstAdd(estLog(N_i), nOut_i + 1 +
///   15*szIdx/szTab) + LogEst(n)` — a non-covering probe adds the table
///   lookups and can never undercut the IPK seek;
/// * covering full-index scan (not for one-pass DML): `N_i + 1 +
///   15*szIdx/szTab`.
///
/// The solver costs a path `LogEstAdd(rRun, 0)`; with an ordering
/// requirement a second pass adds `LogEstAdd(cost, sortCost) + 3` to the
/// unordered paths (the sort cost sized by the first pass's row
/// estimate), first disabling unconstrained loops when the first pass
/// chose a constrained one (whereInterstageHeuristic). Ties break on
/// nOut, then on the unsorted cost, then on insertion order (scan, IPK,
/// indexes) — verified against the bundled SQLite's WHERETRACE.
pub(super) fn sqlite_rowid_in_seeks(
    catalog: &Catalog,
    table: &Table,
    n_members: usize,
    n_other_terms: usize,
    col_use: &ScanColumnUse,
) -> bool {
    #[derive(Clone)]
    struct Loop {
        run: i32,
        n_out: i32,
        /// wherePathSatisfiesOrderBy's isOrdered: how many leading terms
        /// of the ordering the path already emits in order.
        sorted: usize,
        ipk: bool,
        constrained: bool,
        /// WhereLoop::iSortIdx: loops compete for the same slot only when
        /// equal (1 = the IPK probe, 2.. = the indexes, when the probe
        /// might help the ordering; else 0).
        sort_idx: usize,
        /// WHERE_INDEXED loops (not the IPK probe, not the table scan):
        /// (index position, nEq, terms used, covering).
        indexed: Option<(usize, u8, u8, bool)>,
    }
    // whereLoopCheaperProperSubset(x, y).
    fn cheaper_subset(x: &Loop, y: &Loop) -> bool {
        let (Some((xi, xeq, xterms, xcov)), Some((yi, yeq, yterms, ycov))) = (x.indexed, y.indexed)
        else {
            return false;
        };
        if x.run > y.run && x.n_out > y.n_out {
            return false;
        }
        if xeq < yeq && xi == yi {
            return true;
        }
        if xterms >= yterms {
            return false;
        }
        // Every term x uses is the single IN term (or none), which y uses.
        !(xcov && !ycov)
    }
    // whereLoopInsert: whereLoopAdjustCost against the indexed loops, then
    // whereLoopFindLesser — a template no better than a same-slot loop
    // (cost AND rows) is dropped; one at least as good as a loop replaces
    // it (and every later loop it also dominates).
    fn insert(loops: &mut Vec<Loop>, mut t: Loop) {
        if t.indexed.is_some() {
            for p in loops.iter() {
                if cheaper_subset(p, &t) {
                    t.run = t.run.min(p.run);
                    t.n_out = t.n_out.min(p.n_out - 1);
                } else if cheaper_subset(&t, p) {
                    t.run = t.run.max(p.run);
                    t.n_out = t.n_out.max(p.n_out + 1);
                }
            }
        }
        for j in 0..loops.len() {
            let p = &loops[j];
            if p.sort_idx != t.sort_idx {
                continue;
            }
            if p.run <= t.run && p.n_out <= t.n_out {
                return;
            }
            if p.run >= t.run && p.n_out >= t.n_out {
                let mut k = j + 1;
                while k < loops.len() {
                    let q = &loops[k];
                    if q.sort_idx == t.sort_idx {
                        if q.run <= t.run && q.n_out <= t.n_out {
                            break;
                        }
                        if q.run >= t.run && q.n_out >= t.n_out {
                            loops.remove(k);
                            continue;
                        }
                    }
                    k += 1;
                }
                loops[j] = t;
                return;
            }
        }
        loops.push(t);
    }
    let others = n_other_terms as i32;
    let r_size = sqlite_table_row_logest(catalog, table);
    let n_in = log_est(n_members as u64);
    let indexes = catalog.indexes_on_table(&table.name);
    let has_stat4 = catalog.sqlite_has_stat4(&table.name);
    let alias = table
        .rowid_alias
        .map(|i| table.columns[i].name.to_ascii_lowercase());
    let is_rowid_key = |k: &Option<String>| match k {
        Some(k) => {
            alias.as_deref() == Some(k.as_str())
                || (is_rowid_spelling(k) && table.find_column(k).is_none())
        }
        None => false,
    };
    // Paths that emit rowid order satisfy an ordering led by the rowid
    // (unique: the remaining terms are then moot).
    let rowid_ordered = col_use.order.first().is_some_and(is_rowid_key);
    let full = col_use.order.len();
    let rowid_sorted = if rowid_ordered { full } else { 0 };
    // indexMightHelpWithOrderBy for the IPK probe: any rowid term.
    let pk_sort_idx = if col_use.order.iter().any(is_rowid_key) {
        1
    } else {
        0
    };
    let mut loops: Vec<Loop> = Vec::new();
    insert(
        &mut loops,
        Loop {
            run: r_size + if has_stat4 { 14 } else { 16 },
            n_out: r_size - 1 - others,
            sorted: rowid_sorted,
            ipk: false,
            constrained: false,
            sort_idx: pk_sort_idx,
            indexed: None,
        },
    );
    insert(
        &mut loops,
        Loop {
            run: log_est_add(est_log(r_size), 16) + n_in,
            n_out: n_in - others,
            // min/max planning treats the IN as `rowid = ?`: one row per
            // key, so any ordering is trivially met.
            sorted: if rowid_ordered || col_use.min_max {
                full
            } else {
                0
            },
            ipk: true,
            constrained: true,
            sort_idx: pk_sort_idx,
            indexed: None,
        },
    );
    {
        let sz_tab = table_width_logest(table);
        for (pos, idx) in indexes.iter().enumerate() {
            if !partial_usable(idx, alias.as_deref())
                || idx.columns.iter().any(|c| c.expr.is_some())
            {
                continue;
            }
            // A usable PARTIAL index always offers its full scan.
            let partial = idx.partial_expr.is_some();
            let covering = col_use.cols.as_ref().is_some_and(|cols| {
                cols.iter()
                    .all(|c| idx.columns.iter().any(|ic| ic.name.eq_ignore_ascii_case(c)))
            });
            let stats = catalog.index_stats(&idx.name);
            let a0 = match &stats {
                Some(s) => log_est(s.rows.max(0) as u64),
                None => default_index_rows(r_size, idx),
            };
            let sz_idx = index_width_logest(table, idx);
            let k = (15 * sz_idx) / sz_tab;
            let leads_alias = alias
                .as_deref()
                .is_some_and(|a| idx.columns[0].name.eq_ignore_ascii_case(a));
            // Index order: the key columns, then the rowid — the leading
            // ordering terms it meets; a rowid term (or a UNIQUE index's
            // full key) fixes everything after it.
            let index_sorted = {
                let mut n = 0;
                for (i, o) in col_use.order.iter().enumerate() {
                    match idx.columns.get(i) {
                        Some(ic)
                            if o.as_deref()
                                .is_some_and(|o| ic.name.eq_ignore_ascii_case(o)) =>
                        {
                            n += 1
                        }
                        None if is_rowid_key(o) => {
                            n = full;
                            break;
                        }
                        _ => break,
                    }
                }
                if idx.unique && n >= idx.columns.len() {
                    full
                } else {
                    n
                }
            };
            let helps_order = col_use.order.iter().any(|o| {
                is_rowid_key(o)
                    || o.as_deref().is_some_and(|o| {
                        idx.columns.iter().any(|ic| ic.name.eq_ignore_ascii_case(o))
                    })
            });
            let sort_idx = if helps_order { pos + 2 } else { 0 };
            if !covering {
                // A non-covering index competes only as an ORDERED full
                // scan (indexMightHelpWithOrderBy): its rows plus a table
                // lookup each, less one per leading WHERE term the index
                // itself answers — the rowid IN always is. Its IN-loops
                // pay lookups too and never undercut the IPK seek.
                if helps_order || partial {
                    let lookups = a0 + 16 - if others == 0 { 1 } else { 0 };
                    insert(
                        &mut loops,
                        Loop {
                            run: log_est_add(a0 + 1 + k, lookups),
                            n_out: a0 - 1 - others,
                            sorted: index_sorted,
                            ipk: false,
                            constrained: false,
                            sort_idx,
                            indexed: Some((pos, 0, 0, false)),
                        },
                    );
                }
                continue;
            }
            // Full covering-index scan first (whereLoopAddBtree's order),
            // then the IN-loops on an alias-led index.
            if partial || ((helps_order || !col_use.one_pass) && sz_idx < sz_tab) {
                insert(
                    &mut loops,
                    Loop {
                        run: a0 + 1 + k,
                        n_out: a0 - 1 - others,
                        sorted: if leads_alias && rowid_ordered {
                            full
                        } else {
                            index_sorted
                        },
                        ipk: false,
                        constrained: false,
                        sort_idx,
                        indexed: Some((pos, 0, 0, true)),
                    },
                );
            }
            if leads_alias {
                // nOut before the IN multiplier: one row per key (stat1's
                // "N 1", or the STAT4 per-member estimate on unique keys).
                let n_out = match stats.as_ref().and_then(|s| s.distinct_prefix.first()) {
                    Some(&d) => log_est(d.max(0) as u64),
                    None if idx.unique && idx.columns.len() == 1 => 0,
                    None => 33,
                };
                // Ordering (verified ASC/DESC/UNIQUE against WHERETRACE):
                // the nEq=1 IN-loop walks the alias column in key order,
                // so a rowid-led ORDER BY is met (reverse scan for DESC);
                // the nEq=2 loop, IN on both the key and the trailing
                // rowid, never is — index IN terms count as equalities
                // only for ORDER BY-LIMIT and min/max planning.
                let min_max_unique = col_use.min_max && idx.unique && idx.columns.len() == 1;
                let eq1 = Loop {
                    run: log_est_add(est_log(a0), n_out + 1 + k) + n_in,
                    n_out: n_out + n_in - others,
                    sorted: if rowid_ordered || min_max_unique {
                        full
                    } else {
                        0
                    },
                    ipk: false,
                    constrained: true,
                    sort_idx,
                    indexed: Some((pos, 1, 1, true)),
                };
                insert(&mut loops, eq1.clone());
                // The index's trailing rowid column takes the same IN term
                // (nEq=2): costlier raw, then cost-adjusted under its nEq=1
                // subset — one row fewer at the same cost.
                // The IN-vs-scan heuristic (analyzed index, rLogSize >= 10):
                // with M = aiRowLogEst[nEq], an IN whose `M + logK + 10 <
                // nIn + rLogSize` is dropped once an earlier IN multiplied
                // the loop (nInMul >= 2; the first IN degrades to a
                // seek-scan with unchanged costs instead).
                let in_kept = |m: i32| {
                    stats.is_none()
                        || est_log(a0) < 10
                        || m + est_log(n_in) + 10 - (n_in + est_log(a0)) >= 0
                };
                let settled = loops
                    .iter()
                    .find(|l| l.indexed == Some((pos, 1, 1, true)))
                    .cloned()
                    .unwrap_or(eq1);
                if in_kept(n_out) {
                    insert(
                        &mut loops,
                        Loop {
                            run: settled.run + n_in,
                            n_out: 2 * n_in - others,
                            sorted: if min_max_unique { full } else { 0 },
                            indexed: Some((pos, 2, 2, true)),
                            ..settled
                        },
                    );
                }
            }
        }
    }
    // wherePathSolver: (rCost, nRow, rUnsort) lexicographic, first wins
    // ties; `sort` (cost of sorting given n leading terms already in
    // order) is None for the unordered first pass.
    let solve = |loops: &[Loop],
                 sort: Option<&dyn Fn(usize) -> i32>,
                 allow: &dyn Fn(&Loop) -> bool|
     -> usize {
        let mut best: Option<(usize, i32, i32, i32)> = None;
        for (i, l) in loops.iter().enumerate() {
            if !allow(l) {
                continue;
            }
            let unsorted = log_est_add(l.run, 0);
            let (cost, r_unsort) = match sort {
                Some(s) if l.sorted < full => (log_est_add(unsorted, s(l.sorted)) + 3, unsorted),
                _ => (unsorted, unsorted - 2),
            };
            let better = match best {
                None => true,
                Some((_, c, n, u)) => (cost, l.n_out, r_unsort) < (c, n, u),
            };
            if better {
                best = Some((i, cost, l.n_out, r_unsort));
            }
        }
        best.map(|b| b.0).unwrap_or(0)
    };
    let first = solve(&loops, None, &|_| true);
    if col_use.order.is_empty() {
        return loops[first].ipk;
    }
    // whereSortingCost over the first pass's row estimate; a partially
    // ordered path block-sorts only the unordered tail.
    let n_row_est = loops[first].n_out + 1;
    let n_col = log_est(((col_use.n_result_cols.max(1) + 59) / 30) as u64);
    let sort_cost = |n_sorted: usize| -> i32 {
        let mut cost = n_row_est + n_col;
        if n_sorted > 0 {
            cost += log_est(((full - n_sorted) * 100 / full) as u64) - 66;
        }
        let mut n_row = n_row_est;
        if col_use.distinct && n_row > 10 {
            n_row -= 10;
        }
        cost + est_log(n_row)
    };
    let first_constrained = loops[first].constrained;
    let second = solve(&loops, Some(&sort_cost), &|l: &Loop| {
        !first_constrained || l.constrained
    });
    loops[second].ipk
}

// ---------------------------------------------------------------------------
// Two-table joins: does SQLite SEEK the rowid-alias side?
// ---------------------------------------------------------------------------

/// One FROM item of a two-table join, as SQLite's planner sees it.
pub(crate) struct JoinItem<'a> {
    pub table: &'a Table,
    /// Lower-cased columns the statement reads from this item besides the
    /// rowid (`SrcItem::colUsed`); `None` = every column.
    pub cols: Option<Vec<String>>,
    /// Single-table WHERE/ON conjuncts on this item (each lowers a loop's
    /// row estimate by one — whereLoopOutputAdjust's no-likelihood guess).
    pub n_local_terms: usize,
    /// Some local conjunct is `col = constant` (whereLoopOutputAdjust's
    /// iReduce: 10 for a constant in -1..=1, else 20).
    pub local_eq_reduce: i32,
}

/// A term of the ordering the join loop is planned against.
pub(crate) struct JoinOrderTerm {
    /// FROM items the term reads (bit 0 = first item); 0 = constant.
    pub items: u8,
    /// A bare column of ONE item: `(item, None)` is its rowid (or rowid
    /// alias), `(item, Some(col))` a named column.
    pub column: Option<(usize, Option<String>)>,
    pub desc: bool,
}

/// A WHERE-clause term in SQLite's `pWC` order (the WHERE's conjuncts,
/// then the ON's, then the virtual commuted copy of the column equality).
pub(crate) struct JoinTermInfo {
    /// Per FROM item: the columns of that item the term reads (`None` =
    /// the rowid / rowid alias).
    pub cols: [Vec<Option<String>>; 2],
    /// `=` / `IS`: the covered-term lookup discount is 20, else 1.
    pub eq: bool,
}

/// The statement around a two-table rowid-alias equi-join.
pub(crate) struct JoinQuery<'a> {
    /// pWC, in order (see [`JoinTermInfo`]).
    pub terms: Vec<JoinTermInfo>,
    /// FROM order.
    pub items: [JoinItem<'a>; 2],
    /// The item whose key is its rowid alias (the seekable side).
    pub alias_item: usize,
    /// The other item's key column (index into its table's columns).
    pub key_col: usize,
    /// The equality's comparison affinity permits an index on the key
    /// column (sqlite3IndexAffinityOk) — gates automatic and real index
    /// probes of the non-alias side.
    pub key_index_ok: bool,
    /// pOrderBy: the GROUP BY, a DISTINCT turned GROUP BY, or the ORDER BY.
    pub order: Vec<JoinOrderTerm>,
    /// WHERE_GROUPBY: any order of the grouping columns satisfies.
    pub group_by: bool,
    /// WHERE_WANT_DISTINCT.
    pub distinct: bool,
    pub n_result_cols: usize,
    /// SF_FixedLimit: a constant positive LIMIT (non-aggregate queries).
    pub limit: Option<u64>,
}

/// One ordering column of a join loop's scan: the index column name
/// (`None` for the rowid), DESC, NOT NULL.
type OrderCol = (Option<String>, bool, bool);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum JKind {
    /// Rowid-order full table scan.
    TableScan,
    /// `alias = ?` INTEGER PRIMARY KEY seek — the SEEK semantics.
    IpkEq,
    /// Automatic index on the non-alias key (unordered).
    AutoIndex,
    /// Full scan of index `pos`.
    IndexScan(usize),
    /// Equality probe of index `pos` on the join key (`onerow`: unique
    /// single-column key).
    IndexEq(usize, bool),
}

#[derive(Clone)]
struct JLoop {
    item: usize,
    /// Needs the other item in an outer loop.
    needs_other: bool,
    setup: i32,
    run: i32,
    n_out: i32,
    sort_idx: usize,
    kind: JKind,
    /// WHERE_INDEXED loops: (nEq, terms used, covering) for the subset
    /// cost adjustment, and szIdxRow for whereLoopIsNoBetter.
    indexed: Option<(u8, u8, bool, i32)>,
}

impl JLoop {
    fn onerow(&self) -> bool {
        matches!(self.kind, JKind::IpkEq | JKind::IndexEq(_, true))
    }
    fn constrained(&self) -> bool {
        matches!(self.kind, JKind::IpkEq | JKind::IndexEq(..))
    }
}

/// SQLite's verdict for `A JOIN B ON A.alias = B.key` (INNER/CROSS, two
/// base tables): true when its chosen plan runs A as the inner loop of an
/// INTEGER PRIMARY KEY seek driven by B's key — OP_SeekRowid's conversion
/// (a boundary REAL -2^63 finds no rowid); false when A is scanned, or
/// probed through an index, and the key compares with the general
/// comparison (Real(-2^63) == Integer(i64::MIN)).
///
/// Mirrors whereLoopAddBtree (table scan, IPK equality, automatic index,
/// full-index scans that might serve the ordering, index equality probes),
/// whereLoopInsert's dominance and subset cost adjustment, and the two
/// wherePathSolver passes (the ordered pass adds whereSortingCost, with the
/// interstage heuristic in between) with wherePathSatisfiesOrderBy —
/// verified loop by loop against the bundled SQLite's WHERETRACE.
pub(crate) fn sqlite_join_seeks_alias(catalog: &Catalog, q: &JoinQuery) -> bool {
    let a = q.alias_item;
    let b = 1 - a;
    let index_lists: [Vec<Arc<Index>>; 2] = [
        catalog.indexes_on_table(&q.items[0].table.name),
        catalog.indexes_on_table(&q.items[1].table.name),
    ];
    let mut loops: Vec<JLoop> = Vec::new();
    for (item, indexes) in index_lists.iter().enumerate() {
        let it = &q.items[item];
        let table = it.table;
        let r_size = sqlite_table_row_logest(catalog, table);
        let has_stat4 = catalog.sqlite_has_stat4(&table.name);
        let locals = it.n_local_terms as i32;
        // whereLoopOutputAdjust: one row fewer per unused local term,
        // clamped below the table size by an equality's iReduce.
        let adjust = |n_out: i32| (n_out - locals).min(r_size - it.local_eq_reduce);
        let alias_col = table
            .rowid_alias
            .map(|i| table.columns[i].name.to_ascii_lowercase());
        let order_on_item = |pred: &dyn Fn(&Option<String>) -> bool| {
            q.order.iter().any(|t| match &t.column {
                Some((i, c)) => *i == item && pred(c),
                None => false,
            })
        };
        let is_rowid_col = |c: &Option<String>| match c {
            None => true,
            Some(c) => alias_col.as_deref() == Some(c.as_str()),
        };
        let pk_sort_idx = if order_on_item(&is_rowid_col) { 1 } else { 0 };
        // Automatic index on the non-alias key (never on a rowid).
        if item == b && q.key_index_ok {
            let r_log = est_log(r_size);
            insert_jloop(
                &mut loops,
                JLoop {
                    item,
                    needs_other: true,
                    setup: (r_log + r_size + 28).max(0),
                    run: log_est_add(r_log, 43),
                    n_out: 43,
                    sort_idx: 0,
                    kind: JKind::AutoIndex,
                    indexed: None,
                },
            );
        }
        insert_jloop(
            &mut loops,
            JLoop {
                item,
                needs_other: false,
                setup: 0,
                run: r_size + if has_stat4 { 14 } else { 16 },
                n_out: adjust(r_size),
                sort_idx: pk_sort_idx,
                kind: JKind::TableScan,
                indexed: None,
            },
        );
        if item == a {
            insert_jloop(
                &mut loops,
                JLoop {
                    item,
                    needs_other: true,
                    setup: 0,
                    run: log_est_add(est_log(r_size), 16),
                    n_out: adjust(0),
                    sort_idx: pk_sort_idx,
                    kind: JKind::IpkEq,
                    indexed: None,
                },
            );
        }
        let sz_tab = table_width_logest(table);
        let key_name = if item == b {
            Some(table.columns[q.key_col].name.to_ascii_lowercase())
        } else {
            alias_col.clone()
        };
        for (pos, idx) in indexes.iter().enumerate() {
            if !partial_usable(idx, key_name.as_deref())
                || idx.columns.iter().any(|c| c.expr.is_some())
            {
                continue;
            }
            // A usable PARTIAL index always offers its full scan.
            let partial = idx.partial_expr.is_some();
            let covering = it.cols.as_ref().is_some_and(|cols| {
                cols.iter()
                    .all(|c| idx.columns.iter().any(|ic| ic.name.eq_ignore_ascii_case(c)))
            });
            let stats = catalog.index_stats(&idx.name);
            let a0 = match &stats {
                Some(s) => log_est(s.rows.max(0) as u64),
                None => default_index_rows(r_size, idx),
            };
            let sz_idx = index_width_logest(table, idx);
            let k = (15 * sz_idx) / sz_tab;
            let helps = order_on_item(&|c: &Option<String>| {
                is_rowid_col(c)
                    || c.as_deref().is_some_and(|c| {
                        idx.columns.iter().any(|ic| ic.name.eq_ignore_ascii_case(c))
                    })
            });
            let sort_idx = if helps { pos + 2 } else { 0 };
            if helps || partial || (covering && sz_idx < sz_tab) {
                let base = a0 + 1 + k;
                let run = if covering {
                    base
                } else {
                    // Table lookups, discounted for each leading term the
                    // index alone can evaluate (sqlite3ExprCoveredByIndex:
                    // only THIS item's columns must be in the index — the
                    // rowid always is; another item's columns are free).
                    let mut lookups = a0 + 16;
                    for t in &q.terms {
                        let covered = t.cols[item].iter().all(|c| match c {
                            None => true,
                            Some(c) => idx.columns.iter().any(|ic| ic.name.eq_ignore_ascii_case(c)),
                        });
                        if !covered {
                            break;
                        }
                        lookups -= if t.eq { 20 } else { 1 };
                    }
                    log_est_add(base, lookups)
                };
                insert_jloop(
                    &mut loops,
                    JLoop {
                        item,
                        needs_other: false,
                        setup: 0,
                        run,
                        n_out: adjust(a0),
                        sort_idx,
                        kind: JKind::IndexScan(pos),
                        indexed: Some((0, 0, covering, sz_idx)),
                    },
                );
            }
            // Equality probe on the join key through this index.
            let leads_key = key_name
                .as_deref()
                .is_some_and(|kn| idx.columns[0].name.eq_ignore_ascii_case(kn));
            if leads_key && (item == a || q.key_index_ok) {
                let n_eq = match stats.as_ref().and_then(|s| s.distinct_prefix.first()) {
                    Some(&d) => log_est(d.max(0) as u64),
                    None if idx.unique && idx.columns.len() == 1 => 0,
                    None => 33,
                };
                let mut run = log_est_add(est_log(a0), n_eq + 1 + k);
                if !covering {
                    run = log_est_add(run, n_eq + 16);
                }
                let onerow = idx.unique && idx.columns.len() == 1;
                insert_jloop(
                    &mut loops,
                    JLoop {
                        item,
                        needs_other: true,
                        setup: 0,
                        run,
                        n_out: adjust(n_eq),
                        sort_idx,
                        kind: JKind::IndexEq(pos, onerow),
                        indexed: Some((1, 1, covering, sz_idx)),
                    },
                );
                if item == a {
                    // An index on the alias column: its trailing rowid
                    // column takes the SAME rowid term (whereScanInit maps
                    // the IPK column to XN_ROWID) — a one-row nEq=2 probe
                    // whose aiRowLogEst step lands nOut at 0. It can
                    // undercut the IPK seek (`1 + 15*szIdx/szTab` < 16 on a
                    // covering index), and probes compare NUMERICALLY.
                    let mut run2 = log_est_add(est_log(a0), 1 + k);
                    if !covering {
                        run2 = log_est_add(run2, 16);
                    }
                    insert_jloop(
                        &mut loops,
                        JLoop {
                            item,
                            needs_other: true,
                            setup: 0,
                            run: run2,
                            n_out: adjust(0),
                            sort_idx,
                            kind: JKind::IndexEq(pos, true),
                            indexed: Some((2, 2, covering, sz_idx)),
                        },
                    );
                }
            }
        }
    }

    // Pass 1 (unordered), then the ordered pass when an ordering exists.
    let n_order = q.order.len();
    let no_sort = |_: usize| 0;
    let all_on = vec![true; loops.len()];
    let first = solve_join(&loops, q, &index_lists, 0, &no_sort, &all_on);
    let best = if n_order == 0 {
        first
    } else {
        // whereInterstageHeuristic: a constrained outer loop disables the
        // unconstrained loops of its table for the second pass.
        let mut enabled = all_on.clone();
        for &li in &first.loops {
            let l = &loops[li];
            if !l.constrained() {
                break;
            }
            for (j, other) in loops.iter().enumerate() {
                if other.item == l.item && !other.constrained() && other.kind != JKind::AutoIndex {
                    enabled[j] = false;
                }
            }
        }
        let n_row_est = if first.n_row < 0 { 1 } else { first.n_row + 1 };
        let n_col = log_est(((q.n_result_cols.max(1) + 59) / 30) as u64);
        let sort_cost = |n_sorted: usize| -> i32 {
            let mut cost = n_row_est + n_col;
            if n_sorted > 0 {
                cost += log_est(((n_order - n_sorted) * 100 / n_order) as u64) - 66;
            }
            let mut n_row = n_row_est;
            if let Some(limit) = q.limit {
                cost += 10;
                if n_sorted != 0 {
                    cost += 6;
                }
                let i_limit = log_est(limit);
                if i_limit < n_row {
                    n_row = i_limit;
                }
            } else if q.distinct && n_row > 10 {
                n_row -= 10;
            }
            cost + est_log(n_row)
        };
        solve_join(&loops, q, &index_lists, n_order, &sort_cost, &enabled)
    };
    best.loops.iter().any(|&li| loops[li].kind == JKind::IpkEq)
}

/// whereLoopInsert for the join model: subset cost adjustment among the
/// indexed loops of one table, then whereLoopFindLesser.
fn insert_jloop(loops: &mut Vec<JLoop>, mut t: JLoop) {
    if let Some((teq, tterms, tcov, _)) = t.indexed {
        for p in loops.iter() {
            let Some((peq, pterms, pcov, _)) = p.indexed else {
                continue;
            };
            if p.item != t.item {
                continue;
            }
            let same_index = matches!(
                (p.kind, t.kind),
                (JKind::IndexScan(x) | JKind::IndexEq(x, _), JKind::IndexScan(y) | JKind::IndexEq(y, _))
                    if x == y
            );
            // whereLoopCheaperProperSubset(x, y)
            let subset = |xrun: i32,
                          xout: i32,
                          xeq: u8,
                          xterms: u8,
                          xcov: bool,
                          yrun: i32,
                          yout: i32,
                          yeq: u8,
                          yterms: u8,
                          ycov: bool| {
                if xrun > yrun && xout > yout {
                    return false;
                }
                if xeq < yeq && same_index {
                    return true;
                }
                if xterms >= yterms {
                    return false;
                }
                !(xcov && !ycov)
            };
            if subset(
                p.run, p.n_out, peq, pterms, pcov, t.run, t.n_out, teq, tterms, tcov,
            ) {
                t.run = t.run.min(p.run);
                t.n_out = t.n_out.min(p.n_out - 1);
            } else if subset(
                t.run, t.n_out, teq, tterms, tcov, p.run, p.n_out, peq, pterms, pcov,
            ) {
                t.run = t.run.max(p.run);
                t.n_out = t.n_out.max(p.n_out + 1);
            }
        }
    }
    let prereq = |l: &JLoop| -> u8 {
        if l.needs_other {
            1 << (1 - l.item)
        } else {
            0
        }
    };
    let tp = prereq(&t);
    for j in 0..loops.len() {
        let p = &loops[j];
        if p.item != t.item || p.sort_idx != t.sort_idx {
            continue;
        }
        let pp = prereq(p);
        // An application index with an equality beats an automatic one.
        if p.kind == JKind::AutoIndex && matches!(t.kind, JKind::IndexEq(..)) && (pp & tp) == tp {
            loops[j] = t;
            return;
        }
        if (pp & tp) == pp && p.setup <= t.setup && p.run <= t.run && p.n_out <= t.n_out {
            return;
        }
        if (pp & tp) == tp && p.run >= t.run && p.n_out >= t.n_out {
            let mut k = j + 1;
            while k < loops.len() {
                let q = &loops[k];
                if q.item == t.item && q.sort_idx == t.sort_idx {
                    let qp = prereq(q);
                    if (qp & tp) == qp && q.setup <= t.setup && q.run <= t.run && q.n_out <= t.n_out
                    {
                        break;
                    }
                    if (qp & tp) == tp && q.run >= t.run && q.n_out >= t.n_out {
                        loops.remove(k);
                        continue;
                    }
                }
                k += 1;
            }
            loops[j] = t;
            return;
        }
    }
    loops.push(t);
}

#[derive(Clone)]
struct JPath {
    loops: Vec<usize>,
    mask: u8,
    n_row: i32,
    cost: i32,
    unsort: i32,
    ordered: i32,
}

/// wherePathSolver over the join's two levels (mxChoice 5 never binds
/// with two tables: at most one kept path per (mask, ordered-ness)).
fn solve_join(
    loops: &[JLoop],
    q: &JoinQuery,
    index_lists: &[Vec<Arc<Index>>; 2],
    n_order: usize,
    sort_cost: &dyn Fn(usize) -> i32,
    enabled: &[bool],
) -> JPath {
    let mut from = vec![JPath {
        loops: Vec::new(),
        mask: 0,
        n_row: 0,
        cost: 0,
        unsort: 0,
        ordered: if n_order > 0 { -1 } else { 0 },
    }];
    for level in 0..2 {
        let last = level == 1;
        let mut to: Vec<JPath> = Vec::new();
        for pf in &from {
            for (li, l) in loops.iter().enumerate() {
                if !enabled[li] {
                    continue;
                }
                let self_bit = 1u8 << l.item;
                if pf.mask & self_bit != 0 {
                    continue;
                }
                if l.needs_other && pf.mask & (1 << (1 - l.item)) == 0 {
                    continue;
                }
                if l.kind == JKind::AutoIndex && pf.n_row < 3 {
                    continue;
                }
                let mut unsort = l.run + pf.n_row;
                if l.setup != 0 {
                    unsort = log_est_add(l.setup, unsort);
                }
                unsort = log_est_add(unsort, pf.unsort);
                let n_out = pf.n_row + l.n_out;
                let mask = pf.mask | self_bit;
                let mut path_loops = pf.loops.clone();
                path_loops.push(li);
                let ordered = if pf.ordered >= 0 {
                    pf.ordered
                } else {
                    join_order_satisfied(loops, q, index_lists, &path_loops)
                };
                let cost;
                if ordered >= 0 && (ordered as usize) < n_order {
                    cost = log_est_add(unsort, sort_cost(ordered as usize)) + 3;
                } else {
                    cost = unsort;
                    unsort -= 2;
                }
                let slot = to
                    .iter()
                    .position(|t| t.mask == mask && (last || (t.ordered < 0) == (ordered < 0)));
                let cand = JPath {
                    loops: path_loops,
                    mask,
                    n_row: n_out,
                    cost,
                    unsort,
                    ordered,
                };
                match slot {
                    None => to.push(cand),
                    Some(j) => {
                        let t = &to[j];
                        let no_better = || {
                            // whereLoopIsNoBetter on the new level's loops:
                            // only an indexed candidate with a NARROWER
                            // index than an indexed baseline wins a tie.
                            let base = &loops[*t.loops.last().unwrap()];
                            match (l.indexed, base.indexed) {
                                (Some(c), Some(b)) => c.3 >= b.3,
                                _ => true,
                            }
                        };
                        if t.cost < cost
                            || (t.cost == cost && t.n_row < n_out)
                            || (t.cost == cost && t.n_row == n_out && t.unsort < unsort)
                            || (t.cost == cost
                                && t.n_row == n_out
                                && t.unsort == unsort
                                && no_better())
                        {
                            continue;
                        }
                        to[j] = cand;
                    }
                }
            }
        }
        from = to;
    }
    let mut best = from[0].clone();
    for p in from.iter().skip(1) {
        if p.cost < best.cost {
            best = p.clone();
        }
    }
    best
}

/// wherePathSatisfiesOrderBy for the join model's loop kinds: the number
/// of leading ordering terms the path emits in order, or -1 when the path
/// is still well-ordered but incomplete (a later loop may finish it).
fn join_order_satisfied(
    loops: &[JLoop],
    q: &JoinQuery,
    index_lists: &[Vec<Arc<Index>>; 2],
    path: &[usize],
) -> i32 {
    let n = q.order.len();
    let all: u64 = (1u64 << n) - 1;
    let mut sat: u64 = 0;
    let mut distinct = true;
    let mut distinct_mask: u8 = 0;
    let mut ready: u8 = 0;
    for (pi, &li) in path.iter().enumerate() {
        if !(distinct && sat != all) {
            break;
        }
        let l = &loops[li];
        if pi > 0 {
            ready |= 1 << loops[path[pi - 1]].item;
        }
        let table = q.items[l.item].table;
        let alias_col = table
            .rowid_alias
            .map(|i| table.columns[i].name.to_ascii_lowercase());
        let is_rowid = |c: &Option<String>| match c {
            None => true,
            Some(c) => alias_col.as_deref() == Some(c.as_str()),
        };
        // ORDER BY terms fixed by an equality on outer loops: the join
        // key of this item, once the other item is ready.
        let other_ready = ready & (1 << (1 - l.item)) != 0;
        if other_ready {
            for (i, t) in q.order.iter().enumerate() {
                if sat & (1 << i) != 0 {
                    continue;
                }
                if let Some((it, c)) = &t.column {
                    if *it != l.item {
                        continue;
                    }
                    let fixed = if l.item == q.alias_item {
                        is_rowid(c)
                    } else {
                        c.as_deref()
                            .is_some_and(|c| table.columns[q.key_col].name.eq_ignore_ascii_case(c))
                    };
                    if fixed {
                        sat |= 1 << i;
                    }
                }
            }
        }
        if !l.onerow() {
            // (index column name or None for the rowid, DESC)
            let (cols, n_eq, n_key, unique): (Vec<OrderCol>, usize, usize, bool) = match l.kind {
                JKind::TableScan => (vec![(None, false, true)], 0, 0, false),
                JKind::AutoIndex => return 0,
                JKind::IndexScan(pos) | JKind::IndexEq(pos, _) => {
                    let idx = &index_lists[l.item][pos];
                    let mut v: Vec<OrderCol> = idx
                        .columns
                        .iter()
                        .map(|ic| {
                            let name = ic.name.to_ascii_lowercase();
                            let not_null = table
                                .find_column(&name)
                                .is_some_and(|ci| !table.columns[ci].nullable);
                            let desc = matches!(ic.order, crate::sql::ast::Order::Desc);
                            if alias_col.as_deref() == Some(name.as_str()) {
                                (None, desc, true)
                            } else {
                                (Some(name), desc, not_null)
                            }
                        })
                        .collect();
                    let n_key = v.len();
                    v.push((None, false, true));
                    let n_eq = if matches!(l.kind, JKind::IndexEq(..)) {
                        1
                    } else {
                        0
                    };
                    (v, n_eq, n_key, idx.unique)
                }
                JKind::IpkEq => unreachable!("one-row"),
            };
            if !matches!(l.kind, JKind::TableScan) {
                distinct = unique;
            }
            let mut rev: Option<bool> = None;
            let mut distinct_cols = false;
            for (j, (col, rev_idx, not_null)) in cols.iter().enumerate() {
                if j < n_eq {
                    continue;
                }
                if distinct && col.is_some() && !not_null {
                    distinct = false;
                }
                let mut matched = false;
                for (i, t) in q.order.iter().enumerate() {
                    if sat & (1 << i) != 0 {
                        continue;
                    }
                    let hit = match &t.column {
                        Some((it, c)) => {
                            *it == l.item
                                && match col {
                                    None => is_rowid(c),
                                    Some(cn) => c.as_deref() == Some(cn.as_str()),
                                }
                        }
                        None => false,
                    };
                    if hit {
                        let ok = q.group_by
                            || match rev {
                                Some(r) => (r ^ rev_idx) == t.desc,
                                None => {
                                    rev = Some(rev_idx ^ t.desc);
                                    true
                                }
                            };
                        if ok {
                            matched = true;
                            sat |= 1 << i;
                            if col.is_none() {
                                distinct_cols = true;
                            }
                        }
                        break;
                    }
                    if !q.group_by {
                        break; // ORDER BY: only the first unsatisfied term
                    }
                }
                if !matched {
                    if j == 0 || j < n_key {
                        distinct = false;
                    }
                    break;
                }
            }
            if distinct_cols {
                distinct = true;
            }
        }
        if distinct {
            distinct_mask |= 1 << l.item;
            for (i, t) in q.order.iter().enumerate() {
                if sat & (1 << i) == 0 && t.items & !distinct_mask == 0 {
                    sat |= 1 << i;
                }
            }
        }
    }
    if sat == all {
        return n as i32;
    }
    if !distinct {
        for i in (1..n).rev() {
            let m = (1u64 << i) - 1;
            if sat & m == m {
                return i as i32;
            }
        }
        return 0;
    }
    -1
}
