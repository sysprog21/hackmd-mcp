# Development

## Build and test

```sh
make                 # release binary in target/release/hackmd-mcp
make check           # cargo test --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
make indent          # rustfmt, plus commentflow and shfmt when installed
make coverage        # line coverage, needs cargo-llvm-cov
```

`make check` and clippy are separate gates, and CI runs both. To install the
current checkout:

```sh
cargo install --path . --locked
```

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
