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

Both live suites are ignored by default. The read-only one mutates nothing and
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
It does not establish behavior on other deployments, validator changes after
a write, or rate-limit accounting.

The compression test read an existing team note with 233,314 Markdown bytes.
All three GETs succeeded, omitted Content-Encoding, and transferred 238,438
HTTP body bytes (excluding headers and framing): `identity`, `gzip, br`, and
`gzip`. The offered encodings saved zero body bytes, so runtime gzip support
stays disabled. These are separate requests to a live note, not an atomic snapshot.
Recheck before adding a compression dependency if the deployment changes.

The destructive suite creates and deletes notes and folders. Run it only with a
dedicated throwaway account; the environment gates it requires are listed at
the top of `tests/live-destructive.rs`.

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
