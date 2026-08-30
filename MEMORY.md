# Memory baselines

Run `scripts/measure-memory.sh [output.tsv]` on an otherwise idle machine. The
script performs locked release builds first, then records GNU `time` peak RSS
and elapsed time for the same five workloads with default features and `otel`.
The ignored workloads enforce correctness and byte/request budgets but no
machine-dependent timing or RSS threshold.

Baseline from 2026-08-31 (Linux x86-64, Rust 1.88-compatible lockfile):

| Features | Workload | Peak RSS (KiB) | Elapsed (s) |
| --- | ---: | ---: | ---: |
| default | startup | 5,632 | 0.30 |
| default | list 10,000 | 76,288 | 0.29 |
| default | pull 10 MiB | 76,288 | 1.89 |
| default | safe push | 76,288 | 0.78 |
| default | three-way conflict | 158,208 | 0.34 |
| otel | startup | 6,656 | 0.01 |
| otel | list 10,000 | 79,872 | 0.30 |
| otel | pull 10 MiB | 79,872 | 0.42 |
| otel | safe push | 79,360 | 0.25 |
| otel | three-way conflict | 158,720 | 0.33 |

The test-process measurements include the Rust test harness and the in-process
HTTP fixture, including its owned response body. They are reproducible
regression baselines, not estimates of a production server's idle footprint.
Compare like-for-like runs and investigate a stable increase before setting a
threshold.

Valgrind Massif 3.22 did not reach even the bounded startup workload in two
minutes in this environment and produced no snapshot. GNU libc `memusage` was
used as the available equivalent allocation profiler. Default release results
were 228,413 bytes peak heap at startup, 15,228,300 for 10,000-note listing,
68,307,256 for the real 10 MiB pull, 631,310 for safe push, and 157,368,921 for
the maximum conflict workload (which deliberately owns three 50 MiB inputs).
Tracked allocations were freed by process exit in every workload.

The only intentional long-lived bulk allocation is the note-list cache. Its
field-by-field capacity accounting reported 5,987,804 retained bytes for the
10,000-note fixture, below its 8 MiB hard cap. The state store and server retain
paths/configuration and shared clients, not note bodies. Profiling therefore did
not justify converting their `String`, `Vec`, or `Arc` fields. Conflict diff
input was reduced from 256 KiB to 16 KiB per version because each rendered side
is capped at 1.9 KiB; this removes unreachable formatter work without changing
the bounded result contract.
