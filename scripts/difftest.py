#!/usr/bin/env python3
"""Per-statement differential runner: OURS (rustqlite-cli) vs ORACLE
(bundled SQLite, scripts/oracle.c). Splits a speedtest1-style script
into statements, runs each on both engines, prints only the mismatches.

    cc -O2 -I vendor/libsqlite3-sys/sqlite3 scripts/oracle.c \\
       vendor/libsqlite3-sys/sqlite3/sqlite3.c -o /tmp/oracle -lm
    python3 scripts/difftest.py SCRIPT.sql [SUBSTRING-FILTER]
"""
import os
import re
import subprocess
import sys

# Overridable: OURS=/path/to/cli ORACLE=/path/to/oracle python3 ...
ROOT = os.path.normpath(os.path.join(os.path.dirname(__file__), ".."))
OURS = os.environ.get("OURS", os.path.join(ROOT, "target/release/rustqlite-cli"))
ORACLE = os.environ.get("ORACLE", "/tmp/oracle")


def statements(path):
    stmts, buf = [], []
    section = "(setup)"
    for line in open(path, errors="replace"):
        line = line.rstrip("\n")
        if line.startswith("-- begin test "):
            section = line[14:]
            continue
        if line.startswith("-- end test") or line.startswith("--"):
            continue
        buf.append(line)
        if line.rstrip().endswith(";"):
            s = "\n".join(buf).strip()
            if s:
                stmts.append((section, s))
            buf = []
    tail = "\n".join(buf).strip()
    if tail:
        stmts.append((section, tail))
    return stmts


def run_ours(sql):
    p = subprocess.run([OURS], input=sql + "\n", capture_output=True,
                       text=True, timeout=120)
    rows = []
    in_table = False
    for line in p.stdout.splitlines():
        if line.startswith("|---"):
            in_table = True
            continue
        if in_table and line.startswith("|"):
            cells = [c.strip() for c in line.strip("|").split("|")]
            rows.append("|".join(cells))
        elif in_table and not line.startswith("|"):
            in_table = False
    err = ""
    for line in p.stderr.splitlines():
        if "error" in line.lower():
            err = line.strip()
            break
    return rows, err


def run_oracle(sql):
    p = subprocess.run([ORACLE], input=sql + "\n", capture_output=True,
                       text=True, timeout=120)
    rows, err = [], ""
    for line in p.stdout.splitlines():
        if line.startswith("!ERR: "):
            err = line[6:].strip()
        else:
            rows.append(line)
    return rows, err


def norm(rows):
    out = []
    for r in rows:
        # floats: 14 significant digits, like the tcl harness
        def f(m):
            try:
                return f"{float(m.group(0)):.14g}"
            except ValueError:
                return m.group(0)
        out.append(re.sub(r"-?\d+\.\d+(?:[eE][-+]?\d+)?", f, r))
    return out


def main():
    stmts = statements(sys.argv[1])
    only = sys.argv[2] if len(sys.argv) > 2 else None
    bad = 0
    for i, (sec, s) in enumerate(stmts):
        if only and only not in s:
            continue
        o, oerr = run_ours(s)
        r, rerr = run_oracle(s)
        if norm(o) != norm(r) or (oerr != "") != (rerr != ""):
            bad += 1
            print(f"### stmt {i} [{sec}]: {s[:160]}")
            print(f"  ours   : {o[:3]}{' err=' + oerr if oerr else ''}")
            print(f"  sqlite : {r[:3]}{' err=' + rerr if rerr else ''}")
    print(f"{bad} mismatching statements of {len(stmts)}")


if __name__ == "__main__":
    main()
