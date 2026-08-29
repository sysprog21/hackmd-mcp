# Completed tasks

## P0 — RMCP 3.x foundation

- [x] Create the `hackmd-mcp` Cargo crate with `license = "MIT"` on a Rust
  version supported by `rmcp` 3.x. Pin `rmcp = { version = "3",
  default-features = false, features = ["macros", "server", "transport-io"]
  }`; use its transitive `schemars` rather than a second MCP framework or
  hand-written JSON-RPC dispatcher. The Rust prior art hand-rolled JSON-RPC
  dispatch, `initialize`, and every `inputSchema` literal (about 400 lines in
  `protocol.rs` plus `schema.rs`); RMCP's derive macros delete all of it.
- [x] Add only the required application dependencies: `tokio` (macros,
  multi-thread runtime, time), `reqwest` with rustls + JSON, `serde`,
  `serde_json`, `thiserror`, `url`, `directories`, `tracing`,
  `tracing-subscriber`, and `tempfile` (tests/atomic writes). Add `clap` only
  with the later `--help`/`--version` task. Keep modules private behind a small
  `lib.rs` test surface.
- [x] Define `HackmdServer { client: Arc<HackmdClient> }` and implement its
  tool router with RMCP `#[tool_router(server_handler)]`, `#[tool]`, typed
  `Parameters<T>`, and `Deserialize + JsonSchema` input structs. Field docs
  must carry parameter descriptions because RMCP derives schemas from fields.
- [x] Model `workspace` as an internally tagged enum
  (`{"kind":"personal"}` / `{"kind":"team","team_path":"x"}`) defaulting to
  personal, as the Rust proxy does. It keeps one tool family for both
  workspaces and makes the route choice a single match.
- [x] Run the local server with `HackmdServer::serve(rmcp::transport::stdio())`
  then `waiting().await`; reserve stdout for the transport and write logs only
  to stderr.
- [x] Add `Config` for `HACKMD_API_TOKEN`, optional
  `HACKMD_API_URL` (default `https://api.hackmd.io/v1`), a 30-second request
  timeout, a shorter connect timeout, and retry configuration (at most three
  retries; 500 ms initial, 5-second maximum backoff). Defer token validation
  until a tool call so MCP startup stays usable and reports a helpful
  missing-token error; warn once on stderr at startup when the token is absent.
- [x] Load a working-directory `.env` only as a quiet local convenience: never
  print dotenv diagnostics to stdout, let inherited environment variables take
  precedence, and read only the keys this server defines, the way
  `hackmd-mcp-server` allowlists its `SUPPORTED_ENV_KEYS`.
- [x] Add a local-only `HACKMD_MCP_STATE_DIR`, defaulting to the platform state
  directory plus `hackmd-mcp` via `directories`. Store one JSON sidecar and a
  private baseline file per tracked note: internal ID, workspace, local path,
  baseline body hash, last observed remote timestamp, and local file identity.
  Create state only through pull/push; never store API tokens.
- [x] Reject non-HTTPS `HACKMD_API_URL` overrides in normal operation. Permit
  loopback HTTP only through a test-only config constructor; defer
  request-supplied URL allowlisting to the remote-HTTP task. `hackmd-mcp` added
  an API-URL allowlist specifically to close an SSRF hole once it accepted the
  URL from a request header.
