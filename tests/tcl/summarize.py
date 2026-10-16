#!/usr/bin/env python3
"""Merge tests/tcl/run.py JSON outputs (one per CI shard) and print a
Markdown summary: totals plus the files with the most divergences.

    python3 tests/tcl/summarize.py shard-*.json [-o MERGED.json] [--top N]

A divergence our side RAN (different answer) is not the same event as a
test our side never reached because the file hit its per-file timeout
(the fault-sim files die mid-loop at an arbitrary iteration). Both are
failures and both are printed — but as separate numbers, so the real
semantic divergence rate is not drowned by one mega-file's cutoff point.
"""
import argparse, json


def buckets(r):
    """(ran_and_failed, never_reached) for one file record."""
    if "diverge_ran" in r:
        return r["diverge_ran"], r["diverge_unreached"]
    # Old-format records have no split: attribute everything to the
    # bucket the file's timeout flag implies (best effort).
    if r.get("timeout"):
        return [], r["diverge"]
    return r["diverge"], []


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("inputs", nargs="+")
    ap.add_argument("-o", default=None)
    ap.add_argument("--top", type=int, default=60)
    a = ap.parse_args()
    merged = {}
    for p in a.inputs:
        with open(p) as fh:
            merged.update(json.load(fh))

    sq = sum(r["sqlite_pass"] for r in merged.values())
    div = sum(len(r["diverge"]) for r in merged.values())
    bad = sorted(((len(r["diverge"]), f) for f, r in merged.items() if r["diverge"]), reverse=True)
    timeouts = sorted(f for f, r in merged.items() if r["timeout"] and not r.get("sqlite_timeout"))
    sq_timeouts = sorted(f for f, r in merged.items() if r.get("sqlite_timeout"))

    completed = {f: r for f, r in merged.items()
                 if not r["timeout"] and not r.get("sqlite_timeout")}
    c_sq = sum(r["sqlite_pass"] for r in completed.values())
    c_ran = sum(len(buckets(r)[0]) for r in completed.values())
    c_unre = sum(len(buckets(r)[1]) for r in completed.values())
    # Real failures inside files that later hit our timeout still count.
    t_ran = sum(len(buckets(r)[0]) for r in merged.values()) - c_ran

    print("## SQLite Tcl suite (differential)")
    print()
    print(f"- files run: **{len(merged)}**")
    print(f"- tests passing on SQLite: **{sq}**; of those diverging on rustqlite: **{div}**")
    print(f"- files that ran to completion on both engines: **{len(completed)}**")
    if c_sq:
        print(f"  - tests passing on SQLite there: **{c_sq}**; of those, "
              f"**{c_ran}** diverge on rustqlite "
              f"(**{100.0 * c_ran / c_sq:.2f}%**) across "
              f"**{sum(1 for r in completed.values() if r['diverge'])}** files")
        if c_unre:
            print(f"  - plus **{c_unre}** never reached (harness cutoff — see below)")
    if t_ran:
        print(f"- **{t_ran}** more diverge inside files our side later abandoned "
              f"to a timeout")
    print(f"- files timing out only on rustqlite: **{len(timeouts)}** "
          + (", ".join(timeouts[:12]) if timeouts else ""))
    print(f"- files timing out on SQLite too (excluded — unknowable): "
          f"**{len(sq_timeouts)}** " + (", ".join(sq_timeouts[:12]) if sq_timeouts else ""))
    print()
    if bad:
        print("| file | ran&failed | never reached | sqlite pass | ours pass |")
        print("|---|---:|---:|---:|---:|")
        for _, f in bad[:a.top]:
            r = merged[f]
            ran_n, unre_n = buckets(r)
            t = " ⏱" if r["timeout"] else ""
            ts = " ⏱sqlite" if r.get("sqlite_timeout") else ""
            print(f"| {f}{t}{ts} | {len(ran_n)} | {len(unre_n)} | "
                  f"{r['sqlite_pass']} | {r['ours_pass']} |")
        if len(bad) > a.top:
            print(f"\n…and {len(bad) - a.top} more files (see the tcl-suite-report artifact).")
    if a.o:
        with open(a.o, "w") as fh:
            json.dump(merged, fh, indent=1)


if __name__ == "__main__":
    main()
