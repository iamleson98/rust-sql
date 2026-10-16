#!/usr/bin/env python3
"""Run SQLite's TCL test files differentially: each file under the bundled
SQLite (`sqlite`) and under rustqlite (`ours`), through tests/tcl/shim.tcl.
A test that passes on `sqlite` but fails (or never runs) on `ours` is a
divergence; tests that fail on both depend on testfixture internals and
are ignored.

    python3 tests/tcl/run.py SQLITE_TEST_DIR [FILE_GLOB...] [-j N] [-o OUT.json]
                             [--shard I/N]
"""
import argparse, glob, json, os, re, resource, shutil, signal, subprocess, sys, tempfile, time
from concurrent.futures import ThreadPoolExecutor

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
SERVER = os.environ.get("TCL_DB_SERVER") or os.path.join(ROOT, "target", "release", "examples", "tcl_db_server")
SHIM = os.path.join(HERE, "shim.tcl")


# SQLite's mega faultsim files: the per-file timeout applies to EACH
# backend, and these legitimately run for many minutes even on the
# reference SQLite (hundreds of thousands of faultsim iterations). With
# the default budget BOTH backends get cut off mid-file and the
# iteration count at the cutoff becomes "divergence" - noise, not
# signal. These files get a bigger, still-identical budget per backend.
PER_FILE_TIMEOUT = {
    "quota.test": 1500,
    "tkt-313723c356.test": 1500,
    "corruptC.test": 1500,
    "wal4.test": 1500,
    "withM.test": 1500,
    "bitvec.test": 600,
    "speed1p.test": 900,
    "incrvacuum.test": 900,
    "incrvacuum2.test": 900,
    "multiplex3.test": 900,
}


def timeout_for(path, default):
    return PER_FILE_TIMEOUT.get(os.path.basename(path), default)


def run_one(testdir, path, backend, timeout):
    # Not TemporaryDirectory: its cleanup RAISES on a race (a killed
    # process group can leave a late unlink behind - Errno 39 once
    # killed a whole CI shard). mkdtemp + ignore_errors rmtree absorbs
    # it; the retry loop closes the tiny window where the killed
    # process still held the directory open.
    cwd = tempfile.mkdtemp(prefix="tcl-")
    try:
        # Own process group: on timeout the database server child dies
        # too (an orphaned server would hold the output pipes open).
        p = subprocess.Popen(
            ["tclsh", SHIM, SERVER, backend, testdir, path],
            cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            start_new_session=True,
            # A runaway test must not fill the disk: files stop at 1 GiB.
            preexec_fn=lambda: resource.setrlimit(resource.RLIMIT_FSIZE, (1 << 30, 1 << 30)),
        )
        try:
            o, e = p.communicate(timeout=timeout)
            timed_out = False
        except subprocess.TimeoutExpired:
            os.killpg(p.pid, signal.SIGKILL)
            o, e = p.communicate()
            timed_out = True
        out = o.decode("utf-8", "replace")
        err = e.decode("utf-8", "replace")
    finally:
        for _ in range(4):
            if not os.path.isdir(cwd):
                break
            shutil.rmtree(cwd, ignore_errors=True)
            time.sleep(0.05)
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
    ap.add_argument("--exclude", default=r"malloc|fault|crash|ioerr|oom",
                    help="regex of file names to skip (fault-injection files "
                         "drive testfixture C hooks the shim cannot provide)")
    ap.add_argument("--shard", default=None,
                    help="I/N: run only every N-th file starting at I (CI fan-out)")
    a = ap.parse_args()
    files = []
    for g in a.files or ["*.test"]:
        files += glob.glob(os.path.join(a.testdir, g))
    files = sorted(f for f in set(files)
                   if not (a.exclude and re.search(a.exclude, os.path.basename(f))))
    if a.shard:
        i, n = (int(x) for x in a.shard.split("/"))
        files = files[i::n]

    def work(f):
        tmo = timeout_for(f, a.timeout)
        sq, _, _, sq_to = run_one(a.testdir, f, "sqlite", tmo)
        ours, out, unk, to = run_one(a.testdir, f, "ours", tmo)
        diverge = [n for n, r in sq.items() if r == "PASS" and ours.get(n) != "PASS"]
        # The honest split of that list: a test our side RAN and got a
        # different answer on is a real divergence; a test our side never
        # reached is almost always the file's timeout cutting the run
        # short (the fault-sim files die mid-loop), which is a failure to
        # FINISH, not a semantic difference. Both are reported; they are
        # just not the same number.
        ran = [n for n in diverge if ours.get(n) == "FAIL"]
        unreached = [n for n in diverge if n not in ours]
        # The failing tests' output blocks on `ours` (first 50 only — the
        # fault-injection files diverge by the thousand).
        starts = {}
        for m in re.finditer(r"^(\S+)\.\.\.", out, re.M):
            starts.setdefault(m.group(1), m.start())
        details = {}
        for n in diverge[:50]:
            i = starts.get(n, -1)
            details[n] = out[i:i + 1500] if i >= 0 else ("(not reached)" + (" TIMEOUT" if to else ""))
        return f, {
            "sqlite_pass": sum(1 for r in sq.values() if r == "PASS"),
            "ours_pass": sum(1 for r in ours.values() if r == "PASS"),
            "diverge": diverge,
            "diverge_ran": ran,
            "diverge_unreached": unreached,
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
    tot_ran = sum(len(r.get("diverge_ran", r["diverge"])) for r in results.values())
    tot_unre = sum(len(r.get("diverge_unreached", [])) for r in results.values())
    print(f"TOTAL: {len(results)} files, {tot_sq} tests pass on sqlite, "
          f"{tot_div} of them diverge on ours "
          f"({tot_ran} ran-and-failed, {tot_unre} never reached) "
          f"({sum(1 for r in results.values() if r['diverge'])} files)")
    if a.o:
        with open(a.o, "w") as fh:
            json.dump(results, fh, indent=1)


if __name__ == "__main__":
    main()
