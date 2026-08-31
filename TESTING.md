# Testing — the rustqlite test matrix

This project's test suite is modeled on the SQLite testing methodology
described at <https://www.sqlite.org/testing.html>. SQLite's page lists the
techniques that give it its reliability; this file maps each technique to
the concrete harness in this repository, so both humans and CI can run the
same checks.

## Quick reference

```bash
cargo test                                    # the full default matrix (~240 tests)
cargo test --test crash_recovery              # crash/power-loss simulation
cargo test --test oom_fault --features oom-injection   # OOM fault injection
cargo test --test sql_fuzz                    # mutation + structured SQL fuzz
cargo test --test db_corrupt_fuzz             # database corruption fuzz
cargo test --test integrity_check             # PRAGMA integrity_check itself
cargo test --test differential                # results vs real SQLite
RUSTQLITE_FUZZ_ITERS=200000 cargo test --test sql_fuzz   # long fuzz run
RUSTQLITE_FUZZ_SEED=12345 cargo test --test sql_fuzz     # reproduce a failure
cargo clippy --all-targets                    # zero-warning lint gate
cargo run --release --example bench_compare   # performance vs SQLite
```

## The matrix (SQLite technique → rustqlite harness)

| SQLite technique (testing.html §) | rustqlite harness | What it verifies |
|---|---|---|
| **TCL test harness / assert-heavy tests** (§2) | all of `tests/` | every feature is exercised through the public API with `assert!`/`assert_eq!` on exact values |
| **TH3-style branch coverage** (§2.1) | `cargo test` + clippy's exhaustive-match lint | the compiler enforces every `Plan`/`Expr` arm is handled; tests drive each arm |
| **Regression tests** (§2.2) | `tests/regression.rs` | one test per historically-found bug, pinned forever |
| **Boundary-value tests** | `tests/boundary.rs` | i64/MIN/MAX rowids, extreme keys in indexes, LIKE/GLOB edge patterns, deep nesting, huge statements |
| **I/O error injection** (§3.2) | `tests/io_fault.rs` | read-only files, `/dev/full` (ENOSPC), truncation behind the handle, deleted files, read-only dirs — every fault must produce a graceful `Err`, and the file must pass `PRAGMA integrity_check` afterwards |
| **Crash / power-loss simulation** (§3.3) | `tests/crash_recovery.rs` | a child process runs a deterministic workload and `abort()`s at EVERY statement boundary (SIGABRT = no unwinding, no cleanup); the parent re-opens the database after each crash and asserts (1) the committed baseline survived, (2) the in-flight transaction is all-or-nothing, (3) `PRAGMA integrity_check` passes, (4) the writer still works. Both rollback-journal and WAL modes, plus a no-crash control run |
| **OOM fault injection** (§3.5) | `tests/oom_fault.rs` (`--features oom-injection`) | a counting global allocator fails at allocation #N; the parent sweeps N across the entire workload range (527 fault points) and verifies the baseline survives every single failure — SQLite's memsys5 equivalent |
| **Database corruption fuzz** (§4.4) | `tests/db_corrupt_fuzz.rs` | random byte strikes, targeted structural strikes (page headers, cell pointers, file header fields), truncations, zero-fills, garbage files, corrupt WALs — never a panic, never a hang, reads stay consistent, writes still work or fail gracefully |
| **SQL fuzz** (§4.1, fuzzershell/dbsqlfuzz) | `tests/sql_fuzz.rs` | (1) mutation fuzz: a corpus of valid SQL is bit-flipped/truncated/spliced and fed to the engine — no panics, no hangs, `SELECT 1` sanity after every batch; (2) structured random SQL: generated statements run against BOTH rustqlite and real SQLite, results must match; (3) parameter fuzz: arbitrary `Value` combinations must never cause type confusion |
| **Differential testing** | `tests/differential.rs` | randomized workloads executed against rustqlite and real SQLite; row sets must be identical |
| **`PRAGMA integrity_check`** (§3.2's verifier) | `tests/integrity_check.rs` + `src/storage/integrity.rs` | full structural walk: file shape (WAL-aware), freelist chain (cycles, bounds), every table b-tree (rowid ordering, record decoding), every index b-tree (entry order, bidirectional index↔table cross-verification). Clean DBs report `ok`; induced corruption is reported (never panics). Run by the crash and I/O-error suites after every fault |
| **Soak / long-run** | `RUSTQLITE_FUZZ_ITERS=200000` env on the fuzzers; `tests/concurrency_stress.rs` | 32-thread mixed workload for 2s, long fuzz iterations |
| **Concurrency testing** (§5) | `tests/concurrency_stress.rs`, `tests/concurrent_throughput.rs`, `tests/concurrent*.rs` | 16-thread pure reads, mixed readers+writer, writer serialization, snapshot consistency, throughput scaling — no deadlocks, no lost writes, no torn reads |
| **SQL Logic Tests (SLT)** | `tests/slt_runner.rs` + `tests/slt/` | the SQL Logic Test format (record/query/result files) — the same format SQLite's core team uses for thousands of conformance cases |
| **Memory-error detection (valgrind/ASan)** (§3.4) | Rust's ownership model + `--features oom-injection` allocator | use-after-free / leaks are prevented by construction; allocation failure behavior is exercised explicitly |
| **Sanitizer-style checks** | `cargo clippy --all-targets` at zero warnings | a language-level static audit, kept clean at all times |

## Determinism and reproduction

Every fuzzer is seeded explicitly (`RUSTQLITE_FUZZ_SEED`), so any failure
reproduces exactly. This mirrors SQLite's fuzzcheck seed-corpus approach.
The crash matrix advances one statement boundary at a time, like SQLite's
crash tests advance one I/O operation at a time.

## Fault-injection feature gate

`oom-injection` swaps the global allocator for a counting allocator that
can fail at allocation #N (only one global allocator may exist per binary,
hence the feature gate). It is NOT part of the default build:

```bash
cargo test --features oom-injection --test oom_fault
```

## What "passing" means here

For fault and fuzz harnesses, the contract is deliberately SQLite's:

- **never panic, abort, or hang** — every failure must surface as `Err`,
- **committed data must survive** every fault, crash, and corruption event,
- **torn transactions must vanish** (all-or-nothing),
- **the recovered database must pass `PRAGMA integrity_check`**,
- **wrong answers are worse than errors** — result-level divergence from
  SQLite in the differential/structured fuzz suites is a test failure.
