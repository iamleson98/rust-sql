//! Intra-statement parallel aggregation and top-N — the concurrency
//! frontier SQLite categorically does not have (its executor is
//! single-threaded by design). A large single-table aggregate
//! (`SELECT SUM(v), COUNT(*), AVG(v) FROM big`, `... WHERE v > K`,
//! `SELECT cat, COUNT(*) FROM big GROUP BY cat`) or a bounded top-N
//! (`SELECT ... FROM big ORDER BY k LIMIT n [OFFSET o]`) splits the
//! table's rowid space across worker threads; each worker runs the SAME
//! fused/compiled walk or keep-heap selection the serial path runs, over
//! its own rowid range; the main thread merges the partial accumulators
//! or survivor sets deterministically.
//!
//! ## Why this is safe with zero new locking
//!
//! A statement's execution is bracketed by the engine's aliasing model:
//! reads run under `&Database` (the outer `RwLock<Database>` read guard
//! at the server/pool layer, or a plain `&` borrow at the engine layer),
//! and a writer needs `&mut Database` — Rust's type system guarantees no
//! writer can interleave WITHIN one engine call. Workers spawned inside
//! that call therefore observe a write-free world for their whole
//! lifetime; pages are individually `Arc<Mutex<Page>>` locked, so
//! concurrent readers are the engine's bread and butter already.
//!
//! ## Correctness contract
//!
//! - Workers use the serial path's own machinery (`FusedWalk`,
//!   `HashGrouper`, `decode_row_selective`) — byte-identical per-row
//!   semantics, partitioned by rowid range.
//! - Partials merge in RANGE order (worker 0's rowids < worker 1's ...),
//!   so GROUP BY first-seen group order and no-GROUP-BY integer results
//!   are IDENTICAL to the serial scan. REAL SUMs merge in range order:
//!   float addition is not associative, so the last ULP can differ from
//!   a serial scan — the same latitude every parallel SQL engine (and
//!   SQLite's own unspecified summation order) takes.
//! - Any worker bail (TEXT payload in a numeric fused shape, corrupt
//!   cell) aborts the whole parallel attempt: the caller falls back to
//!   the untouched serial path, so results can never diverge.
//!
//! ## Eligibility gates (all must hold; otherwise serial, unchanged)
//!
//! - `PRAGMA parallel_scan` enabled (default: min-rows threshold 131072;
//!   0 disables — see `Pager::parallel_scan_min_rows`).
//! - `!ctx.in_transaction` — no open write transaction on the engine:
//!   worker threads are foreign to the committed-view TLS arming, so
//!   they must only run when live pages ARE the committed state.
//! - Row estimate (`Btree::max_rowid_hint`, O(log N)) >= the threshold.
//! - Aggregates in {COUNT, SUM, TOTAL, AVG, MIN, MAX}; no GROUP_CONCAT /
//!   JSON group aggregates (order-dependent across ranges); DISTINCT is
//!   supported via set-union replay; GROUP BY keys must be bare columns
//!   (the selective branch); filters must be simple numeric comparisons
//!   (the fused filter shape).
//!
//! Worker count: `available_parallelism()` capped at 16, and scaled down
//! for smaller tables (one worker per >= 32768 estimated rows).

use std::sync::Arc as StdArc;

use crate::error::Result;
use crate::planner::plan::AggExpr;
use crate::schema::Table;
use crate::sql::ast::Expr;
use crate::storage::btree::Btree;
use crate::types::Value;

use super::{
    agg_sep_at, fused_filter_of, fused_finalize, fused_merge_acc, fused_resolve_col, AggFunc,
    AggState, ExecContext, ExecResult, FusedAcc, FusedAggSpec, FusedOp, FusedWalk, HashGrouper,
    FUSED_MAX_COLS,
};

/// Minimum estimated rows per worker (below this, fewer threads pay off
/// more than they return).
const MIN_ROWS_PER_WORKER: i64 = 32_768;

/// Hard cap on workers per statement (bounds worker-side scratch memory:
/// one `FusedWalk`/`HashGrouper` + buffers each).
const MAX_WORKERS: usize = 16;

/// Reclaim worker-thread mimalloc heaps after a parallel scope joins.
///
/// Worker threads allocate their per-range scratch (FusedWalk buffers,
/// partial HashGroupers, keep-heaps) in mimalloc THREAD-LOCAL heaps. When
/// a scoped worker thread exits, mimalloc *abandons* its heap — the pages
/// stay resident (RSS) and are NOT automatically reused by the next
/// statement's fresh worker threads. Measured on GROUP BY 100k buckets:
/// each parallel statement left ~+22 MiB of abandoned-heap RSS behind, so
/// a best-of-3 section climbed 71 -> 93 -> ... MiB while the live set
/// never grew. `mi_collect` (inside `drain_mimalloc_wake`) reclaims the
/// abandoned pools and madvises fully-freed pages back to the OS; run
/// once per statement after the scope joins, its ~170 µs amortizes over
/// the multi-ms parallel scan. No-op without the mimalloc feature.
fn reclaim_worker_heaps() {
    crate::api::drain_mimalloc_wake();
}

/// The rowid split handed to the workers (inclusive ranges).
#[derive(Debug)]
pub(crate) struct RangeSplit {
    pub ranges: Vec<(i64, i64)>,
}

/// Decide the worker split for a scan of `root`, or `None` when the
/// parallel path declines (config off / txn open / table too small).
pub(crate) fn plan_range_split(ctx: &ExecContext<'_>, root: u32) -> Result<Option<RangeSplit>> {
    // Gate 1: config (PRAGMA parallel_scan; 0 = disabled).
    let min_rows = ctx.pager.parallel_scan_min_rows();
    if min_rows <= 0 {
        return Ok(None);
    }
    // Gate 2: no open transaction (worker threads are foreign to the
    // committed-view TLS; live pages must BE the committed state).
    if ctx.in_transaction {
        return Ok(None);
    }
    // Gate 3: cheap row estimate.
    let bt = Btree::new(ctx.pager, root, false);
    let max_rowid = bt.max_rowid_hint()?;
    if max_rowid < min_rows {
        return Ok(None);
    }
    // Worker count: hardware threads, scaled to the work size.
    let hw = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let by_rows = ((max_rowid / MIN_ROWS_PER_WORKER) as usize).max(1);
    let n_workers = hw.min(by_rows).clamp(1, MAX_WORKERS);
    if n_workers < 2 {
        return Ok(None); // a single worker is just the serial path + spawn cost
    }
    // Split the FULL rowid space [i64::MIN, max_rowid] into n_workers
    // inclusive ranges. Worker 0's range EXTENDS to -infinity so explicit
    // rowids at or below 0 (legal SQLite values — the insert path allows
    // explicit 0/negative rowids) are never dropped: they live in worker
    // 0's low tail, which is empty for normal auto-assigned rowid tables
    // (a range walk below 1 is one descent + an immediate stop). The u128
    // cut arithmetic is overflow-safe for any max_rowid (i64::MAX
    // included).
    let n = n_workers;
    let cuts: Vec<i64> = (1..n)
        .map(|i| {
            ((i as u128 * max_rowid.max(1) as u128) / n as u128).min(i64::MAX as u128) as i64 + 1
        })
        .collect();
    let mut ranges: Vec<(i64, i64)> = Vec::with_capacity(n);
    // Worker 0: everything at or below cuts[0]-1 (covers ALL rowids < 1).
    let first_hi = cuts.first().map(|c| c - 1).unwrap_or(max_rowid);
    ranges.push((i64::MIN, first_hi));
    // Middle workers: [c_i, c_{i+1}-1].
    for w in cuts.windows(2) {
        ranges.push((w[0], w[1] - 1));
    }
    // Last worker: [last cut, max_rowid].
    if let Some(&last) = cuts.last() {
        ranges.push((last, max_rowid));
    }
    if ranges.len() < 2 {
        return Ok(None);
    }
    // Sanity: the ranges must tile [MIN, max_rowid] with no gaps or
    // overlaps (a cut sequence must be strictly increasing).
    debug_assert_eq!(ranges[0].0, i64::MIN);
    debug_assert_eq!(ranges.last().map(|r| r.1), Some(max_rowid));
    for w in ranges.windows(2) {
        debug_assert_eq!(w[0].1 + 1, w[1].0, "ranges must tile without gaps");
    }
    Ok(Some(RangeSplit { ranges }))
}

/// Build the fused spec set for a no-GROUP-BY aggregate list, or `None`
/// when a shape is unsupported (same shape checks as the serial fused
/// path — kept in lockstep by construction: the serial path runs the
/// exact `FusedWalk` this plan feeds).
struct FusedPlan {
    specs: Vec<FusedAggSpec>,
    wanted_cols: Vec<usize>,
    filter: Option<super::FusedFilter>,
    filter_slot: Option<usize>,
}

fn fused_plan_of(
    table: &Table,
    prefix: &str,
    filter_predicate: Option<&Expr>,
    aggregates: &[AggExpr],
) -> Option<FusedPlan> {
    if aggregates.is_empty() {
        return None;
    }
    let filter = match filter_predicate {
        None => None,
        Some(p) => Some(fused_filter_of(p, table, prefix)?),
    };
    let mut specs: Vec<FusedAggSpec> = Vec::with_capacity(aggregates.len());
    let mut wanted_cols: Vec<usize> = Vec::new();
    if let Some(f) = &filter {
        wanted_cols.push(f.col);
    }
    for agg in aggregates {
        if agg.distinct {
            // The fused machine has no DISTINCT state; the generic path
            // owns those shapes.
            return None;
        }
        let op = match AggFunc::from_name(&agg.func) {
            AggFunc::Count => match &agg.arg {
                None => FusedOp::CountStar,
                Some(_) => FusedOp::CountCol,
            },
            AggFunc::Sum => FusedOp::Sum,
            AggFunc::Total => FusedOp::Total,
            AggFunc::Avg => FusedOp::Avg,
            AggFunc::Min => FusedOp::Min,
            AggFunc::Max => FusedOp::Max,
            _ => return None,
        };
        let col = match &agg.arg {
            None => None,
            Some(arg) => fused_resolve_col(arg, table, prefix),
        };
        if let Some(c) = col {
            if !wanted_cols.contains(&c) {
                wanted_cols.push(c);
            }
        }
        specs.push(FusedAggSpec { op, slot: None });
    }
    if wanted_cols.len() > FUSED_MAX_COLS {
        return None;
    }
    wanted_cols.sort_unstable();
    wanted_cols.dedup();
    let slot_of = |col: usize| wanted_cols.iter().position(|&c| c == col);
    for (agg, spec) in aggregates.iter().zip(specs.iter_mut()) {
        if let Some(arg) = &agg.arg {
            if let Some(c) = fused_resolve_col(arg, table, prefix) {
                spec.slot = slot_of(c);
            }
        }
    }
    let filter_slot = filter.as_ref().and_then(|f| slot_of(f.col));
    Some(FusedPlan {
        specs,
        wanted_cols,
        filter,
        filter_slot,
    })
}

/// Parallel no-GROUP-BY fused aggregate. `Ok(None)` = declined (caller
/// runs the serial path unchanged). A worker bail also returns `None` —
/// the whole attempt is discarded, never a wrong answer.
pub(crate) fn try_parallel_fused_aggregate(
    ctx: &ExecContext<'_>,
    table: &StdArc<Table>,
    alias: Option<&str>,
    filter_predicate: Option<&Expr>,
    aggregates: &[AggExpr],
) -> Result<Option<ExecResult>> {
    let prefix = alias.unwrap_or(&table.name);
    let plan = match fused_plan_of(table, prefix, filter_predicate, aggregates) {
        Some(p) => p,
        None => return Ok(None),
    };
    let root = ctx.table_root(table);
    let split = match plan_range_split(ctx, root)? {
        Some(s) => s,
        None => return Ok(None),
    };

    // Column -> slot lookup table (usize::MAX = not wanted).
    let n_cols = table.n_columns();
    let mut col_slot = vec![usize::MAX; n_cols];
    for (slot, &col) in plan.wanted_cols.iter().enumerate() {
        col_slot[col] = slot;
    }

    let ranges = split.ranges;
    let pager = ctx.pager;
    let rowid_alias = table.rowid_alias;

    // Scoped threads: workers borrow `pager`, the plan and `col_slot`
    // immutably; the main thread is quiescent until the scope joins.
    let partials: Vec<Result<FusedWalk>> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(ranges.len());
        for &(lo, hi) in ranges.iter() {
            let col_slot = &col_slot;
            let specs = &plan.specs;
            let filter = plan.filter.as_ref();
            let filter_slot = plan.filter_slot;
            handles.push(scope.spawn(move || -> Result<FusedWalk> {
                let mut bt = Btree::new(pager, root, false);
                let mut walk = FusedWalk::new(
                    col_slot.clone(),
                    n_cols,
                    rowid_alias,
                    specs.clone(),
                    filter.cloned(),
                    filter_slot,
                );
                bt.scan_table_range_borrowed(lo, hi, |rowid, payload| walk.row(rowid, payload))?;
                Ok(walk)
            }));
        }
        handles
            .into_iter()
            .map(|h| h.join().expect("parallel aggregate worker panicked"))
            .collect()
    });
    reclaim_worker_heaps();

    // Any bail / error / empty -> fall back to the serial path.
    let mut walks: Vec<FusedWalk> = Vec::with_capacity(partials.len());
    for p in partials {
        match p {
            Ok(w) if !w.bailed => walks.push(w),
            _ => return Ok(None),
        }
    }
    if walks.is_empty() {
        return Ok(None);
    }

    // Merge in range order (deterministic; identical to the serial scan
    // for integer accumulators — see the module docs). OP-AWARE: the
    // accumulator layout differs per aggregate (see fused_merge_acc).
    let mut accs: Vec<FusedAcc> = walks[0].accs.clone();
    for w in &walks[1..] {
        for ((dst, src), spec) in accs.iter_mut().zip(w.accs.iter()).zip(plan.specs.iter()) {
            fused_merge_acc(dst, src, spec.op);
        }
    }
    Ok(Some(fused_finalize(aggregates, &accs)))
}

/// Parallel GROUP BY over the selective branch's shape: bare-column keys,
/// bare-column aggregate args (or COUNT(*)), no filter. Each worker runs
/// an exact replica of the serial selective loop over its rowid range
/// into its own `HashGrouper`; the merge folds partial groups in RANGE
/// order, which reproduces the serial scan's first-seen group order
/// exactly (a group's global first occurrence is in the earliest range
/// containing it, and earlier ranges interned their keys first).
///
/// Returns the merged grouper for `finish_group_result`, or `Ok(None)`
/// when the shape is unsupported / below threshold (serial fallback).
#[allow(clippy::too_many_arguments)]
pub(crate) fn try_parallel_groupby_selective(
    ctx: &ExecContext<'_>,
    table: &StdArc<Table>,
    key_col_indices: &[Option<usize>],
    agg_col_indices: &[Option<usize>],
    aggregates: &[AggExpr],
    n_cols: usize,
) -> Result<Option<HashGrouper>> {
    // Shape gates: the serial selective branch requires every key and
    // arg to be a resolved bare column (or COUNT(*) with no arg).
    if key_col_indices.iter().any(|k| k.is_none()) {
        return Ok(None);
    }
    for (i, agg) in aggregates.iter().enumerate() {
        if agg.arg.is_some() && agg_col_indices.get(i).map_or(true, |a| a.is_none()) {
            return Ok(None);
        }
        // Order-dependent / exotic aggregates decline (set semantics are
        // handled by the merge, but concat order across ranges is not
        // the serial order).
        match AggFunc::from_name(&agg.func) {
            AggFunc::Count
            | AggFunc::Sum
            | AggFunc::Total
            | AggFunc::Avg
            | AggFunc::Min
            | AggFunc::Max => {}
            _ => return Ok(None),
        }
    }

    let root = ctx.table_root(table);
    let split = match plan_range_split(ctx, root)? {
        Some(s) => s,
        None => return Ok(None),
    };

    // Wanted columns (the serial selective branch's exact set).
    let mut wanted: Vec<usize> = key_col_indices.iter().filter_map(|x| *x).collect();
    wanted.extend(agg_col_indices.iter().filter_map(|x| *x));
    wanted.sort_unstable();
    wanted.dedup();

    // Per-agg decode positions + COUNT(*) flags (mirrors the serial
    // branch's precomputed tables).
    let agg_pos: Vec<Option<usize>> = agg_col_indices
        .iter()
        .map(|widx| widx.and_then(|c| wanted.iter().position(|x| *x == c)))
        .collect();
    let agg_count_star: Vec<bool> = aggregates.iter().map(|a| a.arg.is_none()).collect();
    let key_pos: Vec<usize> = key_col_indices
        .iter()
        .map(|k| {
            k.and_then(|c| wanted.iter().position(|x| *x == c))
                .unwrap_or(usize::MAX)
        })
        .collect();
    let agg_funcs: Vec<AggFunc> = aggregates
        .iter()
        .map(|a| AggFunc::from_name(&a.func))
        .collect();
    let n_aggs = aggregates.len();
    let rowid_alias = table.rowid_alias;

    let ranges = split.ranges;
    let pager = ctx.pager;

    let partials: Vec<Result<HashGrouper>> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(ranges.len());
        for &(lo, hi) in ranges.iter() {
            let wanted = &wanted;
            let key_pos = &key_pos;
            let agg_pos = &agg_pos;
            let agg_count_star = &agg_count_star;
            let agg_funcs = &agg_funcs;
            let distincts: Vec<bool> = aggregates.iter().map(|a| a.distinct).collect();
            let seps: Vec<Option<String>> = (0..aggregates.len())
                .map(|i| agg_sep_at(aggregates, i).map(|s| s.to_string()))
                .collect();
            handles.push(scope.spawn(move || -> Result<HashGrouper> {
                let mut bt = Btree::new(pager, root, false);
                let mut grouper = HashGrouper::with_aggs(n_aggs);
                let mut sel_buf: Vec<Value> = Vec::with_capacity(wanted.len().max(1));
                let mut key_buf: Vec<Value> = Vec::with_capacity(key_pos.len());
                bt.scan_table_range_borrowed(lo, hi, |rowid, payload| {
                    if crate::storage::row_codec::decode_row_selective(
                        payload,
                        n_cols,
                        wanted,
                        rowid,
                        rowid_alias,
                        &mut sel_buf,
                    )
                    .is_err()
                    {
                        return true; // skip corrupt rows (serial branch's contract)
                    }
                    key_buf.clear();
                    for &pos in key_pos.iter() {
                        key_buf.push(if pos != usize::MAX && pos < sel_buf.len() {
                            sel_buf[pos].clone()
                        } else {
                            Value::Null
                        });
                    }
                    let gi = grouper.intern_key(&key_buf);
                    for i in 0..n_aggs {
                        let arg_val: &Value = if agg_count_star[i] {
                            &super::COUNT_STAR_ARG
                        } else {
                            match agg_pos[i] {
                                Some(pos) if pos < sel_buf.len() => &sel_buf[pos],
                                _ => &Value::Null,
                            }
                        };
                        super::update_agg_state(
                            grouper.state(gi, i),
                            agg_funcs[i],
                            arg_val,
                            distincts[i],
                            seps[i].as_deref(),
                        );
                    }
                    true
                })?;
                Ok(grouper)
            }));
        }
        handles
            .into_iter()
            .map(|h| h.join().expect("parallel group-by worker panicked"))
            .collect()
    });
    reclaim_worker_heaps();

    let mut groupers: Vec<HashGrouper> = Vec::with_capacity(partials.len());
    for p in partials {
        match p {
            Ok(g) => groupers.push(g),
            Err(_) => return Ok(None), // any worker error -> serial fallback
        }
    }
    if groupers.is_empty() {
        return Ok(None);
    }

    // Merge in range order into a fresh grouper: for each worker group,
    // intern its key, then fold its AggStates into the destination.
    let mut merged = HashGrouper::with_aggs(n_aggs);
    let mut key_buf: Vec<Value> = Vec::with_capacity(key_pos.len().max(1));
    for g in groupers {
        let n_groups = g.len();
        for gi in 0..n_groups {
            key_buf.clear();
            match &g.keys_flat {
                Some(flat) => key_buf.push(flat.get(gi).clone()),
                None => key_buf.extend(g.keys_multi[gi].iter().cloned()),
            }
            let dst_gi = merged.intern_key(&key_buf);
            for i in 0..n_aggs {
                let src = g.states.get(gi * n_aggs + i);
                // Safety of the split borrow: `merged.state` mutably
                // borrows `merged`, `src` borrows `g` (a different
                // object).
                merge_agg_state(
                    merged.state(dst_gi, i),
                    src,
                    agg_funcs[i],
                    aggregates[i].distinct,
                    agg_sep_at(aggregates, i),
                );
            }
        }
    }
    // Post-merge drain: the partial groupers freed during the loop above;
    // the post-scope drain ran while they were still live. See the
    // compiled path's comment for the full rationale.
    reclaim_worker_heaps();
    Ok(Some(merged))
}

/// Fold a partial range's `AggState` into the destination. OP-AWARE:
/// `AggState` overloads its fields per function (SUM/Total use
/// (int_sum, sum, sum_is_int); AVG uses (count, sum) with `sum` always
/// f64 and the int fields inert; COUNT uses only `count`). Non-DISTINCT
/// states merge field-wise (integer sums exactly; REAL sums in range
/// order). DISTINCT states replay the source's distinct SET through
/// `update_agg_state` — the set-union is exactly the global distinct
/// value set, and each replayed value updates the numeric state once,
/// which is precisely what the serial scan computed.
fn merge_agg_state(
    dst: &mut AggState,
    src: &AggState,
    func: AggFunc,
    distinct: bool,
    sep: Option<&str>,
) {
    if distinct {
        if let Some(set) = src.cold().and_then(|c| c.distinct.as_ref()) {
            for v in set.iter() {
                super::update_agg_state(dst, func, &v.0, true, sep);
            }
        }
        return;
    }
    match func {
        AggFunc::Avg => {
            // AVG's accumulator: (count, sum). The generic fold below
            // would hit the SUM layout (int_sum/sum_is_int) and merge
            // ZEROS — dropping the real sum on the floor.
            dst.count = dst.count.saturating_add(src.count);
            dst.sum += src.sum;
        }
        AggFunc::Count => {
            dst.count = dst.count.saturating_add(src.count);
        }
        _ => {
            dst.count = dst.count.saturating_add(src.count);
            dst.seen_value |= src.seen_value;
            // SUM/Total: int+int exact; any float flips to f64 addition.
            if dst.sum_is_int && src.sum_is_int {
                dst.int_sum = dst.int_sum.saturating_add(src.int_sum);
            } else {
                let a = if dst.sum_is_int {
                    dst.int_sum as f64
                } else {
                    dst.sum
                };
                let b = if src.sum_is_int {
                    src.int_sum as f64
                } else {
                    src.sum
                };
                dst.sum = a + b;
                dst.sum_is_int = false;
            }
            // MIN/MAX under SQL value ordering (update_agg_state's
            // comparator).
            if let Some(m) = src.cold().and_then(|c| c.min.as_ref()) {
                let better = dst
                    .cold()
                    .and_then(|c| c.min.as_ref())
                    .map_or(true, |d| m < d);
                if better {
                    dst.cold_mut().min = Some(m.clone());
                }
            }
            if let Some(m) = src.cold().and_then(|c| c.max.as_ref()) {
                let better = dst
                    .cold()
                    .and_then(|c| c.max.as_ref())
                    .map_or(true, |d| d < m);
                if better {
                    dst.cold_mut().max = Some(m.clone());
                }
            }
        }
    }
}

/// Parallel GROUP BY over the COMPILED-expression branch's shape (where
/// most real GROUP BY queries land — `keys_all_compile &&
/// args_all_compile` in `exec_aggregate_streaming_scan`, including
/// COUNT(*) whose arg-index is None). Each worker runs an exact replica
/// of the serial compiled loop — `decode_row_selective_wide` into a
/// full-width buffer, compiled key/arg evaluation, `HashGrouper`
/// accumulation — over its rowid range. Partial groupers merge in RANGE
/// order, reproducing the serial scan's first-seen group order exactly.
///
/// `filter` must be compiled (an `eval_row` fallback filter declines —
/// workers do not carry the named-parameter/name-resolution context).
#[allow(clippy::too_many_arguments)]
pub(crate) fn try_parallel_groupby_compiled(
    ctx: &ExecContext<'_>,
    table: &StdArc<Table>,
    wanted: &[usize],
    compiled_keys: &[Option<crate::executor::predicate::CompiledExpr>],
    compiled_args: &[Option<crate::executor::predicate::CompiledExpr>],
    key_col_indices: &[Option<usize>],
    agg_col_indices: &[Option<usize>],
    filter: Option<&crate::executor::predicate::CompiledPredicate>,
    aggregates: &[AggExpr],
    group_by_len: usize,
    n_cols: usize,
) -> Result<Option<HashGrouper>> {
    // Aggregate-function gates (order-dependent / exotic shapes decline).
    for agg in aggregates {
        match AggFunc::from_name(&agg.func) {
            AggFunc::Count
            | AggFunc::Sum
            | AggFunc::Total
            | AggFunc::Avg
            | AggFunc::Min
            | AggFunc::Max => {}
            _ => return Ok(None),
        }
    }
    // The caller gates this branch on keys_all_compile && args_all_compile,
    // but re-verify: a key that neither compiled nor resolved has no
    // worker-side evaluation.
    for (k, c) in key_col_indices.iter().zip(compiled_keys.iter()) {
        if k.is_none() && c.is_none() {
            return Ok(None);
        }
    }

    let root = ctx.table_root(table);
    let split = match plan_range_split(ctx, root)? {
        Some(s) => s,
        None => return Ok(None),
    };

    let agg_funcs: Vec<AggFunc> = aggregates
        .iter()
        .map(|a| AggFunc::from_name(&a.func))
        .collect();
    let n_aggs = aggregates.len();
    let rowid_alias = table.rowid_alias;
    let single_key = group_by_len == 1;
    let identity: Vec<usize> = (0..n_cols).collect();
    // Shared immutable worker inputs (scope threads borrow these).
    let params: &[Value] = &ctx.params;

    let ranges = split.ranges;
    let pager = ctx.pager;

    let partials: Vec<Result<HashGrouper>> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(ranges.len());
        for &(lo, hi) in ranges.iter() {
            let agg_funcs = &agg_funcs;
            let identity = &identity;
            let distincts: Vec<bool> = aggregates.iter().map(|a| a.distinct).collect();
            let seps: Vec<Option<String>> = (0..aggregates.len())
                .map(|i| agg_sep_at(aggregates, i).map(|s| s.to_string()))
                .collect();
            handles.push(scope.spawn(move || -> Result<HashGrouper> {
                let mut bt = Btree::new(pager, root, false);
                let mut grouper = HashGrouper::with_aggs(n_aggs);
                let mut wide: Vec<Value> = vec![Value::Null; n_cols];
                let mut key_buf: Vec<Value> = Vec::with_capacity(group_by_len.max(1));
                let mut owned_key: Value = Value::Null;
                bt.scan_table_range_borrowed(lo, hi, |rowid, payload| {
                    if crate::storage::row_codec::decode_row_selective_wide(
                        payload,
                        n_cols,
                        wanted,
                        rowid,
                        rowid_alias,
                        &mut wide,
                    )
                    .is_err()
                    {
                        return true; // skip corrupt rows (serial branch's contract)
                    }
                    if let Some(cp) = filter {
                        if !cp.eval(&wide, identity, params) {
                            return true; // filtered out
                        }
                    }
                    // Group key: single-value fast path or full key Vec
                    // (byte-identical to the serial compiled branch).
                    let gi = if single_key {
                        let kv = match &compiled_keys[0] {
                            Some(c) => c.eval(&wide, params),
                            None => wide[key_col_indices[0].unwrap()].clone(),
                        };
                        owned_key = kv;
                        grouper.intern_one(&owned_key)
                    } else {
                        key_buf.clear();
                        for i in 0..group_by_len {
                            let kv = match &compiled_keys[i] {
                                Some(c) => c.eval(&wide, params),
                                None => wide[key_col_indices[i].unwrap()].clone(),
                            };
                            key_buf.push(kv);
                        }
                        grouper.intern_key(&key_buf)
                    };
                    for i in 0..n_aggs {
                        if aggregates[i].arg.is_none() {
                            // COUNT(*): constant placeholder, no evaluation.
                            super::update_agg_state(
                                grouper.state(gi, i),
                                agg_funcs[i],
                                &super::COUNT_STAR_ARG,
                                distincts[i],
                                seps[i].as_deref(),
                            );
                            continue;
                        }
                        let arg_val = match (agg_col_indices[i], &compiled_args[i]) {
                            (Some(idx), _) => wide[idx].clone(),
                            (None, Some(c)) => c.eval(&wide, params),
                            (None, None) => Value::Null,
                        };
                        super::update_agg_state(
                            grouper.state(gi, i),
                            agg_funcs[i],
                            &arg_val,
                            distincts[i],
                            seps[i].as_deref(),
                        );
                    }
                    true
                })?;
                Ok(grouper)
            }));
        }
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .expect("parallel compiled group-by worker panicked")
            })
            .collect()
    });
    reclaim_worker_heaps();

    let mut groupers: Vec<HashGrouper> = Vec::with_capacity(partials.len());
    for p in partials {
        match p {
            Ok(g) => groupers.push(g),
            Err(_) => return Ok(None), // any worker error -> serial fallback
        }
    }
    if groupers.is_empty() {
        return Ok(None);
    }

    // Merge in range order (deterministic first-seen order, identical to
    // the serial scan — see the module docs).
    let mut merged = HashGrouper::with_aggs(n_aggs);
    let mut key_buf: Vec<Value> = Vec::with_capacity(group_by_len.max(1));
    for g in groupers {
        let n_groups = g.len();
        for gi in 0..n_groups {
            key_buf.clear();
            match &g.keys_flat {
                Some(flat) => key_buf.push(flat.get(gi).clone()),
                None => key_buf.extend(g.keys_multi[gi].iter().cloned()),
            }
            let dst_gi = merged.intern_key(&key_buf);
            for i in 0..n_aggs {
                let src = g.states.get(gi * n_aggs + i);
                merge_agg_state(
                    merged.state(dst_gi, i),
                    src,
                    agg_funcs[i],
                    aggregates[i].distinct,
                    agg_sep_at(aggregates, i),
                );
            }
        }
    }
    // The partial groupers free DURING the merge loop above (each `g`
    // moves out and drops at its iteration's end) — the post-scope drain
    // above ran while they were still live, so their worker-heap pages
    // could not be reclaimed. Drain again now that the last partial is
    // gone: mi_collect returns the (now fully-free) worker-heap pages to
    // the OS instead of leaving them as abandoned-heap RSS.
    reclaim_worker_heaps();
    Ok(Some(merged))
}

/// Parallel full-table COUNT(*) — the FastPath #0 cell-count walk
/// (`Btree::count_rows`), split by rowid range: each worker counts leaf
/// cells in its range (zero decode), the counts sum. Returns `Ok(None)`
/// when the parallel path declines.
pub(crate) fn try_parallel_count(
    ctx: &ExecContext<'_>,
    table: &StdArc<Table>,
) -> Result<Option<u64>> {
    let root = ctx.table_root(table);
    let split = match plan_range_split(ctx, root)? {
        Some(s) => s,
        None => return Ok(None),
    };
    let ranges = split.ranges;
    let pager = ctx.pager;
    let counts: Vec<Result<u64>> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(ranges.len());
        for &(lo, hi) in ranges.iter() {
            handles.push(scope.spawn(move || -> Result<u64> {
                let mut bt = Btree::new(pager, root, false);
                bt.count_rows_range(lo, hi)
            }));
        }
        handles
            .into_iter()
            .map(|h| h.join().expect("parallel count worker panicked"))
            .collect()
    });
    reclaim_worker_heaps();
    let mut total: u64 = 0;
    for c in counts {
        match c {
            Ok(n) => total = total.saturating_add(n),
            Err(_) => return Ok(None),
        }
    }
    Ok(Some(total))
}

/// Parallel streaming top-N — `ORDER BY <bare col> [DESC] LIMIT k
/// [OFFSET o]` over a bare table scan, split by rowid range: each worker
/// feeds its own `keep`-sized survivor heap from a range-selective walk
/// (the serial `exec_topn_scan` loop, verbatim), then the main thread
/// merges the per-worker survivors under the SAME strict total order
/// (key, then serial=rowid ASC).
///
/// ## Why the merge is exactly the serial answer
///
/// `topn_total_cmp` is a STRICT total order (ties break by rowid, and
/// the rowid ranges are disjoint), so the global top-`keep` set is
/// unique. Any row in the global top-keep has at most `keep - 1` rows
/// ahead of it globally, hence at most `keep - 1` in its OWN range, so
/// it survives its worker's local selection. The union of the local
/// survivor sets therefore contains the global top-keep; sorting the
/// union and taking the first `keep` yields exactly it. No float
/// latitude, no order-of-operations freedom — bit-identical to the
/// serial streaming path (and to SQLite's stable sorter for
/// scan-sourced rows, which this path's tiebreak mirrors).
///
/// `Ok(None)` = declined (below threshold / config off / txn open) —
/// the caller runs the serial `exec_topn_scan` unchanged.
#[allow(clippy::too_many_arguments)]
pub(crate) fn try_parallel_topn(
    ctx: &ExecContext<'_>,
    table: &StdArc<Table>,
    key_col: usize,
    desc: bool,
    keep: usize,
    offset: usize,
    project: Option<&[usize]>,
    out_cols: StdArc<[String]>,
) -> Result<Option<ExecResult>> {
    if keep == 0 {
        return Ok(None); // the serial path's LIMIT-0 shortcut already answers
    }
    let root = ctx.table_root(table);
    let split = match plan_range_split(ctx, root)? {
        Some(s) => s,
        None => return Ok(None),
    };

    let n_cols = table.n_columns();
    let rowid_alias = table.rowid_alias;

    // Wanted columns: the projection's columns plus the key column —
    // the serial path's exact set (scan-selective contract).
    let mut wanted: Vec<usize> = match project {
        Some(ps) => ps.to_vec(),
        None => (0..n_cols).collect(),
    };
    if !wanted.contains(&key_col) {
        wanted.push(key_col);
    }
    wanted.sort_unstable();
    wanted.dedup();
    let project_map: Vec<usize> = match project {
        Some(ps) => ps
            .iter()
            .map(|&p| wanted.iter().position(|&w| w == p).unwrap_or(0))
            .collect(),
        None => (0..wanted.len()).collect(),
    };
    let key_pos = wanted.iter().position(|&w| w == key_col).unwrap();

    let ranges = split.ranges;
    let pager = ctx.pager;

    // Workers: the serial top-N loop over each range (identical heap
    // discipline — sift-up on insert, root-replace + sift-down when the
    // candidate beats the worst survivor).
    let partials: Vec<Result<Vec<super::TopnSel>>> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(ranges.len());
        for &(lo, hi) in ranges.iter() {
            let wanted = wanted.clone();
            handles.push(scope.spawn(move || -> Result<Vec<super::TopnSel>> {
                let mut bt = Btree::new(pager, root, false);
                let mut sel: Vec<super::TopnSel> = Vec::with_capacity(keep.min(4096));
                bt.scan_table_range_selective(
                    lo,
                    hi,
                    n_cols,
                    &wanted,
                    rowid_alias,
                    |rowid, row| {
                        let key = row[key_pos].clone();
                        if sel.len() < keep {
                            let idx = sel.len();
                            sel.push(super::TopnSel {
                                key,
                                serial: rowid,
                                row,
                            });
                            super::topn_sift_up(&mut sel, idx, desc);
                        } else if super::topn_total_cmp(
                            &key,
                            rowid,
                            &sel[0].key,
                            sel[0].serial,
                            desc,
                        ) == std::cmp::Ordering::Less
                        {
                            sel[0] = super::TopnSel {
                                key,
                                serial: rowid,
                                row,
                            };
                            super::topn_sift_down(&mut sel, 0, desc);
                        }
                        true
                    },
                )?;
                Ok(sel)
            }));
        }
        handles
            .into_iter()
            .map(|h| h.join().expect("parallel top-N worker panicked"))
            .collect()
    });
    reclaim_worker_heaps();

    // Any worker error -> serial fallback (never a divergent answer).
    let mut merged: Vec<super::TopnSel> = Vec::with_capacity(keep * ranges.len());
    for p in partials {
        match p {
            Ok(sel) => merged.extend(sel),
            Err(_) => return Ok(None),
        }
    }

    // Sort the survivors under the SAME total order, keep the first
    // `keep` (= the global top-keep — see the proof above), window
    // [offset..keep) — the serial path's exact tail.
    merged.sort_by(|a, b| super::cmp_sel(a, b, desc));
    merged.truncate(keep);
    let start = offset.min(merged.len());
    let rows: Vec<Vec<Value>> = merged[start..]
        .iter()
        .map(|s| project_map.iter().map(|&w| s.row[w].clone()).collect())
        .collect();
    Ok(Some(ExecResult {
        columns: out_cols,
        rows,
    }))
}
