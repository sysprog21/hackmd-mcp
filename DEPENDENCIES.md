# Dependency policy

Every direct runtime dependency below has a production call site. Default
features are disabled when they introduce capabilities the server does not use;
the lockfile and both default/all-feature builds are the source of truth for the
resolved graph.

| Dependency | Required role and feature policy |
| --- | --- |
| `cap-std` | Capability-confined local file access prevents workspace-root escapes. Its default feature set is empty. |
| `clap` | Parses the two self-check flags plus generated help/version. Color and typo suggestions are disabled; derive, errors, help, usage, and `std` remain covered by stdio tests. A measured minimal-parser experiment reduced size but regressed clean release build time, so the maintained parser remains. |
| `directories` | Selects the platform state directory when no explicit override is configured. |
| `fastrand` | Supplies retry full jitter and collision-resistant atomic-write suffixes. The `std` default is required for process/thread-local randomness. |
| `httpdate` | Parses standards-compliant HTTP-date `Retry-After` values; a partial local parser would weaken rate-limit handling. |
| `opentelemetry`, `opentelemetry-otlp`, `opentelemetry_sdk`, `tracing-opentelemetry` | Optional `otel` export only. OTLP disables defaults and enables only tonic gRPC and traces. Each crate supplies a directly used API: trace trait, exporter, provider/runtime, and tracing bridge respectively; the default graph contains none of them or tonic. |
| `reqwest` | TLS HackMD API client. Defaults are disabled; JSON payloads, streaming multipart images, Rustls, and response streaming are all used. |
| `rmcp` | MCP protocol schemas, tool macros, server routing, and stdio transport. Defaults are disabled; client/worker support is dev-only for in-process protocol tests. |
| `serde`, `serde_json` | Typed API/MCP/config wire formats and redacted structured diagnostics. |
| `sha2` | Stable content digests for three-way sync classification and baselines. |
| `similar` | Bounded conflict diffs. Its default text feature is the required API. |
| `tempfile` | Production atomic sidecar/baseline replacement uses `NamedTempFile`; tests also use temporary workspaces. It cannot be dev-only. |
| `thiserror` | Typed, actionable error boundaries without manual source plumbing. |
| `tokio` | Async runtime, filesystem/image streaming, macros, timers, and multi-threaded MCP execution; only those features are enabled. |
| `tracing` | Structured request/API diagnostics. `rmcp` also enables its default tracing feature, so locally disabling that default would not reduce the graph. |
| `tracing-subscriber` | JSON stderr formatting and `RUST_LOG` filtering. Defaults are disabled to exclude ANSI output and log-compatibility bridging. |
| `url` | Strict API-origin and HackMD-reference parsing/encoding. |

`futures-util` is dev-only for panic-safe live-test cleanup. No test-only crate
is present in the normal graph except `tempfile`, whose atomic persistence call
site is production-critical as noted above.
