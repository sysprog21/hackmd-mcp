# Memory baselines

Run `scripts/measure-memory.sh [output.tsv]` on an otherwise idle machine. The
script performs locked release builds first, then records GNU `time` peak RSS
and elapsed time for the same five workloads with default features and `otel`.
The ignored workloads enforce correctness and byte/request budgets but no
machine-dependent timing or RSS threshold.

Baseline from 2026-08-31 (Linux x86-64, Rust 1.88-compatible lockfile):

| Features | Workload | Peak RSS (KiB) | Elapsed (s) |
| --- | ---: | ---: | ---: |
| default | startup | 5,632 | 0.01 |
| default | list 10,000 | 76,288 | 0.27 |
| default | pull 10 MiB | 310,824 | 1.04 |
| default | safe push | 75,776 | 0.22 |
| default | three-way conflict | 159,232 | 0.29 |
| otel | startup | 6,144 | 0.01 |
| otel | list 10,000 | 79,872 | 0.29 |
| otel | pull 10 MiB | 309,800 | 1.04 |
| otel | safe push | 79,872 | 0.40 |
| otel | three-way conflict | 159,232 | 0.33 |

The test-process measurements include the Rust test harness and the in-process
HTTP fixture, including its owned response body. They are reproducible
regression baselines, not estimates of a production server's idle footprint.
Compare like-for-like runs and investigate a stable increase before setting a
threshold.

Valgrind Massif 3.22 did not reach even the bounded startup workload in two
minutes in this environment and produced no snapshot. Do not infer allocation
sites from that failed run. Use `heaptrack` (or a functioning Massif build) for
the retained-field review described in `TODO.md`.
