# Development

## Build and test

| Command | Does |
|---------|------|
| `make` | release binary in `target/release/hackmd-mcp` |
| `make check` | `cargo test --all-targets --all-features` |
| `cargo clippy --all-targets --all-features -- -D warnings` | lint gate |
| `make indent` | rustfmt, plus commentflow and shfmt when installed |
| `make coverage` | line coverage, needs cargo-llvm-cov |

CI runs the same all-target, all-feature suite with pinned cargo-llvm-cov on
Linux, macOS, and Windows, and merges their line coverage. The coverage job
checks changed production source lines against the floor in
`.ci/coverage-floor`, an integer percentage raised by hand in a reviewed change;
the gate refuses lowering or deleting it. Macro attribute spans and test
items are excluded; error and platform code remains measured. Coverage tools
are installed in CI, never linked into the server. `python3 tests/coverage.py`
checks the merge, source filter, and ratchet locally. Raw platform reports are
available as `coverage-*` artifacts, and the merged result is in the coverage
job summary.

`make check` and clippy are separate gates, and CI runs both. To install the
current checkout and register it:

```sh
make install
```

It installs into `~/.local/bin` (set `BINDIR=...` for another directory), then
registers the binary with Claude Code when the `claude` CLI is on
`PATH`, and with Codex when `~/.codex/config.toml` (or
`$CODEX_HOME/config.toml`) exists, writing the entries
[clients.md](clients.md) describes. An entry either client already has is left
as is, since it may carry env or args you added, so update it yourself after
changing `BINDIR`. The installer needs Python 3.9 or newer, and 3.11 once
there is a Codex config to read; `PYTHON=...` picks the interpreter. `make
register` repeats only the registration. `python3 tests/install.py` checks all
of this against stubbed `cargo` and `claude`; the harness itself reads TOML,
so it needs Python 3.11 or newer.

The `otel` feature adds an OTLP span exporter, switched on at runtime with
`HACKMD_MCP_OTEL=1`. Release binaries are built without it.

## Live API suites

The live suites are ignored by default. The read-only one mutates nothing and
prints both conditional-validator results and compression body-byte counts:

```sh
HACKMD_RUN_LIVE_READONLY_TESTS=1 HACKMD_LIVE_TEST_TOKEN=... \
    HACKMD_LIVE_TEST_TEAM_PATH=... HACKMD_LIVE_TEST_LARGE_NOTE_ID=... \
    cargo test --test live-readonly -- --ignored --nocapture
```

The large note must have at least 64 KiB of Markdown in the selected team
(personal when the team path is unset). Identity, `gzip, br`, and gzip-only
GETs measure response body bytes with automatic decompression disabled.

### Measured validators and compression (2026-10-14)

Both ignored read-only tests passed at revision `12d1dec` against
`https://api.hackmd.io/v1` (no API URL override).
This is the tracked evidence record for the dated local TODO/DONE entries.
The dedicated-token default remains in place; the user authorized use of the
configured token for this read-only run only. No note content, identifiers or
credentials are recorded here.

| GET endpoint | Status | ETag | Last-Modified | If-None-Match |
|--------------|--------|------|---------------|---------------|
| `/me` | 200 | present, weak | absent | 304 |
| `/notes` | 200 | present, weak | absent | 304 |
| `/notes/{id}` | 200 | present, weak | absent | 304 |
| `/teams` | 200 | present, weak | absent | 304 |
| `/teams/{team}/notes` | 200 | present, weak | absent | 304 |
| `/teams/{team}/notes/{id}` | 200 | present, weak | absent | 304 |

The personal and team note GETs therefore support conditional revalidation for
this deployment. Last-Modified conditionals were not sent because that header
was absent. This observation permits implementing the bounded snapshot cache;
each conditional request was sampled once immediately after its initial GET.
This initial read-only observation does not establish behavior on other
deployments or rate-limit accounting. Post-edit validator behavior is
[measured below](#measured-post-edit-validators-2026-10-14).

The compression test read an existing team note with 233,314 Markdown bytes.
All three GETs succeeded, omitted Content-Encoding, and transferred 238,438
HTTP body bytes (excluding headers and framing): `identity`, `gzip, br`, and
`gzip`. The offered encodings saved zero body bytes, so runtime gzip support
stays disabled. These are separate requests to a live note, not an atomic snapshot.
Recheck before adding a compression dependency if the deployment changes.

The destructive suite creates and deletes notes and folders. Run it only with a
dedicated throwaway account; the environment gates it requires are listed at
the top of `tests/live-destructive.rs`.

### Snapshot validation and workload

`tests/live-validators.rs` is an additional ignored write probe, with the same
explicit destructive gates. Run only with separately authorized writes:

```sh
cargo test --test live-validators -- --ignored --nocapture
```

It creates one personal and one selected-team note with owner permissions,
checks that each saved ETag returns 304 before an edit and 200 after separate
body/title edits, then DELETEs only its own new notes (no purge request).
It does not run the broader destructive suite. The completed measurement below
supports the 8 MiB default snapshot budget.

Offline verification uses
`cargo test benchmark_repeated_note_snapshots -- --ignored --nocapture`.
With Rust 1.99.0 on aarch64 macOS, eight sequential reads of the same 262,160-byte
fixture Markdown body produced these results, using the same client and
workload with only the snapshot byte capacity changed:

| Capacity | GETs | Full responses | HTTP body bytes | Loopback time |
|----------|------|----------------|-----------------|---------------|
| zero | 8 | 8 | 2,202,704 | 70,846 us |
| 8 MiB | 8 | 1 | 275,338 | 16,787 us |

Seven bodies are avoided (87.5% fewer transferred body bytes), with no GET-count
reduction. Fixture timing is not a live latency claim. Retained snapshots add
memory up to the configured byte budget; TTL, LRU eviction and write invalidation
bound that retention. Body storage shares the decoded String rather than
copying it for each reader. Full metadata reads stay uncached.

### Measured post-edit validators (2026-10-14)

The user explicitly authorized the existing configured token for this isolated
write probe. At revision `85978fa`, the ignored `live-validators` test passed
against `https://api.hackmd.io/v1`, using one new personal note and one new
selected-team note with verified owner read/write permissions. It edited only
those notes; DELETE succeeded for both, with no purge request and no
modification of existing notes. Team trash placement was not measured.
No credentials or note identifiers are published.

| Workspace | Old ETag before edit | Old ETag after body edit | Old ETag after title edit | Final unchanged ETag |
|-----------|----------------------|--------------------------|---------------------------|----------------------|
| personal | 304 before each edit | 200, new body | 200, new title and same body | 304 |
| team | 304 before each edit | 200, new body | 200, new title and same body | 304 |

Both edits produced a different ETag. Plain GET polling first confirmed each
write was visible; the conditional GET then asserted the changed fields.
These measured API writes support enabling the 8 MiB snapshot default on this
deployment. They do not measure web UI edits, other deployments, cross-replica
consistency or rate-limit accounting. Every cache read still revalidates;
TTL alone never supplies content for conflict detection.

## Continuous integration

`.github/workflows/ci.yml` runs on pushes to `main`, on pull requests, and
weekly:

| Job | What it checks |
|-----|----------------|
| `test` | the full suite with line coverage on Linux, macOS, and Windows |
| `lint` | formatting, coverage-script tests, unused dependencies, duplicate reporting, and clippy on a pinned toolchain |
| `coverage` | merged platform reports and the changed-line coverage ratchet; skipped on the weekly run |
| `msrv` | the build on the `rust-version` in `Cargo.toml` |
| `audit` | RustSec advisories against `Cargo.lock` |
| `build` | release binaries for each tested platform, Linux against glibc 2.17; skipped on the weekly run |
| `release` | on pushes to `main` only, after every job above passes, replaces the `latest` GitHub release |

The release scripts live in `.ci/`; their header comments explain the
ordering that keeps a failed run from leaving no release behind.

## Why stdio only

A remote HTTP deployment would need authentication, per-user token isolation,
endpoint allowlisting, rate limits, and encrypted credential storage with secure
rotation. GitHub sync is also excluded, because it needs separate credentials,
durable conflict state, and a frontmatter policy. Both should remain opt-in,
separately reviewed features, added only when a concrete workflow requires them.
