# Development

## Build and test

| Command | Does |
|---------|------|
| `make` | release binary in `target/release/hackmd-mcp` |
| `make check` | `cargo test --all-targets --all-features` |
| `cargo clippy --all-targets --all-features -- -D warnings` | lint gate |
| `make indent` | rustfmt, plus commentflow and shfmt when installed |
| `make coverage` | line coverage, needs cargo-llvm-cov |

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
prints `ETag`/`Last-Modified` headers and conditional response status:

```sh
HACKMD_RUN_LIVE_READONLY_TESTS=1 HACKMD_LIVE_TEST_TOKEN=... \
    cargo test --test live-readonly -- --ignored --nocapture
```

The destructive suite creates and deletes notes and folders. Run it only with a
dedicated throwaway account; the environment gates it requires are listed at
the top of `tests/live-destructive.rs`.

## Continuous integration

`.github/workflows/ci.yml` runs on pushes to `main`, on pull requests, and
weekly:

| Job | What it checks |
|-----|----------------|
| `test` | the full suite on Linux, macOS, and Windows |
| `lint` | `cargo fmt --check` and clippy on a pinned toolchain |
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
