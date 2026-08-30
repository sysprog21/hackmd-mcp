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
- [x] Implement a single `HackmdClient`: URL-encode every path segment, attach
  bearer auth, handle empty `204`/`202` responses, parse JSON once, and map
  network/timeout/401/403/404/409/429/5xx failures to concise, actionable MCP
  errors. Include request method/path and status, never the token. Model the
  hint text on `py-hackmd-mcp`'s `_ERROR_HINTS`: name the fix, not just the
  code, and truncate an unrecognized 4xx body to about 300 characters.
- [x] Use typed request/response DTOs with `serde` rename rules and permission
  enums. Reject empty PATCH bodies. Build payloads from explicitly supplied
  fields only, and never default a permission field. Validate the
  read/write permission ordering before sending.
- [x] Add fixture-based tests for path construction, encoded IDs/team paths,
  payload omission, empty responses, and every error mapping. Run
  `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo test` locally.

## P1 — essential RMCP tools

- [x] Let RMCP 3.x negotiate MCP initialization and generate `tools/list` /
  `tools/call`; do not hard-code a protocol version or manually maintain tool
  JSON schemas.
- [x] Return `CallToolResult::success` with concise text and structured JSON
  for successful tools. Return `CallToolResult::error` for API failures and
  handler-level validation failures; allow RMCP's schema decoder to reject
  structurally invalid JSON-RPC parameters.
- [x] Add read-only `hackmd_get_me` (`GET /me`) and `hackmd_list_teams`
  (`GET /teams`) discovery tools. Return each team's `path`, because it is the
  required `workspace.team_path` for all team routes, and keep `/me`'s
  `userPath` for note-reference resolution.
- [x] Define `note_ref: String` for note tools. Accept an internal API ID or a
  HackMD URL. Parse a bare `hackmd.io/<id>` URL directly. For `hackmd.io/@X/slug`,
  resolve `@X` against the caller's own `userPath` first and only then against
  the team list, matching `shortId` or `permalink`; cache the resulting internal
  ID for the call. Direct internal IDs never list notes. Provide a separate
  `title` search only where requested and accept exactly one exact match;
  otherwise return a disambiguation result.
- [x] Add `hackmd_list_notes` with `workspace`, `limit` (default 20, max 100),
  `offset`, optional case-insensitive metadata `query` over title, description,
  tags, id, and shortId, tag-all filtering, and deterministic `sort` (default
  `lastChangedAt` descending). The API list is unpaged, so fetch once,
  filter/sort locally, then slice. Return `total`, `count`, `offset`,
  `has_more`, `next_offset`, and a slim note summary. Do not offer folder
  filtering here: list responses carry no `folderPaths`.
- [x] Add an opt-in `refresh` input to `hackmd_list_notes` and every note tool
  that can resolve an `@owner/slug` URL. It bypasses and replaces the
  workspace's 60-second cached list, while the default path stays cached and
  direct note IDs continue to avoid list requests entirely.
- [x] Add `hackmd_get_note` with full content and normalized metadata. Include
  `patch_path` exactly `notes/{id}.md` for personal notes or
  `teams/{team_path}/notes/{id}.md` for team notes, unencoded, for safe content
  edits. Normalize `folderPaths` into a `folder_ids` array here, since this is
  the only response that carries folder ancestry.
- [x] Add `hackmd_create_note`, `hackmd_update_note`, and `hackmd_delete_note`.
  Use `workspace` for both `/notes` and `/teams/{team_path}/notes`. Create
  accepts title, content, tags, description, permalink, read/write/comment/
  suggest-edit permissions, and folder placement; update accepts the PATCH
  subset only and must reject `comment_permission` and `suggest_edit_permission`
  with an explanation rather than silently dropping them.
- [x] Set tool annotations accurately: read tools are read-only/idempotent;
  create is non-idempotent; delete and explicit overwrite are destructive; full
  content replacement is destructive because it overwrites unversioned text.
  Assert the generated `readOnlyHint`, `destructiveHint`, and `idempotentHint`
  for every tool in protocol tests, the way the Rust proxy asserts them in its
  tool-list test.
- [x] Add `hackmd_edit_note` as the default body-edit tool: GET current body,
  accept a Codex-style patch envelope with exactly one `*** Update File:` target
  matching `patch_path`, and apply it only when every hunk context matches
  exactly one location. Reject ambiguous context, missing context, unsupported
  add/delete/move operations, malformed hunk lines, and a wrong target with
  distinct errors, and return a tool error without PATCH. Preserve the body's
  original trailing-newline state. PATCH only if content changed and report
  `changed: false` for a no-op. Keep `hackmd_update_note` for metadata and
  explicit full replacement, and say so in both tool descriptions so agents pick
  the patch tool by default.
- [x] Add `rmcp` dev features `client` and `transport-worker` for in-process
  protocol tests. Assert generated schemas, tool annotations, `tools/list`,
  tool calls, workspace routes, pagination/search, no-op edits, and patch
  conflicts without parsing stdio by hand.

## P2 — complete daily HackMD workflow

- [x] Add `hackmd_get_history` (`GET /history`) with the same slim, client-side
  pagination as note lists. Tolerate both a bare array and a wrapped
  `{"history": [...]}` response, as `py-hackmd-mcp` does.
- [x] Add folder tools: `hackmd_list_folders`, `hackmd_get_folder`,
  `hackmd_create_folder`, `hackmd_update_folder`, `hackmd_delete_folder`, and
  `hackmd_set_folder_order`, all workspace-aware except that HackMD exposes
  folder metadata PATCH only for teams. Folder create carries `name`,
  `description`, `icon`, `color`, and `parent_folder_id`; team update carries
  the metadata fields.
- [x] Omit `parentFolderId` when creating a root folder; do not send `null`,
  which POST rejects. On folder update, absent/null/value states clear
  `description`, `icon`, and `color`; reject every update-time
  `parentFolderId` because guarded personal and team probes proved moves are
  silent no-ops. Confirm a requested team path exists before creating content
  or folders in it.
- [x] Reject unsupported folder moves before network I/O. Before deleting a
  non-empty folder, return its child
  count without changing state; delete only when the same request supplies
  `confirm: true`.
- [x] Implement `hackmd_set_folder_order` as GET `folder-order`, replace only
  the requested parent entry (`root` for top level), and PUT the whole map back
  while preserving every unrelated key.
- [x] Treat HackMD folder and note-move operations as asynchronous where
  applicable: read back after accepted PATCH requests, and verify safe body
  edits match the requested content.
- [x] Create a note in a folder by sending `parentFolderId` on POST and reading
  it back. A guarded live probe confirmed current personal POST placement; issue
  the compatibility PATCH and second readback only when the first readback
  shows that an older deployment dropped the field.
- [x] Add `hackmd_upload_note_image` with `workspace`, `note_ref`, and an
  absolute `image_path`; stream multipart field `image`, require confirmation
  above 5 MiB, refuse above 10 MiB, map 413 to a resize hint, and return only
  `data.link`. A guarded team probe confirmed the inferred team route returns
  404, so reject team uploads with an explicit unsupported error.
- [x] Add fixture tests for known quirks: `202` readbacks, POST-then-PATCH folder
  assignment, folder-path normalization, order-map merge preservation, folder
  move rejection, root-folder create omission, and folder IDs containing `/`.
- [x] Add personal `hackmd_list_trash` (`GET /trash`) and
  `hackmd_restore_note` (`PUT /trash/{note_id}/restore`) with slim pagination,
  non-destructive/idempotent restore annotations, encoded IDs, input guards,
  and empty/accepted response coverage. A guarded live run confirmed DELETE
  moves personal notes to trash and restore makes them readable again.

## P2.5 — local Markdown sync

- [x] Add paginated `hackmd_list_tracked_notes` for discovering private sync
  records without reading working files or contacting HackMD. Add confirmed,
  local-only `hackmd_untrack_note` keyed by workspace and internal note ID; it
  removes the verified sidecar, exact baseline, and only an owned by-path hint,
  while never opening, changing, or deleting the Markdown file. Stale records
  remain removable after their working file has disappeared.

- [x] Add `hackmd_pull_note` with workspace-aware note resolution and an
  absolute local path. Write the exact remote Markdown body without rewriting
  it or injecting frontmatter, requiring explicit overwrite for existing files.
- [x] Persist the working Markdown, private exact baseline, and JSON sidecar via
  temporary files and atomic renames. Record SHA-256, internal note ID,
  workspace, remote timestamp, canonical path, and local file identity.
- [x] Validate paths before remote access: reject relative paths, directories,
  existing files without confirmation, and existing non-Markdown files without
  overwrite; resolve symlinks and create parents only when explicitly enabled.
- [x] Add `hackmd_push_note` with safe and overwrite strategies. Safe mode
  compares local and remote bodies to the private exact baseline, reports
  no-op/remote-only/conflict states without mutation, and pushes only when the
  remote baseline is unchanged.
- [x] Immediately re-fetch before every safe PATCH and abort to a conflict if
  the baseline changed. Advance private sync state only after PATCH plus an
  exact-content readback.
- [x] Require `confirm: true` for overwrite mode, mark the tool destructive,
  PATCH the exact local body, read back accepted `202` updates, and persist the
  new baseline only after confirmation.
- [x] Add read-only `hackmd_check_note_sync` taking `local_path`. Resolve its
  private sidecar and return `in_sync`, `remote_changed`, `local_changed`, or
  `conflict` with remote timestamp and SHA-256 baseline/local/remote hashes,
  without filesystem writes.
- [x] Add `hackmd_save_remote_snapshot` with optional explicit overwrite.
  Atomically save the tracked remote body as sibling `*.remote.md`, refuse an
  existing snapshot by default, and never overwrite the working Markdown file.
- [x] Make safe-push conflicts agent-actionable with bounded baseline→local and
  baseline→remote unified diffs, absolute local/baseline paths,
  `merge_required: true`, snapshot instructions, and the existing absolute
  `*.remote.md` path when present. Never silently reapply an agent edit.
- [x] Require `confirm_large_file: true` before pulling or pushing bodies over
  5 MiB and refuse bodies over 50 MiB, with exact boundary tests matching the
  external `push-to-hackmd` policy pending a verified HackMD limit.
- [x] Cover the P2.5 workflow matrix with temporary directories and mocked API
  responses: clean pull, safe/no-op/remote-only/conflicting pushes, overwrite
  confirmation, `202` readback, atomic state, snapshot refusal, team and
  personal scoped URL resolution, ambiguous titles, root folders, size guards,
  and path validation.

## P3 — efficiency and reliability

- [x] Add `tests/live-smoke.rs`, disabled by default and gated by
  `HACKMD_RUN_LIVE_TESTS=1` plus a dedicated token. Its panic-safe cleanup
  covers personal profile, note create/read/edit/no-op, nested folders and
  order, delete/trash/restore, plus team metadata PATCH, unsupported folder
  moves, and the absent team image route. Both guarded workflows passed against
  HackMD on 2026-08-29.

- [x] Reuse one `reqwest::Client` with distinct 30-second request and 10-second
  connect timeouts. Retry only GETs and explicitly idempotent PATCHes after
  transient network/5xx/429 failures, at most three retries with capped
  500 ms–5 s exponential full jitter and `Retry-After`; never retry
  create/delete automatically.
- [x] Attach concurrency-safe `_meta.retry` data only after an actual retry,
  including attempts, total waited seconds, and whether a 429 occurred, on
  successful and final-error tool results. Emit no retry metadata or progress
  noise for single-attempt requests.
- [x] Keep responses context-efficient: note lists and history return slim,
  explicitly paginated projections; folder lists now do the same with a
  default limit of 20 and maximum of 100. Only the explicitly invoked
  `hackmd_get_note` read returns full note content.
- [x] Cache each workspace's unpaginated note list for 60 seconds because both
  listing and `@owner/slug` reference resolution consume it. Share cached
  slices without cloning the full response and clear the cache before and
  after every write. Note bodies remain uncached, and callers can explicitly
  refresh a workspace list.
- [x] Add process-unique request IDs and redacted JSON tracing to stderr for
  every MCP tool call and HackMD request. `RUST_LOG` opts into debug detail;
  the optional `otel` feature plus `HACKMD_MCP_OTEL=true` exports spans through
  the standard OTLP environment configuration. MCP stdout remains transport-only.

## P4 — remote use

- [x] Add `clap`-generated `--help` and `--version`, covered through the built
  executable. Add `README.md`, `.env.example`, `.env` ignore protection, sample
  Claude Desktop and Codex stdio configurations, token/state security guidance,
  inherited-environment precedence, and exact-release pinning guidance.
- [x] Keep transport stdio-only. Streamable HTTP is intentionally excluded
  because no remote multi-user requirement exists and a secure implementation
  would require authentication, per-user token isolation, URL allowlisting,
  rate limits, encrypted credential storage, and refresh-token rotation. Do not
  accept credentials over HTTP until that separately reviewed scope exists.
- [x] Keep GitHub sync out of the core server. No concrete workflow currently
  justifies separate GitHub credentials, durable sync/conflict state, and a
  frontmatter-normalization policy; revisit only as an opt-in feature.
