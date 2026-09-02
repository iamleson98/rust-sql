#!/usr/bin/env python3
"""bench_gate.py — CI gate: rustqlite must beat (or tie) SQLite on every row.

Runs one of the project's head-to-head benchmark harnesses, parses its
output, re-runs noisy rows up to N times (best-per-engine across attempts —
the same rule for both engines, mirroring the benches' own best-of-N
sampling), and fails the build if rustqlite is slower than SQLite on any
row by more than the tolerance.

Supported parsers (the canonical head-to-head harnesses of this repo):
  bench_compare              examples/bench_compare.rs      (time columns)
  bench_full_vs_sqlite       examples/bench_full_vs_sqlite.rs (ops/s columns)
  bench_sqlx_native          examples/bench_sqlx_native.rs  (ms columns, sqlx)
  criterion_sqlite_comparison benches/sqlite_comparison.rs  (criterion time lines)

Verdict semantics (matching bench_full_vs_sqlite's own "tie" band):
  loss_pct = how much slower rustqlite is than SQLite (higher = worse)
  PASS  loss_pct <= tolerance (wins and ties pass)
  FAIL  loss_pct >  tolerance (a real throughput loss)

Default tolerance is 5% — the same band the project's own bench summary
treats as a tie. Set BENCH_TOLERANCE_PCT / --tolerance-pct to 0 for a
strict every-row-must-win gate (not recommended on shared CI runners).

Exit codes: 0 = pass, 1 = gate failure (rows lost to SQLite), 2 = harness
error (bench could not run or output could not be parsed — also fails CI,
so a format drift can never make the gate vacuously green).
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path
from typing import Callable, Dict, List, Optional, Sequence, Tuple

# ---------------------------------------------------------------------------
# Duration / number parsing
# ---------------------------------------------------------------------------

_UNIT_SECONDS = {"s": 1.0, "ms": 1e-3, "µs": 1e-6, "us": 1e-6, "ns": 1e-9}

# "1.23s" / "4.56ms" / "7.89µs" / "12ns"  (optionally "807.94 µs" with a space)
_DUR = r"(\d+(?:\.\d+)?)\s*(µs|us|ms|ns|s)"
_DUR_RE = re.compile(rf"^{_DUR}$")


def parse_duration(text: str) -> Optional[float]:
    """Parse a duration string into seconds (None if not a duration)."""
    m = _DUR_RE.match(text.strip())
    if not m:
        return None
    value, unit = float(m.group(1)), m.group(2)
    return value * _UNIT_SECONDS[unit]


# ---------------------------------------------------------------------------
# Row model
# ---------------------------------------------------------------------------

class Row:
    """One head-to-head comparison row.

    metric=="time":  lower is better; best = min across attempts.
    metric=="ops":   higher is better; best = max across attempts.
    """

    def __init__(self, name: str, metric: str, unit_hint: str = ""):
        self.name = name
        self.metric = metric
        self.unit_hint = unit_hint
        self.rust: List[float] = []
        self.sql: List[float] = []

    @property
    def best_rust(self) -> Optional[float]:
        if not self.rust:
            return None
        return min(self.rust) if self.metric == "time" else max(self.rust)

    @property
    def best_sql(self) -> Optional[float]:
        if not self.sql:
            return None
        return min(self.sql) if self.metric == "time" else max(self.sql)

    @property
    def ratio(self) -> Optional[float]:
        """>1 means rustqlite is faster."""
        r, s = self.best_rust, self.best_sql
        if r is None or s is None or r == 0 or s == 0:
            return None
        if self.metric == "time":
            return s / r
        return r / s

    @property
    def loss_pct(self) -> Optional[float]:
        """How much slower rustqlite is than SQLite, in percent (negative = faster)."""
        r, s = self.best_rust, self.best_sql
        if r is None or s is None or s == 0:
            return None
        if self.metric == "time":
            return (r - s) / s * 100.0
        return (s - r) / s * 100.0


# ---------------------------------------------------------------------------
# Parsers: stdout text -> {row_name: Row}
# ---------------------------------------------------------------------------

# bench_compare rows: "<name padded to 50>  <rust time>  <sqlite time>"
# Resource-metric rows (KB/MB/B values) intentionally do not match the
# duration pattern and are skipped automatically.
_BENCH_COMPARE_RE = re.compile(
    rf"^(?P<name>.*?)\s+(?P<rust>{_DUR})\s+(?P<sql>{_DUR})\s*$"
)


def _parse_bench_compare(text: str) -> Dict[str, Row]:
    rows: Dict[str, Row] = {}
    for line in text.splitlines():
        line = line.rstrip()
        m = _BENCH_COMPARE_RE.match(line)
        if not m:
            continue
        name = m.group("name").strip()
        rust = parse_duration(m.group("rust"))
        sql = parse_duration(m.group("sql"))
        if not name or rust is None or sql is None:
            continue
        rows[name] = Row(name, "time")
        rows[name].rust.append(rust)
        rows[name].sql.append(sql)
    return rows


# bench_full_vs_sqlite rows:
#   "  <name> | rust-sql:  123456 ops/s | SQLite:  123000 ops/s | ratio: 1.00x (rust-sql)"
_BENCH_FULL_RE = re.compile(
    r"^\s*(?P<name>[^|]+?)\s*\|\s*rust-sql:\s*(?P<rust>[\d.]+)\s*ops/s"
    r"\s*\|\s*SQLite:\s*(?P<sql>[\d.]+)\s*ops/s"
)


def _parse_bench_full_vs_sqlite(text: str) -> Dict[str, Row]:
    rows: Dict[str, Row] = {}
    for line in text.splitlines():
        m = _BENCH_FULL_RE.match(line)
        if not m:
            continue
        name = m.group("name").strip()
        if not name:
            continue
        row = Row(name, "ops", "ops/s")
        row.rust.append(float(m.group("rust")))
        row.sql.append(float(m.group("sql")))
        rows[name] = row
    return rows


# bench_sqlx_native rows: "<scenario>  123.4 ms  456.7 ms  1.23x"
# (seed lines and headers do not match the trailing "ms ms x" shape)
_BENCH_SQLX_RE = re.compile(
    r"^(?P<name>.*?)\s+(?P<rust>\d+(?:\.\d+)?)\s*ms"
    r"\s+(?P<sql>\d+(?:\.\d+)?)\s*ms"
    r"\s+(?P<ratio>\d+(?:\.\d+)?)x\s*$"
)


def _parse_bench_sqlx_native(text: str) -> Dict[str, Row]:
    rows: Dict[str, Row] = {}
    for line in text.splitlines():
        line = line.rstrip()
        m = _BENCH_SQLX_RE.match(line)
        if not m:
            continue
        name = m.group("name").strip()
        if not name:
            continue
        row = Row(name, "time", "ms")
        row.rust.append(float(m.group("rust")) / 1e3)
        row.sql.append(float(m.group("sql")) / 1e3)
        rows[name] = row
    return rows


# criterion (benches/sqlite_comparison.rs) emits, per benchmark:
#     Benchmarking insert/rustqlite_autocommit: Analyzing
#     insert/rustqlite_autocommit
#                         time:   [273.77 µs 292.92 µs 312.75 µs]
#                         thrpt:  [3.1975 Kelem/s ...]
# or, when the id is short enough, id and time on ONE line:
#     point_lookup/rustqlite  time:   [180.65 ns 181.03 ns 181.48 ns]
# The middle value of [lower, estimate, upper] is the point estimate.
# The value line ("time:") may follow a bare id line, so the parser carries a
# pending id across lines. Only "time:" lines are used (thrpt/change skipped).
_CRITERION_TIME_RE = re.compile(
    r"time:\s+\["
    r"\s*(?:\d+(?:\.\d+)?)\s*(?P<ua>µs|us|ms|ns|s)"
    r"\s+(?P<b>\d+(?:\.\d+)?)\s*(?P<ub>µs|us|ms|ns|s)"
    r"\s+(?:\d+(?:\.\d+)?)\s*(?:µs|us|ms|ns|s)"
    r"\s*\]"
)
# "<group>/<bench>  time:" all on one line (id at line start, no indent).
_CRITERION_INLINE_ID_RE = re.compile(r"^(?P<id>[A-Za-z0-9_./-]+)\s+time:")
# Bare id line: the whole line is "<group>/<bench>" (no spaces, no indent).
_CRITERION_BARE_ID_RE = re.compile(r"^(?P<id>[A-Za-z0-9_./-]+)/[A-Za-z0-9_./-]+$")


def _criterion_times(text: str) -> Dict[str, float]:
    """Extract {bench_id: point-estimate seconds} from criterion stdout."""
    times: Dict[str, float] = {}
    pending_id: Optional[str] = None
    for raw in text.splitlines():
        line = raw.rstrip()
        m = _CRITERION_INLINE_ID_RE.match(line)
        if m:
            pending_id = m.group("id")
        elif _CRITERION_BARE_ID_RE.match(line):
            pending_id = line
        tm = _CRITERION_TIME_RE.search(line)
        if tm and pending_id and "Benchmarking" not in line:
            times[pending_id] = float(tm.group("b")) * _UNIT_SECONDS[tm.group("ub")]
            pending_id = None
    return times


def _common_prefix_len(a: str, b: str) -> int:
    n = 0
    for x, y in zip(a, b):
        if x != y:
            break
        n += 1
    return n


# Engine leaf prefixes. NOTE: "rustqlite" and "rusqlite" differ at index 3
# (t vs q), so the startswith() tests below are mutually exclusive.
_RUST_PREFIX = "rustqlite"
_SQL_PREFIX = "rusqlite"


def _pair_criterion(ids: Sequence[str]) -> List[Tuple[str, str]]:
    """Pair each rustqlite_* benchmark with its rusqlite_* (real SQLite)
    counterpart, by group and engine-stripped remainder.

    Within a group, leafs like rustqlite_8_readers pair with
    rusqlite_8_readers_mutex (SQLite side carries extra mode suffixes), so
    single/single groups pair directly and multi-pair groups match on the
    longest common prefix of the engine-stripped remainders. New benches
    added to sqlite_comparison.rs are picked up automatically — nothing is
    hardcoded, so the gate can never silently skip a scenario.
    """
    groups: Dict[str, Dict[str, List[str]]] = {}
    for bench_id in ids:
        group, _, leaf = bench_id.partition("/")
        side = None
        if leaf.startswith(_RUST_PREFIX):
            side = "rust"
        elif leaf.startswith(_SQL_PREFIX):
            side = "sql"
        if side:
            groups.setdefault(group, {"rust": [], "sql": []})[side].append(leaf)
    pairs: List[Tuple[str, str]] = []
    for group in sorted(groups):
        rust, sql = groups[group]["rust"], groups[group]["sql"]
        if len(rust) == 1 and len(sql) == 1:
            pairs.append((f"{group}/{rust[0]}", f"{group}/{sql[0]}"))
            continue
        used: set = set()
        for r in rust:
            r_rem = r[len(_RUST_PREFIX):].lstrip("_")
            best, best_score = None, -1
            for s in sql:
                if s in used:
                    continue
                s_rem = s[len(_SQL_PREFIX):].lstrip("_")
                score = _common_prefix_len(r_rem, s_rem)
                if score > best_score:
                    best, best_score = s, score
            if best is not None:
                used.add(best)
                pairs.append((f"{group}/{r}", f"{group}/{best}"))
    return pairs


def _criterion_display_name(rust_id: str) -> str:
    group, _, leaf = rust_id.partition("/")
    rem = leaf[len(_RUST_PREFIX):].lstrip("_")
    return f"{group}/{rem}" if rem else group


def _parse_criterion(text: str) -> Dict[str, Row]:
    times = _criterion_times(text)
    rows: Dict[str, Row] = {}
    for rust_id, sql_id in _pair_criterion(list(times)):
        name = _criterion_display_name(rust_id)
        row = Row(name, "time")
        row.rust.append(times[rust_id])
        row.sql.append(times[sql_id])
        rows[name] = row
    return rows


PARSERS: Dict[str, Tuple[Callable[[str], Dict[str, Row]], int]] = {
    "bench_compare": (_parse_bench_compare, 18),
    "bench_full_vs_sqlite": (_parse_bench_full_vs_sqlite, 15),
    "bench_sqlx_native": (_parse_bench_sqlx_native, 10),
    "criterion_sqlite_comparison": (_parse_criterion, 8),
}

DEFAULT_ATTEMPTS = {
    "bench_compare": 3,
    "bench_full_vs_sqlite": 3,
    "bench_sqlx_native": 3,
    "criterion_sqlite_comparison": 2,
}


# ---------------------------------------------------------------------------
# Formatting helpers
# ---------------------------------------------------------------------------


def fmt_time(seconds: float) -> str:
    if seconds >= 1.0:
        return f"{seconds:.2f}s"
    if seconds >= 1e-3:
        return f"{seconds * 1e3:.2f}ms"
    if seconds >= 1e-6:
        return f"{seconds * 1e6:.2f}µs"
    return f"{seconds * 1e9:.0f}ns"


def fmt_ops(ops: float) -> str:
    return f"{ops:,.0f} ops/s"


def fmt_row_value(row: Row, value: float) -> str:
    if row.metric == "ops":
        return fmt_ops(value)
    return fmt_time(value)


def verdict(row: Row, tolerance_pct: float) -> str:
    loss = row.loss_pct
    if loss is None:
        return "NO DATA"
    if loss > tolerance_pct:
        return "LOSS"
    if loss > 0:
        return "TIE"
    return "WIN"


# ---------------------------------------------------------------------------
# Gate runner
# ---------------------------------------------------------------------------


def run_command(
    cmd: Sequence[str], timeout: float, cwd: Path
) -> Tuple[int, str, float]:
    """Run a benchmark command, return (exit_code, combined_output, seconds)."""
    resolved = list(cmd)
    if not os.path.isabs(resolved[0]) and not shutil.which(resolved[0]):
        # cargo/cargo.exe resolution quirk guard
        raise FileNotFoundError(
            f"executable not found on PATH: {resolved[0]!r}"
        )
    start = time.monotonic()
    try:
        proc = subprocess.run(
            resolved,
            cwd=str(cwd),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=timeout,
        )
        elapsed = time.monotonic() - start
        return proc.returncode, proc.stdout.decode("utf-8", errors="replace"), elapsed
    except subprocess.TimeoutExpired as exc:
        elapsed = time.monotonic() - start
        out = b""
        if exc.stdout:
            out = exc.stdout if isinstance(exc.stdout, bytes) else exc.stdout.encode()
        return 124, out.decode("utf-8", errors="replace") + "\n[TIMED OUT]", elapsed


def merge_rows(target: Dict[str, Row], new_rows: Dict[str, Row]) -> None:
    for name, row in new_rows.items():
        if name not in target:
            target[name] = Row(row.name, row.metric, row.unit_hint)
        target[name].rust.extend(row.rust)
        target[name].sql.extend(row.sql)


def tail(text: str, n: int = 60) -> str:
    lines = text.splitlines()
    return "\n".join(lines[-n:])


def main(argv: Optional[Sequence[str]] = None) -> int:
    ap = argparse.ArgumentParser(
        description="CI gate: every head-to-head bench row must beat (or tie) SQLite."
    )
    ap.add_argument("--parser", required=True, choices=sorted(PARSERS))
    ap.add_argument(
        "--attempts",
        type=int,
        default=0,
        help="max bench invocations (default: 3 for example benches, 2 for criterion; "
        "stops early once every row passes)",
    )
    ap.add_argument(
        "--tolerance-pct",
        type=float,
        default=float(os.environ.get("BENCH_TOLERANCE_PCT", "5.0")),
        help="max regression vs SQLite allowed before the row counts as a loss "
        "(default 5, the project's own tie band; 0 = strict win-only)",
    )
    ap.add_argument("--timeout", type=float, default=1800.0, help="per-attempt seconds")
    ap.add_argument(
        "--results-dir",
        default="target/bench-results",
        help="where JSON results and per-attempt logs are written",
    )
    ap.add_argument(
        "--summary",
        default=os.environ.get("GITHUB_STEP_SUMMARY", ""),
        help="GitHub step-summary markdown file to append to",
    )
    ap.add_argument(
        "--min-rows",
        type=int,
        default=0,
        help="minimum parsed rows required (default: per-parser safe floor)",
    )
    ap.add_argument("cmd", nargs=argparse.REMAINDER, help="bench command after --")

    args = ap.parse_args(argv)
    if args.cmd and args.cmd[0] == "--":
        args.cmd = args.cmd[1:]
    if not args.cmd:
        ap.error("no bench command given (pass it after `--`)")

    parser_fn, min_rows_default = PARSERS[args.parser]
    min_rows = args.min_rows or min_rows_default
    attempts = args.attempts or DEFAULT_ATTEMPTS.get(args.parser, 3)
    tolerance = args.tolerance_pct

    repo_root = Path(__file__).resolve().parents[2]
    results_dir = Path(args.results_dir)
    if not results_dir.is_absolute():
        results_dir = repo_root / results_dir
    results_dir.mkdir(parents=True, exist_ok=True)

    # stdout must survive µ / em-dash on a cp1252 Windows console
    try:
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass

    print(f"[bench-gate:{args.parser}] command: {' '.join(args.cmd)}")
    print(
        f"[bench-gate:{args.parser}] attempts<={attempts} "
        f"tolerance={tolerance:g}% timeout={args.timeout:g}s min_rows={min_rows}"
    )

    merged: Dict[str, Row] = {}
    attempts_run = 0
    run_errors: List[str] = []
    all_pass = False

    for attempt in range(1, attempts + 1):
        attempts_run = attempt
        code, output, elapsed = run_command(args.cmd, args.timeout, repo_root)
        log_path = results_dir / f"{args.parser}-attempt{attempt}.log"
        log_path.write_text(output, encoding="utf-8", errors="replace")
        print(
            f"[bench-gate:{args.parser}] attempt {attempt}/{attempts}: "
            f"exit={code} ({elapsed:.0f}s), log: {log_path}"
        )
        if code != 0:
            run_errors.append(f"attempt {attempt} exited {code}")
            print(tail(output, 25))
        rows = parser_fn(output)
        if rows:
            merge_rows(merged, rows)
        else:
            run_errors.append(f"attempt {attempt} produced 0 parseable rows")
        all_pass = bool(merged) and all(
            verdict(r, tolerance) != "LOSS" for r in merged.values()
        )
        if all_pass:
            break  # green fast-path: no need to spend more runner minutes

    # ---------------------------------------------------------------- verdict
    failed = [r for r in merged.values() if verdict(r, tolerance) == "LOSS"]
    if len(merged) < min_rows:
        print(
            f"[bench-gate:{args.parser}] ERROR: parsed {len(merged)} rows, "
            f"need >= {min_rows} — output format drift? (gate refuses to pass vacuously)"
        )
        return 2
    if not merged:
        print(f"[bench-gate:{args.parser}] ERROR: no rows parsed at all")
        for err in run_errors:
            print(f"  - {err}")
        return 2

    status_order = {"LOSS": 0, "TIE": 1, "WIN": 2}
    ordered = sorted(
        merged.values(), key=lambda r: (status_order[verdict(r, tolerance)], r.name)
    )

    print()
    header = (
        f"{'row':<44} {'rustqlite':>14} {'SQLite':>14} {'ratio':>8} {'loss%':>8}  verdict"
    )
    print(header)
    print("-" * len(header))
    for row in ordered:
        ratio = row.ratio if row.ratio is not None else 0.0
        loss = row.loss_pct if row.loss_pct is not None else 0.0
        rust_v = fmt_row_value(row, row.best_rust) if row.best_rust is not None else "-"
        sql_v = fmt_row_value(row, row.best_sql) if row.best_sql is not None else "-"
        print(
            f"{row.name[:44]:<44} {rust_v:>14} {sql_v:>14} "
            f"{ratio:>7.2f}x {loss:>7.1f}%  {verdict(row, tolerance)}"
        )
    print()

    wins = sum(1 for r in merged.values() if verdict(r, tolerance) == "WIN")
    ties = sum(1 for r in merged.values() if verdict(r, tolerance) == "TIE")
    print(
        f"[bench-gate:{args.parser}] {wins} wins / {ties} ties / {len(failed)} losses "
        f"(of {len(merged)} rows, {attempts_run} attempt(s), tolerance {tolerance:g}%)"
    )

    # ---------------------------------------------------------------- summary
    summary_md = [
        f"### `--parser {args.parser}` — {'PASS' if not failed else 'FAIL'}",
        "",
        f"`{' '.join(args.cmd)}` — {attempts_run} attempt(s), "
        f"tolerance ±{tolerance:g}% ({wins} wins / {ties} ties / {len(failed)} losses)",
        "",
        "| row | rustqlite | SQLite | ratio | verdict |",
        "|---|---:|---:|---:|:-:|",
    ]
    for row in ordered:
        ratio = row.ratio if row.ratio is not None else 0.0
        rust_v = fmt_row_value(row, row.best_rust) if row.best_rust is not None else "-"
        sql_v = fmt_row_value(row, row.best_sql) if row.best_sql is not None else "-"
        summary_md.append(
            f"| {row.name} | {rust_v} | {sql_v} | {ratio:.2f}x | "
            f"{'✅' if verdict(row, tolerance) != 'LOSS' else '❌'} "
            f"{verdict(row, tolerance)} |"
        )
    summary_md.append("")
    if failed:
        summary_md.append("**Lost to SQLite:**")
        for row in failed:
            summary_md.append(
                f"- `{row.name}` — rustqlite {fmt_row_value(row, row.best_rust)} vs "
                f"SQLite {fmt_row_value(row, row.best_sql)} "
                f"({row.loss_pct:.1f}% slower)"
            )
        summary_md.append("")
    if args.summary:
        with open(args.summary, "a", encoding="utf-8") as fh:
            fh.write("\n".join(summary_md))

    # ---------------------------------------------------------------- json
    payload = {
        "parser": args.parser,
        "command": list(args.cmd),
        "attempts_run": attempts_run,
        "tolerance_pct": tolerance,
        "rows": [
            {
                "name": r.name,
                "metric": r.metric,
                "best_rust": r.best_rust,
                "best_sql": r.best_sql,
                "ratio": r.ratio,
                "loss_pct": r.loss_pct,
                "verdict": verdict(r, tolerance),
                "samples_rust": len(r.rust),
                "samples_sql": len(r.sql),
            }
            for r in ordered
        ],
        "wins": wins,
        "ties": ties,
        "losses": len(failed),
        "pass": not failed,
    }
    json_path = results_dir / f"{args.parser}.json"
    json_path.write_text(
        json.dumps(payload, indent=2, ensure_ascii=False), encoding="utf-8"
    )
    print(f"[bench-gate:{args.parser}] results: {json_path}")

    if failed:
        print(
            f"[bench-gate:{args.parser}] FAIL — rustqlite lost to SQLite on "
            f"{len(failed)} row(s):"
        )
        for row in failed:
            print(
                f"  - {row.name}: rustqlite {fmt_row_value(row, row.best_rust)} vs "
                f"SQLite {fmt_row_value(row, row.best_sql)} "
                f"({row.loss_pct:.1f}% slower, ratio {row.ratio:.2f}x)"
            )
        return 1

    print(f"[bench-gate:{args.parser}] PASS — every row beats (or ties) SQLite")
    return 0


if __name__ == "__main__":
    sys.exit(main())
