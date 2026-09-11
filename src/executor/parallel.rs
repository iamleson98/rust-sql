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

use std::option::Option as StdOption;
use std::sync::Arc as StdArc;

use crate::error::Result;
use crate::planner::plan::AggExpr;
use crate::schema::Table;
use crate::sql::ast::Expr;
use crate::storage::btree::Btree;
use crate::types::Value;
use crate::Row;

use super::{
    agg_sep_at, fused_filter_of, fused_finalize, fused_merge_acc, fused_resolve_col, AggFunc,
    AggState, ExecContext, ExecResult, FusedAcc, FusedAggSpec, FusedOp, FusedWalk, HashGrouper,
    FUSED_MAX_COLS,
};

/// One resolved ORDER BY term of the parallel sort: (column position,
/// DESC, collation resolved ONCE on the main thread — see
/// `sort_total_cmp` for why resolution must not happen in workers).
type SortKey = (usize, bool, StdOption<StdArc<dyn crate::plugin::Collation>>);

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
    params: &[Value],
    named_params: &std::collections::HashMap<String, Value>,
) -> Option<FusedPlan> {
    if aggregates.is_empty() {
        return None;
    }
    let filter = match filter_predicate {
        None => None,
        Some(p) => {
            // Bound parameters resolve ONCE here against the statement's
            // fixed parameter set (see fused_filter_lit): the worker-
            // split walk and the serial fused walk see the same value.
            Some(fused_filter_of(p, table, prefix, params, named_params)?)
        }
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
            // An expression argument (`total(v * 1.0)`) must DECLINE
            // (propagates None): slot None is the COUNT(*) placeholder
            // (NumVal::I(1) per row), so a Sum/Total/Avg spec with no
            // slot would accumulate 1-per-row — the serial fused path
            // rejects this exact shape (None => return Ok(None)), and
            // the parallel plan builder must keep the contract identical.
            Some(arg) => Some(fused_resolve_col(arg, table, prefix)?),
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
    let plan = match fused_plan_of(
        table,
        prefix,
        filter_predicate,
        aggregates,
        &ctx.params,
        &ctx.named_params,
    ) {
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
    // TLS (conn text encoding) does not cross threads: each worker
    // re-installs the tag copied on the spawning thread.
    let conn_enc_tag = super::conn_enc::current();
    let partials: Vec<Result<FusedWalk>> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(ranges.len());
        for &(lo, hi) in ranges.iter() {
            let col_slot = &col_slot;
            let specs = &plan.specs;
            let filter = plan.filter.as_ref();
            let filter_slot = plan.filter_slot;
            handles.push(scope.spawn(move || -> Result<FusedWalk> {
                let _enc_guard = super::conn_enc::reinstall(conn_enc_tag);
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

/// Parallel no-GROUP-BY DISTINCT aggregates (`SELECT COUNT(DISTINCT x),
/// SUM(DISTINCT y) ... FROM big` — with an optional compiled filter):
/// per-range `AggState` sets accumulate through the serial
/// `update_agg_state` (the DISTINCT arm interns `SqlValueKey`s in a
/// set), partials fold in RANGE ORDER via `merge_agg_state` (the
/// DISTINCT arm replays the worker's set into the destination — the
/// set-union is exactly the global distinct value set, each replayed
/// value updating the numeric state once, which is precisely what the
/// serial scan computed), and the merged states finish through the
/// serial `finish_no_group_by` — column naming and finalization
/// identical to the serial paths.
///
/// Gates: at least one DISTINCT aggregate (pure non-DISTINCT lists stay
/// with the fused machine); every aggregate in {COUNT, SUM, TOTAL, AVG,
/// MIN, MAX} — GROUP_CONCAT and the JSON group aggregates are
/// order-dependent across ranges and decline; every arg a bare column
/// of the scan or COUNT(*); a present filter must COMPILE positionally
/// (`compile_predicate`), matching the serial compiled path the decline
/// falls back to. `Ok(None)` = declined (shape below threshold / config
/// off / txn open / a worker error) — the caller runs the serial path
/// unchanged, so answers can never diverge.
pub(crate) fn try_parallel_distinct_aggregate(
    ctx: &ExecContext<'_>,
    table: &StdArc<Table>,
    alias: Option<&str>,
    filter_predicate: Option<&Expr>,
    aggregates: &[AggExpr],
) -> Result<Option<ExecResult>> {
    if !aggregates.iter().any(|a| a.distinct) {
        return Ok(None); // the fused machine owns non-DISTINCT shapes
    }
    for agg in aggregates {
        match AggFunc::from_name(&agg.func) {
            AggFunc::Count
            | AggFunc::Sum
            | AggFunc::Total
            | AggFunc::Avg
            | AggFunc::Min
            | AggFunc::Max => {}
            _ => return Ok(None), // GROUP_CONCAT / JSON / percentile: order-dependent
        }
    }
    let prefix = alias.unwrap_or(&table.name);
    let n_cols = table.n_columns();

    // Bare-column args only (COUNT(*) carries no arg). The resolution
    // mirrors exec_aggregate_no_group_by's agg_col_indices discipline —
    // an alias REPLACES the table name, so a `t.col` reference under
    // `FROM t t2` must not bind.
    let mut agg_col_indices: Vec<Option<usize>> = Vec::with_capacity(aggregates.len());
    for agg in aggregates {
        match &agg.arg {
            None => agg_col_indices.push(None),
            Some(Expr::Column { table: ref_t, name }) => {
                let matches = ref_t
                    .as_ref()
                    .map(|t| {
                        if prefix == table.name {
                            t == &table.name || t == prefix
                        } else {
                            t == prefix
                        }
                    })
                    .unwrap_or(true);
                if matches {
                    match table.find_column(name) {
                        Some(idx) => agg_col_indices.push(Some(idx)),
                        None => return Ok(None), // unresolvable: serial path evaluates
                    }
                } else {
                    return Ok(None);
                }
            }
            _ => return Ok(None), // expression args keep the serial paths
        }
    }

    // Filter: must compile positionally (the serial compiled path's own
    // gate); an uncompilable predicate declines.
    let compiled = filter_predicate
        .and_then(|p| crate::executor::predicate::compile_predicate(p, table, prefix));

    let root = ctx.table_root(table);
    let split = match plan_range_split(ctx, root)? {
        Some(s) => s,
        None => return Ok(None),
    };

    // Wanted columns: aggregate args + the filter's columns (the serial
    // compiled path's exact set), sorted + deduped; positions maps table
    // column -> decoded-slice position.
    let mut wanted: Vec<usize> = agg_col_indices.iter().filter_map(|x| *x).collect();
    if let Some(pred) = &compiled {
        crate::executor::predicate::compiled_columns(pred, &mut wanted);
    }
    wanted.sort_unstable();
    wanted.dedup();
    let mut positions = vec![usize::MAX; n_cols];
    for (pos, &c) in wanted.iter().enumerate() {
        positions[c] = pos;
    }
    // Decoded-slice position per aggregate arg (usize::MAX = COUNT(*)).
    let agg_pos: Vec<usize> = agg_col_indices
        .iter()
        .map(|a| a.map(|c| positions[c]).unwrap_or(usize::MAX))
        .collect();

    let agg_funcs: Vec<AggFunc> = aggregates
        .iter()
        .map(|a| AggFunc::from_name(&a.func))
        .collect();
    let n_aggs = aggregates.len();
    let rowid_alias = table.rowid_alias;
    let ranges = split.ranges;
    let pager = ctx.pager;
    let params: &[Value] = &ctx.params;

    // Workers: an exact replica of the serial compiled-path loop over
    // each range — decode_row_selective into a reused buffer, compiled
    // filter, then update_agg_state with the DISTINCT flags.
    let conn_enc_tag = super::conn_enc::current();
    let partials: Vec<Result<(Vec<AggState>, bool)>> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(ranges.len());
        for &(lo, hi) in ranges.iter() {
            let wanted = wanted.clone();
            let agg_pos = agg_pos.clone();
            let agg_funcs = agg_funcs.clone();
            let distincts: Vec<bool> = aggregates.iter().map(|a| a.distinct).collect();
            let seps: Vec<Option<String>> = (0..aggregates.len())
                .map(|i| agg_sep_at(aggregates, i).map(|s| s.to_string()))
                .collect();
            let compiled = compiled.clone();
            let positions = positions.clone();
            handles.push(scope.spawn(move || -> Result<(Vec<AggState>, bool)> {
                let _enc_guard = super::conn_enc::reinstall(conn_enc_tag);
                let mut bt = Btree::new(pager, root, false);
                let mut states: Vec<AggState> = (0..n_aggs).map(|_| AggState::default()).collect();
                let mut saw_any_row = false;
                let mut sel_buf: Vec<Value> = Vec::with_capacity(wanted.len());
                bt.scan_table_range_borrowed(lo, hi, |rowid, payload| {
                    if crate::storage::row_codec::decode_row_selective(
                        payload,
                        n_cols,
                        &wanted,
                        rowid,
                        rowid_alias,
                        &mut sel_buf,
                    )
                    .is_err()
                    {
                        return true; // skip corrupt rows (serial contract)
                    }
                    if let Some(pred) = &compiled {
                        if !pred.eval(&sel_buf, &positions, params) {
                            return true; // filtered out
                        }
                    }
                    saw_any_row = true;
                    for i in 0..n_aggs {
                        let arg_val: &Value = if agg_pos[i] == usize::MAX {
                            &super::COUNT_STAR_ARG
                        } else if agg_pos[i] < sel_buf.len() {
                            &sel_buf[agg_pos[i]]
                        } else {
                            &Value::Null
                        };
                        super::update_agg_state(
                            &mut states[i],
                            agg_funcs[i],
                            arg_val,
                            distincts[i],
                            seps[i].as_deref(),
                        );
                    }
                    true
                })?;
                Ok((states, saw_any_row))
            }));
        }
        handles
            .into_iter()
            .map(|h| h.join().expect("parallel DISTINCT worker panicked"))
            .collect()
    });
    reclaim_worker_heaps();

    // Any worker error -> serial fallback (never a divergent answer).
    let mut states: Option<Vec<AggState>> = None;
    let mut saw_any_row = false;
    for p in partials {
        match p {
            Ok((ws, saw)) => match &mut states {
                None => {
                    states = Some(ws);
                    saw_any_row = saw;
                }
                Some(dst) => {
                    saw_any_row |= saw;
                    for i in 0..n_aggs {
                        merge_agg_state(
                            &mut dst[i],
                            &ws[i],
                            agg_funcs[i],
                            aggregates[i].distinct,
                            agg_sep_at(aggregates, i),
                        );
                    }
                }
            },
            Err(_) => return Ok(None),
        }
    }
    let states = match states {
        Some(s) => s,
        None => return Ok(None),
    };
    Ok(Some(super::finish_no_group_by(
        aggregates,
        states,
        saw_any_row,
    )?))
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
    key_collations: &[Option<String>],
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

    let conn_enc_tag = super::conn_enc::current();
    let partials: Vec<Result<HashGrouper>> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(ranges.len());
        for &(lo, hi) in ranges.iter() {
            let wanted = &wanted;
            let key_pos = &key_pos;
            let agg_pos = &agg_pos;
            let agg_count_star = &agg_count_star;
            let agg_funcs = &agg_funcs;
            let key_collations = key_collations.to_vec();
            let distincts: Vec<bool> = aggregates.iter().map(|a| a.distinct).collect();
            let seps: Vec<Option<String>> = (0..aggregates.len())
                .map(|i| agg_sep_at(aggregates, i).map(|s| s.to_string()))
                .collect();
            // Workers take the same temp-store discipline as the serial
            // path (spill-armed unless PRAGMA temp_store=MEMORY), so a
            // high-cardinality parallel GROUP BY stays bounded too.
            let spill_ok = !ctx.pager.temp_store_memory();
            let aggregates_for_grouper = aggregates;
            handles.push(scope.spawn(move || -> Result<HashGrouper> {
                let _enc_guard = super::conn_enc::reinstall(conn_enc_tag);
                let mut bt = Btree::new(pager, root, false);
                let mut grouper = if spill_ok {
                    HashGrouper::with_spill_for(
                        aggregates_for_grouper,
                        super::group_spill_threshold(),
                    )
                } else {
                    HashGrouper::with_aggs(n_aggs)
                };
                // Collated GROUP BY: workers fold keys exactly like the
                // serial path (bit-identical groups after the merge).
                grouper.set_key_collations(key_collations);
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
    // Spilled partials iterate through their own chunk merges, and the
    // destination spills under the same threshold — the whole fold runs
    // at bounded memory.
    let spill_ok = !ctx.pager.temp_store_memory();
    let mut merged = if spill_ok {
        HashGrouper::with_spill_for(aggregates, super::group_spill_threshold())
    } else {
        HashGrouper::with_aggs(n_aggs)
    };
    // The partials' RAM iterations emit FIRST-SEEN ORIGINAL keys (the
    // display form); re-folding at the merge reproduces the serial
    // grouper's groups AND its representatives exactly.
    merged.set_key_collations(key_collations.to_vec());
    for g in groupers {
        let mut iter = g.into_group_iter();
        while let Some((keys, states)) = iter.next_group() {
            let dst_gi = merged.intern_key(&keys);
            for i in 0..n_aggs {
                // Safety of the split borrow: `merged.state` mutably
                // borrows `merged`, `states` borrows the iterator's
                // record (disjoint).
                merge_agg_state(
                    merged.state(dst_gi, i),
                    &states[i],
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
///
/// The same fold is the temp-store spill merge's combiner (see
/// `GroupIter`): two partial states of ONE group key, the earlier
/// (lower seq / earlier range) as `dst`, the later as `src`. Every
/// builtin aggregate is covered — `AggFunc::Other` (plugin aggregates)
/// never reaches a HashGrouper (the plugin path has its own grouper),
/// and spill-arming declines on it defensively.
pub(crate) fn merge_agg_state(
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
            // AVG's accumulator: (count, numeric-sum) with the numeric
            // half on the SAME int-exact + KBN-compensated layout as
            // Sum (int_sum/sum_is_int/comp — see the generic
            // accumulate): count merges, then the sum folds int+int
            // exactly and flips to the compensated merge on any float
            // side.
            dst.count = dst.count.saturating_add(src.count);
            if dst.sum_is_int && src.sum_is_int {
                dst.int_sum = dst.int_sum.saturating_add(src.int_sum);
            } else {
                let b = if src.sum_is_int {
                    src.int_sum as f64
                } else {
                    src.sum
                };
                if dst.sum_is_int {
                    dst.sum = dst.int_sum as f64;
                    dst.sum_is_int = false;
                    dst.comp = 0.0;
                }
                super::kbn_step(&mut dst.sum, &mut dst.comp, b);
                dst.comp += src.comp;
            }
        }
        AggFunc::Count => {
            dst.count = dst.count.saturating_add(src.count);
        }
        AggFunc::GroupConcat => {
            // dst ++ sep ++ src — chunk/range order preserves scan order
            // (all of dst's rows precede all of src's for one key).
            if let Some(b) = src.cold().and_then(|c| c.concat.clone()) {
                if !b.is_empty() {
                    let c = dst.cold_mut();
                    let joiner = c
                        .concat_sep
                        .clone()
                        .or_else(|| sep.map(|s| s.to_string()))
                        .unwrap_or_else(|| ",".to_string());
                    let buf = c.concat.get_or_insert_with(String::new);
                    if !buf.is_empty() {
                        buf.push_str(&joiner);
                        c.concat_sep = Some(joiner);
                    }
                    buf.push_str(&b);
                }
            }
        }
        AggFunc::JsonGroupArray | AggFunc::JsonGroupObject => {
            // Elements/fragments join with ',' inside one container.
            if let Some(b) = src.cold().and_then(|c| c.concat.clone()) {
                if !b.is_empty() {
                    let buf = dst.cold_mut().concat.get_or_insert_with(String::new);
                    if !buf.is_empty() {
                        buf.push(',');
                    }
                    buf.push_str(&b);
                }
            }
        }
        AggFunc::JsonbGroupArray | AggFunc::JsonbGroupObject => {
            // Raw per-element frames — plain byte concatenation.
            if let Some(b) = src.cold().and_then(|c| c.concat_blob.clone()) {
                if !b.is_empty() {
                    dst.cold_mut()
                        .concat_blob
                        .get_or_insert_with(Vec::new)
                        .extend_from_slice(&b);
                }
            }
        }
        AggFunc::Stddev
        | AggFunc::StddevPop
        | AggFunc::Median
        | AggFunc::PercentileCont
        | AggFunc::PercentileDisc => {
            // The collected-value multiset concatenates; the percentile
            // P is constant per aggregate (kept from whichever side has
            // it — dst first, matching update's first-write semantics).
            if let Some(src_nums) = src.cold().and_then(|c| c.nums.clone()) {
                let c = dst.cold_mut();
                c.nums.get_or_insert_with(Vec::new).extend(src_nums);
                if c.pct.is_none() {
                    c.pct = src.cold().and_then(|sc| sc.pct);
                }
            }
        }
        _ => {
            dst.count = dst.count.saturating_add(src.count);
            dst.seen_value |= src.seen_value;
            // SUM/Total: int+int exact; any float side flips to the
            // KBN-compensated merge (step dst's running sum with src's,
            // absorb both error terms).
            if dst.sum_is_int && src.sum_is_int {
                dst.int_sum = dst.int_sum.saturating_add(src.int_sum);
            } else {
                let b = if src.sum_is_int {
                    src.int_sum as f64
                } else {
                    src.sum
                };
                if dst.sum_is_int {
                    dst.sum = dst.int_sum as f64;
                    dst.sum_is_int = false;
                    dst.comp = 0.0;
                }
                super::kbn_step(&mut dst.sum, &mut dst.comp, b);
                dst.comp += src.comp;
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
    key_collations: &[Option<String>],
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
    let conn_enc_tag = super::conn_enc::current();

    let partials: Vec<Result<HashGrouper>> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(ranges.len());
        for &(lo, hi) in ranges.iter() {
            let agg_funcs = &agg_funcs;
            let identity = &identity;
            let key_collations = key_collations.to_vec();
            let distincts: Vec<bool> = aggregates.iter().map(|a| a.distinct).collect();
            let seps: Vec<Option<String>> = (0..aggregates.len())
                .map(|i| agg_sep_at(aggregates, i).map(|s| s.to_string()))
                .collect();
            // Same temp-store discipline as the serial / selective paths.
            let spill_ok = !ctx.pager.temp_store_memory();
            let aggregates_for_grouper = aggregates;
            handles.push(scope.spawn(move || -> Result<HashGrouper> {
                let _enc_guard = super::conn_enc::reinstall(conn_enc_tag);
                let mut bt = Btree::new(pager, root, false);
                let mut grouper = if spill_ok {
                    HashGrouper::with_spill_for(
                        aggregates_for_grouper,
                        super::group_spill_threshold(),
                    )
                } else {
                    HashGrouper::with_aggs(n_aggs)
                };
                // Collated GROUP BY: fold like the serial compiled branch.
                grouper.set_key_collations(key_collations);
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
    // the serial scan — see the module docs). Spilled partials iterate
    // through their chunk merges; the destination spills under the same
    // threshold.
    let spill_ok = !ctx.pager.temp_store_memory();
    let mut merged = if spill_ok {
        HashGrouper::with_spill_for(aggregates, super::group_spill_threshold())
    } else {
        HashGrouper::with_aggs(n_aggs)
    };
    // Collated GROUP BY: partials emit first-seen ORIGINAL keys; the
    // merge re-folds them (bit-identical groups + representatives).
    merged.set_key_collations(key_collations.to_vec());
    for g in groupers {
        let mut iter = g.into_group_iter();
        while let Some((keys, states)) = iter.next_group() {
            let dst_gi = merged.intern_key(&keys);
            for i in 0..n_aggs {
                merge_agg_state(
                    merged.state(dst_gi, i),
                    &states[i],
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
    let conn_enc_tag = super::conn_enc::current();
    let counts: Vec<Result<u64>> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(ranges.len());
        for &(lo, hi) in ranges.iter() {
            handles.push(scope.spawn(move || -> Result<u64> {
                let _enc_guard = super::conn_enc::reinstall(conn_enc_tag);
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
    coll: Option<StdArc<dyn crate::plugin::Collation>>,
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
    // candidate beats the worst survivor). The collation is resolved
    // ONCE on the main thread and CLONED into each worker — an Arc from
    // the process-global registry, so every comparison (worker heap,
    // main-thread merge) uses the same comparator the serial path
    // resolved.
    let conn_enc_tag = super::conn_enc::current();
    let partials: Vec<Result<Vec<super::TopnSel>>> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(ranges.len());
        for &(lo, hi) in ranges.iter() {
            let wanted = wanted.clone();
            let coll = coll.clone();
            handles.push(scope.spawn(move || -> Result<Vec<super::TopnSel>> {
                let _enc_guard = super::conn_enc::reinstall(conn_enc_tag);
                let coll_ref = coll.as_deref();
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
                            super::topn_sift_up(&mut sel, idx, desc, coll_ref);
                        } else if super::topn_total_cmp(
                            &key,
                            rowid,
                            &sel[0].key,
                            sel[0].serial,
                            desc,
                            coll_ref,
                        ) == std::cmp::Ordering::Less
                        {
                            sel[0] = super::TopnSel {
                                key,
                                serial: rowid,
                                row,
                            };
                            super::topn_sift_down(&mut sel, 0, desc, coll_ref);
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
    merged.sort_by(|a, b| super::cmp_sel(a, b, desc, coll.as_deref()));
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

/// Parallel streaming top-N with an EXPRESSION key (`ORDER BY <expr>
/// [COLLATE x] [DESC] LIMIT k [OFFSET o]` over a big bare scan): the
/// fused hash of the bare-column `try_parallel_topn` — workers
/// wide-decode their ranges (all table columns + the rowid trailing
/// slot), EVALUATE the compiled key once per row, and run the identical
/// keep-heap discipline; the main thread merges the survivor sets under
/// the same strict total order and windows [offset..keep).
///
/// `coll` is the main-thread-resolved collation (see `sort_total_cmp`);
/// `project` is the projection's table-column indices (ROWID_PROJ emits
/// the rowid) or None for the star shape. `Ok(None)` = declined — the
/// caller runs the serial path unchanged.
pub(crate) fn try_parallel_topn_expr(
    ctx: &ExecContext<'_>,
    table: &StdArc<Table>,
    compiled: &crate::executor::predicate::CompiledExpr,
    coll: Option<StdArc<dyn crate::plugin::Collation>>,
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
    let rowid_slot = n_cols;
    let rowid_proj = crate::storage::row_codec::ROWID_PROJ;
    let identity: Vec<usize> = (0..n_cols).collect();
    let params: &[Value] = &ctx.params;

    let ranges = split.ranges;
    let pager = ctx.pager;
    // Workers: the serial expression-key loop over each range (identical
    // heap discipline — sift-up on insert, root-replace + sift-down when
    // the candidate beats the worst survivor).
    let conn_enc_tag = super::conn_enc::current();
    let partials: Vec<Result<Vec<super::TopnSel>>> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(ranges.len());
        for &(lo, hi) in ranges.iter() {
            let compiled = compiled.clone();
            let coll = coll.clone();
            let identity = &identity;
            handles.push(scope.spawn(move || -> Result<Vec<super::TopnSel>> {
                let _enc_guard = super::conn_enc::reinstall(conn_enc_tag);
                let coll_ref = coll.as_deref();
                let mut bt = Btree::new(pager, root, false);
                let mut sel: Vec<super::TopnSel> = Vec::with_capacity(keep.min(4096));
                let mut wide: Vec<Value> = Vec::with_capacity(n_cols + 1);
                bt.scan_table_range_borrowed(lo, hi, |rowid, payload| {
                    if crate::storage::row_codec::decode_row_selective_wide(
                        payload,
                        n_cols,
                        identity,
                        rowid,
                        rowid_alias,
                        &mut wide,
                    )
                    .is_err()
                    {
                        return true; // skip corrupt rows (serial contract)
                    }
                    wide.push(Value::Integer(rowid));
                    let key = compiled.eval(&wide, params);
                    let row: Row = match project {
                        Some(ps) => ps
                            .iter()
                            .map(|&p| {
                                if p == rowid_proj {
                                    Value::Integer(rowid)
                                } else {
                                    wide.get(p).cloned().unwrap_or(Value::Null)
                                }
                            })
                            .collect(),
                        None => wide[..n_cols].to_vec(),
                    };
                    if sel.len() < keep {
                        let idx = sel.len();
                        sel.push(super::TopnSel {
                            key,
                            serial: rowid,
                            row,
                        });
                        super::topn_sift_up(&mut sel, idx, desc, coll_ref);
                    } else if super::topn_total_cmp(
                        &key,
                        rowid,
                        &sel[0].key,
                        sel[0].serial,
                        desc,
                        coll_ref,
                    ) == std::cmp::Ordering::Less
                    {
                        sel[0] = super::TopnSel {
                            key,
                            serial: rowid,
                            row,
                        };
                        super::topn_sift_down(&mut sel, 0, desc, coll_ref);
                    }
                    if wide.len() > rowid_slot {
                        wide.truncate(rowid_slot);
                    }
                    true
                })?;
                Ok(sel)
            }));
        }
        handles
            .into_iter()
            .map(|h| h.join().expect("parallel expression top-N worker panicked"))
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
    // `keep`, window [offset..keep) — the serial path's exact tail.
    merged.sort_by(|a, b| super::cmp_sel(a, b, desc, coll.as_deref()));
    merged.truncate(keep);
    let start = offset.min(merged.len());
    let rows: Vec<Vec<Value>> = merged.drain(start..).map(|s| s.row).collect();
    Ok(Some(ExecResult {
        columns: out_cols,
        rows,
    }))
}

// ============================================================================
// Parallel unbounded ORDER BY — chunk sort + k-way range-ordered merge
// ============================================================================

/// Multi-term (keys, rowid) total-order comparator, shared verbatim by
/// the worker local sorts and the main-thread merge. Terms compare with
/// `Value::cmp` (the serial sort's `sort_key` comparator for bare
/// columns) with DESC reversal; the rowid tiebreak makes the order
/// STRICT, which is what makes the chunk/merge decomposition reproduce
/// the serial answer bit-for-bit.
///
/// The collation is a RESOLVED `Arc<dyn Collation>`, captured ONCE on
/// the MAIN thread before workers spawn. Custom (user-registered)
/// collations live in a THREAD-LOCAL statement scope — a worker-side
/// name lookup would see no scope and silently fall back to BINARY
/// while the main-thread merge used the real collation, k-way-merging
/// chunks sorted under a DIFFERENT order (a divergent answer). The
/// main-thread resolution hands every comparison — worker heap, worker
/// local sort, main-thread merge — the same comparator object; a
/// missing name resolves to None, which is the serial comparator's
/// exact connection-comparator fallback.
fn sort_total_cmp(
    a_row: &[Value],
    a_rowid: i64,
    b_row: &[Value],
    b_rowid: i64,
    keys: &[SortKey],
) -> std::cmp::Ordering {
    for &(col, desc, ref coll) in keys {
        let ord = match coll.as_deref() {
            Some(c) => crate::plugin::compare_collated(&a_row[col], &b_row[col], c),
            None => super::value_cmp_conn(&a_row[col], &b_row[col]),
        };
        let ord = if desc { ord.reverse() } else { ord };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    a_rowid.cmp(&b_rowid)
}

// ============================================================================
// Parallel unbounded ORDER BY with EXPRESSION terms — compiled keys
// ============================================================================

/// One EXPRESSION-TERM sort key: (compiled positional expression, DESC,
/// collation resolved ONCE on the main thread — see `sort_total_cmp`).
type ExprSortTerm = (
    crate::executor::predicate::CompiledExpr,
    bool,
    StdOption<StdArc<dyn crate::plugin::Collation>>,
);

/// (keys, rowid) strict total order over MATERIALIZED key values — the
/// worker local sorts and the main-thread merge share it verbatim. The
/// rowid tiebreak makes the order strict, reproducing the serial stable
/// sort's tie behavior (equal keys keep scan order).
fn expr_sort_cmp(
    a_keys: &[Value],
    a_rowid: i64,
    b_keys: &[Value],
    b_rowid: i64,
    terms: &[ExprSortTerm],
) -> std::cmp::Ordering {
    for (i, (_, desc, ref coll)) in terms.iter().enumerate() {
        let ord = match coll.as_deref() {
            Some(c) => crate::plugin::compare_collated(&a_keys[i], &b_keys[i], c),
            None => super::value_cmp_conn(&a_keys[i], &b_keys[i]),
        };
        let ord = if *desc { ord.reverse() } else { ord };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    a_rowid.cmp(&b_rowid)
}

/// One range worker's chunk of the expression-term sort:
/// (rowid, materialized key values, output row) in `expr_sort_cmp` order.
struct ExprSortChunk {
    rows: Vec<(i64, Vec<Value>, Row)>,
}

/// Parallel unbounded ORDER BY whose terms are EXPRESSIONS over the
/// scanned table (`ORDER BY v * -1, id + 1 DESC, name COLLATE NOCASE`):
/// every term compiles to a positional `CompiledExpr` (literals, table
/// columns, parameters, unary/binary arithmetic — anything outside that
/// set declines, including user function calls), the collations resolve
/// ONCE on the main thread (see `sort_total_cmp` for the worker-scope
/// divergence this prevents), and each worker:
///
/// 1. wide-decodes its range's rows (all table columns, the rowid
///    appended as a trailing slot — the compiled Col(n_cols) reads it),
/// 2. evaluates each term's key value ONCE per row (O(n) evals; the
///    serial path re-evaluates per comparison, O(n log n)),
/// 3. sorts its chunk under the strict (keys, rowid) total order.
///
/// The main thread k-way merges under the SAME order — the materialized
/// key of row r equals the serial comparator's `sort_key` for every term
/// (same shared unary/binary evaluators), and the rowid tiebreak is the
/// serial stable sort's tie order, so the merged output is identical to
/// the serial answer.
///
/// `project == Some(indices)` fuses a 1:1 bare-column projection above
/// the sort (indices are table columns; `ROWID_PROJ` emits the rowid);
/// `project == None` emits the scan's own shape (hidden-rowid slot
/// appended when the scan emits one).
///
/// `Ok(None)` = declined (below threshold / config off / txn open / a
/// worker error) — the caller runs the serial path unchanged.
#[allow(clippy::too_many_arguments)]
pub(crate) fn try_parallel_sort_expr(
    ctx: &ExecContext<'_>,
    table: &StdArc<Table>,
    terms: &[(
        crate::executor::predicate::CompiledExpr,
        bool,
        StdOption<String>,
    )],
    project: Option<&[usize]>,
    append_rowid: bool,
    out_cols: StdArc<[String]>,
) -> Result<Option<ExecResult>> {
    let root = ctx.table_root(table);
    let split = match plan_range_split(ctx, root)? {
        Some(s) => s,
        None => return Ok(None),
    };
    // Resolve every term's collation ONCE on the MAIN thread (the
    // statement's plugin scope lives here; workers must not look names
    // up — see sort_total_cmp's doc).
    let resolved: Vec<ExprSortTerm> = terms
        .iter()
        .map(|(compiled, desc, name)| {
            (
                compiled.clone(),
                *desc,
                name.as_deref().and_then(crate::plugin::lookup_collation),
            )
        })
        .collect();

    let n_cols = table.n_columns();
    let rowid_alias = table.rowid_alias;
    let rowid_slot = n_cols; // trailing slot the worker appends
    let identity: Vec<usize> = (0..n_cols).collect();
    let rowid_proj = crate::storage::row_codec::ROWID_PROJ;

    let ranges = split.ranges;
    let pager = ctx.pager;
    let params: &[Value] = &ctx.params;

    // Workers: wide decode + per-row key materialization + local sort.
    let conn_enc_tag = super::conn_enc::current();
    let partials: Vec<Result<ExprSortChunk>> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(ranges.len());
        for &(lo, hi) in ranges.iter() {
            let resolved = &resolved;
            let identity = &identity;
            handles.push(scope.spawn(move || -> Result<ExprSortChunk> {
                let _enc_guard = super::conn_enc::reinstall(conn_enc_tag);
                let mut bt = Btree::new(pager, root, false);
                let mut wide: Vec<Value> = Vec::with_capacity(n_cols + 1);
                let mut rows: Vec<(i64, Vec<Value>, Row)> = Vec::new();
                bt.scan_table_range_borrowed(lo, hi, |rowid, payload| {
                    if crate::storage::row_codec::decode_row_selective_wide(
                        payload,
                        n_cols,
                        identity,
                        rowid,
                        rowid_alias,
                        &mut wide,
                    )
                    .is_err()
                    {
                        return true; // skip corrupt rows (serial contract)
                    }
                    // Trailing rowid slot (Col(n_cols) reads it; ordinal
                    // terms over the hidden slot address it too).
                    wide.push(Value::Integer(rowid));
                    let keys: Vec<Value> = resolved
                        .iter()
                        .map(|(c, _, _)| c.eval(&wide, params))
                        .collect();
                    // Output row: the scan's own shape — with the hidden
                    // rowid slot when the scan emits one, else the table
                    // columns only.
                    let row: Row = if append_rowid {
                        wide.clone()
                    } else {
                        wide[..n_cols].to_vec()
                    };
                    rows.push((rowid, keys, row));
                    wide.truncate(rowid_slot);
                    true
                })?;
                rows.sort_unstable_by(|a, b| expr_sort_cmp(&a.1, a.0, &b.1, b.0, resolved));
                Ok(ExprSortChunk { rows })
            }));
        }
        handles
            .into_iter()
            .map(|h| h.join().expect("parallel expression-sort worker panicked"))
            .collect()
    });
    reclaim_worker_heaps();

    // Any worker error -> serial fallback (never a divergent answer).
    let mut chunks: Vec<ExprSortChunk> = Vec::with_capacity(partials.len());
    for p in partials {
        match p {
            Ok(c) => chunks.push(c),
            Err(_) => return Ok(None),
        }
    }
    if chunks.is_empty() {
        return Ok(None);
    }

    // k-way merge under the same strict total order; the emit maps the
    // projection indices over the scan-shaped row (ROWID_PROJ -> the
    // rowid) or passes the row through.
    let rows = expr_chunks_merge(chunks, &resolved, &|rowid, row| match project {
        Some(ps) => ps
            .iter()
            .map(|&p| {
                if p == rowid_proj {
                    Value::Integer(rowid)
                } else {
                    row.get(p).cloned().unwrap_or(Value::Null)
                }
            })
            .collect(),
        None => row,
    });

    Ok(Some(ExecResult {
        columns: out_cols,
        rows,
    }))
}

/// k-way merge of expression-sorted chunks under the shared strict
/// (keys, tiebreak-index) total order, emitting through `emit` (the
/// tiebreak index and the scan-shaped row in; the output row out). A
/// binary heap of chunk indices; rows MOVE out of their chunk via
/// mem::take (the output reuses each row's allocation).
fn expr_chunks_merge(
    mut chunks: Vec<ExprSortChunk>,
    terms: &[ExprSortTerm],
    emit: &dyn Fn(i64, Row) -> Row,
) -> Vec<Row> {
    let mut cursors: Vec<usize> = vec![0; chunks.len()];
    let mut heap: Vec<usize> = (0..chunks.len())
        .filter(|&c| !chunks[c].rows.is_empty())
        .collect();

    // Sift-down over the chunk-index heap (heads read through the
    // immutable chunks/cursors between sifts).
    fn sift_down(
        heap: &mut [usize],
        mut i: usize,
        chunks: &[ExprSortChunk],
        cursors: &[usize],
        terms: &[ExprSortTerm],
    ) {
        let n = heap.len();
        loop {
            let l = 2 * i + 1;
            let r = l + 1;
            let mut best = i;
            if l < n {
                let (bl, bi) = (&chunks[heap[l]], cursors[heap[l]]);
                let (bb, bidx) = (&chunks[heap[best]], cursors[heap[best]]);
                if expr_sort_cmp(
                    &bl.rows[bi].1,
                    bl.rows[bi].0,
                    &bb.rows[bidx].1,
                    bb.rows[bidx].0,
                    terms,
                ) == std::cmp::Ordering::Less
                {
                    best = l;
                }
            }
            if r < n {
                let (br, ri) = (&chunks[heap[r]], cursors[heap[r]]);
                let (bb, bidx) = (&chunks[heap[best]], cursors[heap[best]]);
                if expr_sort_cmp(
                    &br.rows[ri].1,
                    br.rows[ri].0,
                    &bb.rows[bidx].1,
                    bb.rows[bidx].0,
                    terms,
                ) == std::cmp::Ordering::Less
                {
                    best = r;
                }
            }
            if best == i {
                return;
            }
            heap.swap(i, best);
            i = best;
        }
    }

    for i in (0..heap.len() / 2).rev() {
        sift_down(&mut heap, i, &chunks, &cursors, terms);
    }

    let mut rows: Vec<Row> = Vec::new();
    while let Some(&top) = heap.first() {
        let (idx, _keys, row) = std::mem::take(&mut chunks[top].rows[cursors[top]]);
        rows.push(emit(idx, row));
        cursors[top] += 1;
        if cursors[top] >= chunks[top].rows.len() {
            let last = heap.len() - 1;
            heap.swap(0, last);
            heap.pop();
        }
        if !heap.is_empty() {
            sift_down(&mut heap, 0, &chunks, &cursors, terms);
        }
    }
    rows
}

/// One range worker's sorted chunk: (rowid, row) pairs in
/// `sort_total_cmp` order. Rows are in the caller's wanted-column
/// layout (full scan columns with the hidden-rowid trailing slot when
/// the scan shape emits one, or the fused projection's columns).
struct SortChunk {
    rows: Vec<(i64, Row)>,
}

// ============================================================================
// Parallel sort of MATERIALIZED rows — compound bodies, subqueries, joins
// ============================================================================

/// Compile one ORDER BY term against a MATERIALIZED input's output
/// columns (a UNION's combined list, a subquery's/CTE's output, a join's
/// combined row — anything `execute(input)` produced). Column references
/// resolve through the SAME `resolve_column_index` chain the serial
/// comparator's `eval_row` uses (qualified-exact -> exact -> suffix ->
/// case-insensitive), so the compiled key evaluates identically; ordinals
/// (positive integer literals) read the k-1 output column; the root
/// `COLLATE` wrapper is handled by the caller (the term list carries the
/// collation NAME — resolved on the main thread).
fn compile_materialized_expr(
    e: &Expr,
    columns: &[String],
    params_len: usize,
) -> Option<crate::executor::predicate::CompiledExpr> {
    use crate::executor::predicate::CompiledExpr;
    match e {
        Expr::Literal(v) => Some(CompiledExpr::Literal(v.clone())),
        // Ordinal: the k-th OUTPUT column (validated by exec_sort's
        // runtime range check before this compiler runs).
        Expr::Column { table, name } => {
            let idx = super::resolve_column_index(columns, table.as_deref(), name)?;
            Some(CompiledExpr::Col(idx))
        }
        // Inner Collate wrappers evaluate their inner expression (the
        // serial `evaluate` arm); collation only matters at comparison.
        Expr::Collate { expr, .. } => compile_materialized_expr(expr, columns, params_len),
        Expr::Parameter(name) => {
            if name == "?" || name.is_empty() {
                return Some(CompiledExpr::Param(0));
            }
            if let Ok(idx) = name.parse::<usize>() {
                if idx < params_len || params_len == 0 {
                    return Some(CompiledExpr::Param(idx));
                }
            }
            None
        }
        Expr::Unary { op, expr } => Some(CompiledExpr::Unary(
            *op,
            Box::new(compile_materialized_expr(expr, columns, params_len)?),
        )),
        Expr::Binary { op, left, right } => {
            if matches!(
                op,
                crate::sql::ast::BinaryOp::And | crate::sql::ast::BinaryOp::Or
            ) {
                return None;
            }
            Some(CompiledExpr::Binary(
                *op,
                Box::new(compile_materialized_expr(left, columns, params_len)?),
                Box::new(compile_materialized_expr(right, columns, params_len)?),
            ))
        }
        _ => None,
    }
}

/// Compile EVERY term of a materialized-input sort: (compiled key, DESC,
/// collation name). Returns None when any term is outside the compiled
/// set (user functions, CASE, ...) — the serial comparator's own
/// semantics stay in charge.
pub(crate) fn materialized_sort_terms(
    terms: &[crate::sql::ast::OrderTerm],
    columns: &[String],
    params_len: usize,
) -> Option<
    Vec<(
        crate::executor::predicate::CompiledExpr,
        bool,
        StdOption<String>,
    )>,
> {
    let mut out = Vec::with_capacity(terms.len());
    for term in terms {
        let (expr, collation): (&Expr, Option<String>) = match &term.expr {
            Expr::Collate { expr, collation } => (expr.as_ref(), Some(collation.clone())),
            e => (e, None),
        };
        // Ordinals first (the serial sort_key's special case).
        let compiled = match expr {
            Expr::Literal(Value::Integer(k)) if *k >= 1 => {
                let idx = (*k as usize).checked_sub(1)?;
                if idx >= columns.len() {
                    return None;
                }
                crate::executor::predicate::CompiledExpr::Col(idx)
            }
            e => compile_materialized_expr(e, columns, params_len)?,
        };
        out.push((
            compiled,
            term.order == crate::sql::ast::Order::Desc,
            collation,
        ));
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
}

/// Parallel sort of ALREADY-MATERIALIZED rows: the ORDER BY of a
/// compound SELECT (`... UNION ... ORDER BY x`), a subquery/CTE output,
/// or anything else whose sort input is not a bare table scan. The rows
/// split into contiguous index chunks; each worker materializes the
/// COMPILED key values once per row and sorts its chunk under the strict
/// (keys, global-index) total order; the main thread k-way merges under
/// the same order.
///
/// ## Why the merge is exactly the serial answer
///
/// The serial path runs a STABLE sort with a per-term comparator that
/// returns Equal on full ties, so tied rows keep input order: the serial
/// output is the (keys, input-index ASC) sequence. Chunks arrive in
/// input-index order; a local sort under the strict (keys, index) order
/// yields (keys, index ASC) within the chunk; the chunks tile the index
/// space in ascending order, so a k-way merge under the same strict
/// order emits the global (keys, index ASC) sequence — identical rows,
/// identical order.
///
/// Gates: row count >= the PRAGMA threshold; config on; no open
/// transaction; >= 2 worthwhile workers. `Ok(None)` = declined — the
/// caller runs the serial sort unchanged.
pub(crate) fn try_parallel_sort_rows(
    ctx: &ExecContext<'_>,
    rows: &mut Vec<Row>,
    terms: &[(
        crate::executor::predicate::CompiledExpr,
        bool,
        StdOption<String>,
    )],
) -> Result<Option<Vec<Row>>> {
    // Gate: config + threshold (the row count IS the estimate here — the
    // rows are materialized).
    let min_rows = ctx.pager.parallel_scan_min_rows();
    if min_rows <= 0 || (rows.len() as i64) < min_rows {
        return Ok(None);
    }
    // Gate: no open transaction (workers are foreign to the committed-
    // view TLS).
    if ctx.in_transaction {
        return Ok(None);
    }
    // Worker count: hardware threads, scaled to the work size.
    let hw = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let by_rows = (rows.len() / MIN_ROWS_PER_WORKER as usize).max(1);
    let n_workers = hw.min(by_rows).clamp(1, MAX_WORKERS);
    if n_workers < 2 {
        return Ok(None); // one worker = the serial path + spawn cost
    }

    // Resolve every term's collation ONCE on the MAIN thread (the
    // statement's plugin scope lives here).
    let resolved: Vec<ExprSortTerm> = terms
        .iter()
        .map(|(compiled, desc, name)| {
            (
                compiled.clone(),
                *desc,
                name.as_deref().and_then(crate::plugin::lookup_collation),
            )
        })
        .collect();

    // Split the rows into contiguous index chunks, MOVED out of the
    // input Vec (the merge reuses each row's allocation; no clones). A
    // decline below never touches the rows — the gates have all passed.
    let n = rows.len();
    let chunk = n.div_ceil(n_workers);
    let params: &[Value] = &ctx.params;
    let mut rest = std::mem::take(rows);
    let mut chunks: Vec<(usize, Vec<Row>)> = Vec::with_capacity(n_workers);
    let mut lo = 0usize;
    while !rest.is_empty() {
        let take = chunk.min(rest.len());
        let part: Vec<Row> = rest.drain(..take).collect();
        chunks.push((lo, part));
        lo += take;
    }
    let total = lo;

    // Workers: materialize the key values once per row (parallel key
    // evaluation — the serial comparator re-evaluates per comparison)
    // and sort the chunk under the strict (keys, global-index) order.
    // Pure compute — no I/O, no fallible step.
    let conn_enc_tag = super::conn_enc::current();
    let partials: Vec<ExprSortChunk> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(chunks.len());
        for (base, part) in chunks {
            let resolved = &resolved;
            handles.push(scope.spawn(move || -> ExprSortChunk {
                let _enc_guard = super::conn_enc::reinstall(conn_enc_tag);
                let mut rows: Vec<(i64, Vec<Value>, Row)> = Vec::with_capacity(part.len());
                for (i, row) in part.into_iter().enumerate() {
                    let mut keys = Vec::with_capacity(resolved.len());
                    for (c, _, _) in resolved {
                        keys.push(c.eval(&row, params));
                    }
                    rows.push(((base + i) as i64, keys, row));
                }
                rows.sort_unstable_by(|a, b| expr_sort_cmp(&a.1, a.0, &b.1, b.0, resolved));
                ExprSortChunk { rows }
            }));
        }
        handles
            .into_iter()
            .map(|h| h.join().expect("parallel row-sort worker panicked"))
            .collect()
    });
    reclaim_worker_heaps();

    let chunks = partials;
    if chunks.is_empty() {
        return Ok(Some(Vec::new()));
    }

    // k-way merge under the same strict total order; rows emit as-is
    // (they ARE the output — the caller applies no projection).
    let rows = expr_chunks_merge(chunks, &resolved, &|_idx, row| row);
    debug_assert_eq!(rows.len(), total);
    Ok(Some(rows))
}

/// Parallel unbounded `ORDER BY a, b DESC, ...` over a bare full table
/// scan (no predicate, no index, not a vtab): the rowid space splits
/// across workers exactly like the aggregate paths; each worker decodes
/// its range with the SAME selective scanner the parallel top-N uses,
/// sorts its chunk locally under the (keys, rowid) total order, and the
/// main thread runs a k-way merge under the SAME order.
///
/// `project == None` decodes every table column (the Sort's own output
/// shape, hidden-rowid slot appended when the scan emits one).
/// `project == Some(indices)` fuses a 1:1 bare-column projection ABOVE
/// the sort (the `Project(Sort(Scan))` shape): the worker decodes only
/// the projected + key columns — the serial path decodes every column,
/// sorts, and drops the unprojected values on the floor, so the fused
/// decode skips exactly the work the projection would discard (wide
/// TEXT columns a `SELECT id, val ... ORDER BY val` never needed).
///
/// ## Why the merge is exactly the serial answer
///
/// The serial path materializes the scan (rowid ASC — the B+tree walk
/// order) and runs a STABLE sort with a per-term comparator that returns
/// Equal on full ties, so tied rows keep scan order: the serial output
/// is the (keys, rowid ASC) sequence. Each worker's chunk arrives in
/// rowid ASC order (its range), and a local sort under the strict total
/// order yields (keys, rowid ASC) within the range. The ranges tile the
/// rowid space in ascending order with no overlap, so a k-way merge
/// under the same strict total order emits the global (keys, rowid ASC)
/// sequence — identical rows, identical order, no float latitude and no
/// order-of-operations freedom. Corrupt rows are skipped by BOTH
/// decoders (the selective scanner mirrors the inline scan's error
/// tolerance), short rows NULL-pad in both.
///
/// `Ok(None)` = declined (shape below threshold / config off / txn
/// open / a worker error) — the caller runs the serial path unchanged.
pub(crate) fn try_parallel_sort(
    ctx: &ExecContext<'_>,
    table: &StdArc<Table>,
    keys: &[(usize, bool, StdOption<String>)],
    project: Option<&[usize]>,
    append_rowid: bool,
    out_cols: StdArc<[String]>,
) -> Result<Option<ExecResult>> {
    let root = ctx.table_root(table);
    let split = match plan_range_split(ctx, root)? {
        Some(s) => s,
        None => return Ok(None),
    };

    // Resolve every term's collation ONCE, HERE — the main thread, where
    // the statement's plugin scope is live (custom collations are
    // thread-local; a worker-side lookup silently misses them — see
    // sort_total_cmp's doc). None = the serial comparator's fallback.
    let resolved: Vec<SortKey> = keys
        .iter()
        .map(|&(col, desc, ref name)| {
            (
                col,
                desc,
                name.as_deref().and_then(crate::plugin::lookup_collation),
            )
        })
        .collect();

    let n_cols = table.n_columns();
    let rowid_alias = table.rowid_alias;
    let rowid_proj = crate::storage::row_codec::ROWID_PROJ;

    // ---- Wanted-column + position mapping --------------------------------
    // Full-row mode (project None): the decoder reads every table
    // column (identity positions) and the callback appends the hidden
    // rowid slot when the scan shape emits one, so a hidden-slot key
    // (index == n_cols) addresses the appended value directly.
    // Projection mode: wanted = projected columns + key columns (the
    // projection is 1:1, so sorting the projected row under the same
    // total order equals sorting the full row); a hidden-slot key maps
    // to the ROWID_PROJ sentinel, which the selective decoder fills
    // from the B+tree cell key.
    let scan_wanted: Vec<usize> = match project {
        Some(ps) => {
            let mut w = ps.to_vec();
            for &(k, ..) in keys {
                let k_eff = if k >= n_cols { rowid_proj } else { k };
                if !w.contains(&k_eff) {
                    w.push(k_eff);
                }
            }
            w.sort_unstable();
            w.dedup();
            w
        }
        None => (0..n_cols).collect(),
    };
    // Key position in the WORKER row (post-append in full-row mode).
    let row_keys: Vec<SortKey> = match project {
        Some(_) => resolved
            .iter()
            .map(|&(k, d, ref c)| {
                let k_eff = if k >= n_cols { rowid_proj } else { k };
                (
                    scan_wanted
                        .iter()
                        .position(|&w| w == k_eff)
                        .expect("key column in wanted"),
                    d,
                    c.clone(),
                )
            })
            .collect(),
        None => resolved,
    };
    // Output position mapping (projection mode only).
    let project_map: Vec<usize> = match project {
        Some(ps) => ps
            .iter()
            .map(|&p| {
                scan_wanted
                    .iter()
                    .position(|&w| w == p)
                    .expect("projected column in wanted")
            })
            .collect(),
        None => Vec::new(),
    };

    let ranges = split.ranges;
    let pager = ctx.pager;
    let append = append_rowid && project.is_none();

    // Workers: range scan + local sort. The rows move out of the chunk
    // during the merge (mem::take), so the chunk Vecs are drained
    // allocation-free on the emit side.
    let conn_enc_tag = super::conn_enc::current();
    let partials: Vec<Result<SortChunk>> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(ranges.len());
        for &(lo, hi) in ranges.iter() {
            let wanted = &scan_wanted;
            let row_keys = &row_keys;
            handles.push(scope.spawn(move || -> Result<SortChunk> {
                let _enc_guard = super::conn_enc::reinstall(conn_enc_tag);
                let mut bt = Btree::new(pager, root, false);
                let mut rows: Vec<(i64, Row)> = Vec::new();
                bt.scan_table_range_selective(
                    lo,
                    hi,
                    n_cols,
                    wanted,
                    rowid_alias,
                    |rowid, mut row| {
                        if append {
                            row.push(Value::Integer(rowid));
                        }
                        rows.push((rowid, row));
                        true
                    },
                )?;
                rows.sort_unstable_by(|a, b| sort_total_cmp(&a.1, a.0, &b.1, b.0, row_keys));
                Ok(SortChunk { rows })
            }));
        }
        handles
            .into_iter()
            .map(|h| h.join().expect("parallel sort worker panicked"))
            .collect()
    });
    reclaim_worker_heaps();

    // Any worker error -> serial fallback (never a divergent answer).
    let mut chunks: Vec<SortChunk> = Vec::with_capacity(partials.len());
    let mut total: usize = 0;
    for p in partials {
        match p {
            Ok(c) => {
                total += c.rows.len();
                chunks.push(c);
            }
            Err(_) => return Ok(None),
        }
    }
    if chunks.is_empty() {
        return Ok(None);
    }

    // ---- k-way merge under the same strict total order ----------------
    // A binary heap of chunk indices; chunk i's head is
    // chunks[i].rows[cursors[i]]. Rows MOVE out of their chunk via
    // mem::take (the output reuses each row's allocation).
    let mut cursors: Vec<usize> = vec![0; chunks.len()];
    let mut heap: Vec<usize> = (0..chunks.len())
        .filter(|&c| !chunks[c].rows.is_empty())
        .collect();

    // Sift-down over the chunk-index heap; the comparator reads the
    // current heads through `chunks`/`cursors` (immutable during the
    // sift — mutations happen between sifts).
    fn sift_down(
        heap: &mut [usize],
        mut i: usize,
        chunks: &[SortChunk],
        cursors: &[usize],
        row_keys: &[SortKey],
    ) {
        loop {
            let n = heap.len();
            let l = 2 * i + 1;
            let r = l + 1;
            let mut m = i;
            let lt = |a: usize, b: usize| -> bool {
                let (pa, pb) = (cursors[a], cursors[b]);
                let (ra, rb) = (&chunks[a].rows[pa].1, &chunks[b].rows[pb].1);
                sort_total_cmp(ra, chunks[a].rows[pa].0, rb, chunks[b].rows[pb].0, row_keys)
                    == std::cmp::Ordering::Less
            };
            if l < n && lt(heap[l], heap[m]) {
                m = l;
            }
            if r < n && lt(heap[r], heap[m]) {
                m = r;
            }
            if m == i {
                break;
            }
            heap.swap(i, m);
            i = m;
        }
    }
    // Heapify (Floyd).
    for i in (0..heap.len() / 2).rev() {
        sift_down(&mut heap, i, &chunks, &cursors, &row_keys);
    }

    let mut out: Vec<Row> = Vec::with_capacity(total);
    while let Some(&c) = heap.first() {
        let pos = cursors[c];
        cursors[c] += 1;
        let (_, mut row) = std::mem::take(&mut chunks[c].rows[pos]);
        if project.is_some() {
            let projected: Vec<Value> = project_map
                .iter()
                .map(|&p| std::mem::replace(&mut row[p], Value::Null))
                .collect();
            out.push(projected);
        } else {
            out.push(row);
        }
        if cursors[c] >= chunks[c].rows.len() {
            // Chunk exhausted: remove the root (swap-with-last + pop).
            let last = heap.pop().unwrap();
            if !heap.is_empty() {
                heap[0] = last;
                sift_down(&mut heap, 0, &chunks, &cursors, &row_keys);
            }
        } else {
            sift_down(&mut heap, 0, &chunks, &cursors, &row_keys);
        }
    }

    Ok(Some(ExecResult {
        columns: out_cols,
        rows: out,
    }))
}

// ---------------------------------------------------------------------------
// Parallel JOIN probe (the multi-table parallelism frontier)
// ---------------------------------------------------------------------------

/// One output-column source for the parallel probe: (from_build, pos).
/// Mirrors the fused path's `OutSlot`.
#[derive(Clone, Copy)]
struct ProbeOutSlot {
    from_build: bool,
    pos: usize,
}

/// Fibonacci-hash slot for a u64 join key (same constants the build side
/// uses — the two must agree or probes miss).
#[inline]
fn join_key_slot(k: u64, shift: u32, mask: usize) -> usize {
    ((k.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> shift) as usize) & mask
}

/// Parallel PROBE side of the fused scan-hash join
/// (`try_fused_scan_hash_join`): the build state (`built`) is complete
/// and read-only by now, so the probe table's rowid space can split
/// across workers exactly like a single-table parallel aggregate — each
/// worker selective-decodes its range, probes the shared open-addressing
/// table, and emits joined rows into its own buffer. Partials
/// concatenate in RANGE order, which is the serial probe scan's row
/// order: identical output, bit for bit.
///
/// Gates beyond [`plan_range_split`]: no committed-view scope armed on
/// this thread (workers would read LIVE pages inside a foreign write
/// transaction — the serial path's BEGIN-time snapshot semantics would
/// be lost), and the fused path has already declined TEXT/BLOB build
/// keys, so worker probe keys are numeric-only by construction (the
/// worker re-checks per row anyway).
///
/// Returns `None` when any gate fails or a worker errors — the caller's
/// serial probe walk runs unchanged.
pub(crate) fn try_parallel_join_probe(
    ctx: &ExecContext<'_>,
    probe_root: u32,
    probe_n_cols: usize,
    probe_wanted: &[usize],
    probe_alias: Option<usize>,
    probe_key_pos: &[usize],
    built: &StdArc<crate::storage::join_cache::JoinBuildState>,
    out_slots: &[(bool, usize)],
    n_out: usize,
    left_outer: bool,
) -> Result<Option<Vec<crate::Row>>> {
    let pager = ctx.pager;
    // Committed-view scope: workers are foreign to the TLS arming.
    if pager.committed_reads_armed() {
        return Ok(None);
    }
    let Some(split) = plan_range_split(ctx, probe_root)? else {
        return Ok(None);
    };
    let ranges = split.ranges;
    let slots: &[crate::storage::join_cache::JoinSlot] = &built.slots;
    let table_cap = slots.len().max(2).next_power_of_two();
    debug_assert_eq!(table_cap, slots.len());
    let table_mask = table_cap.wrapping_sub(1);
    let hash_shift = 64 - table_cap.trailing_zeros();

    let built = StdArc::clone(built);
    let wanted: StdArc<Vec<usize>> = StdArc::new(probe_wanted.to_vec());
    let out_slots: StdArc<Vec<ProbeOutSlot>> = StdArc::new(
        out_slots
            .iter()
            .map(|&(from_build, pos)| ProbeOutSlot { from_build, pos })
            .collect(),
    );
    let probe_key_pos: StdArc<Vec<usize>> = StdArc::new(probe_key_pos.to_vec());
    let n_keys = built.n_keys;
    let built_key_slots: StdArc<Vec<usize>> = StdArc::new(built.key_slots.clone());
    let n_workers = ranges.len();
    let conn_enc_tag = super::conn_enc::current();
    let partials: Vec<Result<Vec<crate::Row>>> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(n_workers);
        for &(lo, hi) in ranges.iter() {
            let built = StdArc::clone(&built);
            let wanted = StdArc::clone(&wanted);
            let out_slots = StdArc::clone(&out_slots);
            let probe_key_pos = StdArc::clone(&probe_key_pos);
            let built_key_slots = StdArc::clone(&built_key_slots);
            handles.push(scope.spawn(move || -> Result<Vec<crate::Row>> {
                let _enc_guard = super::conn_enc::reinstall(conn_enc_tag);
                let mut out_rows: Vec<crate::Row> = Vec::new();
                let mut pbuf: Vec<crate::types::Value> = Vec::new();
                let mut pks: Vec<u64> = vec![0u64; n_keys];
                let mut match_ords: Vec<u32> = Vec::new();
                let mut bt = Btree::new(pager, probe_root, false);
                bt.scan_table_range_borrowed(lo, hi, |rowid, payload| {
                    if crate::storage::row_codec::decode_row_selective_sorted(
                        payload,
                        probe_n_cols,
                        &wanted,
                        rowid,
                        probe_alias,
                        &mut pbuf,
                    )
                    .is_err()
                    {
                        return true; // corrupt row: skip (serial parity)
                    }
                    let mut key_ok = true;
                    for (j, &kp) in probe_key_pos.iter().enumerate() {
                        let k = match pbuf.get(kp) {
                            Some(crate::types::Value::Integer(i)) => {
                                crate::types::value::double_order_key(*i as f64)
                            }
                            Some(crate::types::Value::Real(f)) => {
                                crate::types::value::double_order_key(*f)
                            }
                            // NULL/TEXT/BLOB: no match (LEFT still emits
                            // the NULL-extended probe row).
                            _ => {
                                key_ok = false;
                                break;
                            }
                        };
                        pks[j] = k;
                    }
                    let mut next = u32::MAX;
                    if key_ok {
                        let k = if n_keys == 1 {
                            pks[0]
                        } else {
                            super::fold_join_hash(&pks)
                        };
                        next = {
                            let mut slot = join_key_slot(k, hash_shift, table_mask);
                            loop {
                                let existing = slots[slot].key;
                                if existing == u64::MAX {
                                    break u32::MAX;
                                }
                                if existing == k
                                    && (n_keys == 1
                                        || super::join_keys_match(
                                            &built.build_vals,
                                            slots[slot].head as usize,
                                            built.stride,
                                            &built_key_slots,
                                            &pbuf,
                                            &probe_key_pos,
                                        ))
                                {
                                    break slots[slot].head;
                                }
                                slot = (slot + 1) & table_mask;
                            }
                        };
                    }
                    let stride = built.stride;
                    if !left_outer {
                        // INNER: matches in chain order (the serial fused
                        // path's discipline — range-ordered partials stay
                        // bit-identical to it).
                        while next != u32::MAX {
                            let ord = next as usize;
                            let mut out: Vec<crate::types::Value> = Vec::with_capacity(n_out);
                            for s in out_slots.iter() {
                                let v = if s.from_build {
                                    &built.build_vals[ord * stride + s.pos]
                                } else {
                                    &pbuf[s.pos]
                                };
                                out.push(v.clone());
                            }
                            out_rows.push(out);
                            next = built.chain[ord];
                        }
                    } else {
                        // LEFT: collect the chain, REVERSE to build-scan
                        // order, emit — or one NULL-extended row.
                        match_ords.clear();
                        while next != u32::MAX {
                            match_ords.push(next);
                            next = built.chain[next as usize];
                        }
                        if match_ords.is_empty() {
                            let mut out: Vec<crate::types::Value> = Vec::with_capacity(n_out);
                            for s in out_slots.iter() {
                                out.push(if s.from_build {
                                    crate::types::Value::Null
                                } else {
                                    pbuf[s.pos].clone()
                                });
                            }
                            out_rows.push(out);
                        } else {
                            match_ords.reverse();
                            for &ord in match_ords.iter() {
                                let ord = ord as usize;
                                let mut out: Vec<crate::types::Value> = Vec::with_capacity(n_out);
                                for s in out_slots.iter() {
                                    let v = if s.from_build {
                                        &built.build_vals[ord * stride + s.pos]
                                    } else {
                                        &pbuf[s.pos]
                                    };
                                    out.push(v.clone());
                                }
                                out_rows.push(out);
                            }
                        }
                    }
                    true
                })?;
                Ok(out_rows)
            }));
        }
        handles
            .into_iter()
            .map(|h| h.join().expect("parallel join-probe worker panicked"))
            .collect()
    });
    reclaim_worker_heaps();
    let mut rows: Vec<crate::Row> = Vec::new();
    for p in partials {
        match p {
            Ok(mut part) => rows.append(&mut part),
            Err(_) => return Ok(None), // worker error: serial fallback
        }
    }
    Ok(Some(rows))
}
