#!/usr/bin/env python3
"""Run SQLite's TCL test files differentially: each file under the bundled
SQLite (`sqlite`) and under rustqlite (`ours`), through tests/tcl/shim.tcl.
A test that passes on `sqlite` but fails (or never runs) on `ours` is a
divergence; tests that fail on both depend on testfixture internals and
are ignored.

    python3 tests/tcl/run.py SQLITE_TEST_DIR [FILE_GLOB...] [-j N] [-o OUT.json]
"""
import argparse, glob, json, os, re, subprocess, sys, tempfile
from concurrent.futures import ThreadPoolExecutor

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
SERVER = os.path.join(ROOT, "target", "release", "examples", "tcl_db_server")
SHIM = os.path.join(HERE, "shim.tcl")


def run_one(testdir, path, backend, timeout):
    with tempfile.TemporaryDirectory(prefix="tcl-") as cwd:
        try:
            p = subprocess.run(
                ["tclsh", SHIM, SERVER, backend, testdir, path],
                cwd=cwd, capture_output=True, timeout=timeout,
            )
            out = p.stdout.decode("utf-8", "replace")
            err = p.stderr.decode("utf-8", "replace")
            timed_out = False
        except subprocess.TimeoutExpired as e:
            out = (e.stdout or b"").decode("utf-8", "replace")
            err = (e.stderr or b"").decode("utf-8", "replace")
            timed_out = True
    res = {}
    for line in out.splitlines():
        m = re.match(r"@@(PASS|FAIL) (.*)", line)
        if m:
            res[m.group(2)] = m.group(1)
    unknown = sorted(set(re.findall(r"@@UNKNOWN (\S+)", err)))
    return res, out, unknown, timed_out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("testdir")
    ap.add_argument("files", nargs="*")
    ap.add_argument("-j", type=int, default=8)
    ap.add_argument("-o", default=None)
    ap.add_argument("--timeout", type=int, default=300)
    a = ap.parse_args()
    files = []
    for g in a.files or ["*.test"]:
        files += glob.glob(os.path.join(a.testdir, g))
    files = sorted(set(files))

    def work(f):
        sq, _, _, sq_to = run_one(a.testdir, f, "sqlite", a.timeout)
        ours, out, unk, to = run_one(a.testdir, f, "ours", a.timeout)
        diverge = [n for n, r in sq.items() if r == "PASS" and ours.get(n) != "PASS"]
        details = {}
        for n in diverge:
            # The failing test's output block on `ours`.
            i = out.find(n + "...")
            details[n] = out[i:i + 1500] if i >= 0 else ("(not reached)" + (" TIMEOUT" if to else ""))
        return f, {
            "sqlite_pass": sum(1 for r in sq.values() if r == "PASS"),
            "ours_pass": sum(1 for r in ours.values() if r == "PASS"),
            "diverge": diverge,
            "details": details,
            "timeout": to,
            "sqlite_timeout": sq_to,
        }

    results = {}
    with ThreadPoolExecutor(max_workers=a.j) as ex:
        for f, r in ex.map(work, files):
            results[os.path.basename(f)] = r
            flag = "OK  " if not r["diverge"] else "DIFF"
            print(f"{flag} {os.path.basename(f)}: sqlite {r['sqlite_pass']} pass, "
                  f"ours {r['ours_pass']} pass, {len(r['diverge'])} diverge"
                  + (" TIMEOUT" if r["timeout"] else ""), flush=True)
    tot_sq = sum(r["sqlite_pass"] for r in results.values())
    tot_div = sum(len(r["diverge"]) for r in results.values())
    print(f"TOTAL: {len(results)} files, {tot_sq} tests pass on sqlite, "
          f"{tot_div} of them diverge on ours "
          f"({sum(1 for r in results.values() if r['diverge'])} files)")
    if a.o:
        with open(a.o, "w") as fh:
            json.dump(results, fh, indent=1)


if __name__ == "__main__":
    main()
