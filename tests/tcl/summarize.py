#!/usr/bin/env python3
"""Merge tests/tcl/run.py JSON outputs (one per CI shard) and print a
Markdown summary: totals plus the files with the most divergences.

    python3 tests/tcl/summarize.py shard-*.json [-o MERGED.json] [--top N]
"""
import argparse, json


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
    print("## SQLite Tcl suite (differential)")
    print()
    print(f"- files run: **{len(merged)}**")
    print(f"- tests passing on SQLite: **{sq}**")
    print(f"- of those, diverging on rustqlite: **{div}** "
          f"({100.0 * div / sq if sq else 0:.2f}%) across **{len(bad)}** files")
    print(f"- files timing out only on rustqlite: **{len(timeouts)}**")
    print()
    if bad:
        print("| file | diverging | sqlite pass | ours pass |")
        print("|---|---:|---:|---:|")
        for n, f in bad[:a.top]:
            r = merged[f]
            t = " (timeout)" if r["timeout"] else ""
            print(f"| {f}{t} | {n} | {r['sqlite_pass']} | {r['ours_pass']} |")
        if len(bad) > a.top:
            print(f"\n…and {len(bad) - a.top} more files (see the tcl-suite-report artifact).")
    if a.o:
        with open(a.o, "w") as fh:
            json.dump(merged, fh, indent=1)


if __name__ == "__main__":
    main()
