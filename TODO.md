# hackmd-mcp

Build `hackmd-mcp`, a local-first Rust MCP server for HackMD API v1, released
under the MIT License. Stdio with a local `HACKMD_API_TOKEN` is the product;
remote hosting is a separate, authenticated deployment mode, not a prerequisite.

## Definition of done

- Fast, async, typed Rust server with no token in logs or tool responses.
- Personal and team workspaces use the same tools (`workspace` is personal or a
  `team_path`), rather than duplicate personal/team tool families.
- Read tools return small, paginated projections; only `get_note` returns body
  content.
- Write tools validate input, send only explicitly supplied fields, expose
  correct MCP annotations, and produce actionable API errors.
- The binary works over stdio without requiring a web server.

## Primary workflows

1. **Connect** — configure a HackMD API token locally and verify it with
   `hackmd_get_me`; never ask an agent to paste a token into a tool argument.
2. **Pull and edit** — download a selected personal or team note to a chosen
   local Markdown path; an AI agent edits that ordinary local file with its
   normal filesystem tools.
3. **Push safely** — compare the local file with its recorded base revision and
   current HackMD text, then update HackMD only when it is safe or the caller
   explicitly chooses an overwrite.
4. **Track remote changes** — on demand, or via an explicit bounded polling
   watch, detect a tracked note change, write the remote version locally, and
   return a merge-ready conflict result for the AI agent. Stdio MCP cannot
   spontaneously prompt an agent, so the client must invoke the check/watch
   tool.

## P0 — RMCP 3.x foundation

- [ ] Add only the required application dependencies: `tokio` (macros,
  multi-thread runtime, time), `reqwest` with rustls + JSON, `serde`,
  `serde_json`, `thiserror`, `url`, `directories`, `tracing`,
  `tracing-subscriber`, and `tempfile` (tests/atomic writes). Add `clap` only
  with the later `--help`/`--version` task. Keep modules private behind a small
  `lib.rs` test surface.
- [ ] Define `HackmdServer { client: Arc<HackmdClient> }` and implement its
  tool router with RMCP `#[tool_router(server_handler)]`, `#[tool]`, typed
  `Parameters<T>`, and `Deserialize + JsonSchema` input structs. Field docs
  must carry parameter descriptions because RMCP derives schemas from fields.
- [ ] Run the local server with `HackmdServer::serve(rmcp::transport::stdio())`
  then `waiting().await`; reserve stdout for the transport and write logs only
  to stderr.
- [ ] Add `Config` for `HACKMD_API_TOKEN`, optional
  `HACKMD_API_URL` (default `https://api.hackmd.io/v1`), a 30-second request
  timeout, and retry configuration (at most three retries; 500 ms initial,
  5-second maximum backoff). Defer token validation until a tool call so MCP
  startup stays usable and reports a helpful missing-token error.
- [ ] Load a working-directory `.env` only as a quiet local convenience: never
  print dotenv diagnostics to stdout, and let inherited environment variables
  take precedence.
- [ ] Add a local-only `HACKMD_MCP_STATE_DIR`, defaulting to the platform state
  directory plus `hackmd-mcp` via `directories`. Store one JSON sidecar and a
  private baseline file per tracked note: internal ID, workspace, local path,
  baseline body hash, last observed remote timestamp, and local file identity.
  Create state only through pull/push; never store API tokens.
- [ ] Reject non-HTTPS `HACKMD_API_URL` overrides in normal operation. Permit
  loopback HTTP only through a test-only config constructor; defer
  request-supplied URL allowlisting to the remote-HTTP task.
- [ ] Implement a single `HackmdClient`: URL-encode every path segment, attach
  bearer auth, handle empty `204`/`202` responses, parse JSON once, and map
  network/timeout/401/403/404/409/429/5xx failures to concise, actionable MCP
  errors. Include request method/path and status, never the token.
- [ ] Use typed request/response DTOs with `serde` rename rules and permission
  enums. Reject empty PATCH bodies; omit absent optional fields instead of
  overwriting HackMD account defaults.
- [ ] Add fixture-based tests for path construction, encoded IDs/team paths,
  payload omission, empty responses, and every error mapping. Run
  `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo test` locally.

## P1 — essential RMCP tools

- [ ] Let RMCP 3.x negotiate MCP initialization and generate `tools/list` /
  `tools/call`; do not hard-code a protocol version or manually maintain tool
  JSON schemas.
- [ ] Return `CallToolResult::success` with concise text and structured JSON
  for successful tools. Return `CallToolResult::error` for API failures and
  handler-level validation failures; allow RMCP's schema decoder to reject
  structurally invalid JSON-RPC parameters.
- [ ] Add read-only `hackmd_get_me` (`GET /me`) and `hackmd_list_teams`
  (`GET /teams`) discovery tools. Return each team's `path`, because it is the
  required `workspace.team_path` for all team routes.
- [ ] Define `note_ref: String` for note tools. Accept an internal API ID or a
  HackMD URL; parse personal URLs directly and resolve a team URL's `shortId`
  against its team list once, caching the resulting internal ID for the call.
  Direct internal IDs never list notes. Provide a separate `title` search only
  where requested and accept exactly one exact match; otherwise return a
  disambiguation result.
- [ ] Add `hackmd_list_notes` with `workspace`, `limit` (default 20, max 100),
  `offset`, optional case-insensitive metadata `query`, tag-all filtering, and
  deterministic `sort` (default `lastChangedAt` descending). The API list is
  unpaged, so fetch once, filter/sort locally, then slice. Return `total`,
  `count`, `offset`, `has_more`, `next_offset`, and a slim note summary. Do not
  offer folder filtering here: summaries omit folder membership.
- [ ] Add `hackmd_get_note` with full content and normalized metadata. Include
  `patch_path` exactly `notes/{percent-encoded-id}.md` for personal notes or
  `teams/{percent-encoded-team-path}/notes/{percent-encoded-id}.md` for team
  notes, for safe content edits.
- [ ] Add `hackmd_create_note`, `hackmd_update_note`, and `hackmd_delete_note`.
  Use `workspace` for both `/notes` and `/teams/{team_path}/notes`; support
  title, content, tags, description, permissions, comment/suggest-edit
  permissions where the endpoint accepts them, permalink, and folder ID.
- [ ] Set tool annotations accurately: read tools are read-only/idempotent;
  create is non-idempotent; delete and explicit overwrite are destructive; full
  content replacement is destructive because it overwrites unversioned text.
  Assert the generated `readOnlyHint`, `destructiveHint`, and `idempotentHint`
  for every tool in protocol tests.
- [ ] Add `hackmd_edit_note` as the default body-edit tool: GET current body,
  accept a unified diff with exactly one target matching `patch_path`, and
  apply it only when every hunk context matches exactly; return a tool error
  without PATCH on mismatch. PATCH only if content changed and report
  `changed: false` for a no-op. Keep
  `hackmd_update_note` for metadata and explicit full replacement.
- [ ] Add `rmcp` dev features `client` and `transport-worker` for in-process
  protocol tests. Assert generated schemas, tool annotations, `tools/list`,
  tool calls, workspace routes, pagination/search, no-op edits, and patch
  conflicts without parsing stdio by hand.

## P2 — complete daily HackMD workflow

- [ ] Add `hackmd_get_history` with the same slim, client-side pagination as
  note lists.
- [ ] Add personal `hackmd_list_trash` (`GET /trash`) and
  `hackmd_restore_note` (`PUT /trash/{note_id}/restore`) with the same slim
  pagination as note lists. Mark restore non-destructive/idempotent; keep delete
  destructive and explain that it moves a personal note to trash. Mock both
  routes and their empty/accepted responses.
- [ ] Add folder tools: `hackmd_list_folders`, `hackmd_get_folder`,
  `hackmd_create_folder`, `hackmd_update_folder`, `hackmd_delete_folder`, and
  `hackmd_reorder_folder_children`, all workspace-aware.
- [ ] Omit `parentFolderId` when creating a root folder; do not send `null`,
  which the API rejects. Confirm a requested team path exists before creating
  content or folders in it.
- [ ] Before moving a folder, reject self/descendant moves by walking the
  folder parent chain. Before deleting a non-empty folder, return its child
  count without changing state; delete only when the same request supplies
  `confirm: true`.
- [ ] Implement folder ordering as read-modify-write of only the requested
  parent entry (`root` for top level), preserving all unrelated ordering keys.
- [ ] Treat HackMD folder operations as asynchronous where applicable: after a
  note move or other `202 PATCH`, read back to verify. Normalize read-side
  `folderPaths` by extracting ancestor object IDs into `folder_ids`; the API
  does not return scalar `parentFolderId` on reads.
- [ ] Create a note in a folder as POST followed by PATCH and read-back:
  HackMD currently silently ignores `parentFolderId` on note POST despite its
  documented schema.
- [ ] Add `hackmd_upload_note_image` taking `workspace`, `note_ref`, and an
  absolute `image_path`; stream multipart data, warn through a tool error above
  5 MB unless `confirm_large_file: true`, refuse above 10 MB, map upstream 413
  to a resize hint, and return only the resulting image URL.
- [ ] Add tests for all known quirks: `202` updates, POST folder assignment,
  folder-path normalization, order-map merge, move-cycle rejection, and folder
  names containing `/` (document the API's rename/patch workaround only if
  still reproducible against the live API).

## P2.5 — local Markdown sync

- [ ] Add `hackmd_pull_note` (`workspace`, `note_ref`, absolute `local_path`,
  optional `overwrite_local: false`). Fetch the full note and create the local
  Markdown file plus its sidecar only when the destination is absent or
  overwrite was explicitly requested. Return note metadata and the path; do
  not rewrite Markdown or inject frontmatter.
- [ ] Write the pulled Markdown and sidecar through temporary files followed
  by atomic rename. Record the exact pulled body (or a body hash plus a private
  baseline file), internal note ID, workspace, and remote timestamp; write this
  state only after both files are successfully in place.
- [ ] Validate every local path before I/O: require an absolute path, resolve
  symlinks where possible, and reject a directory or an existing non-Markdown
  file unless `overwrite_local: true`. Create a missing parent only when
  `create_parent_dirs: true` is supplied.
- [ ] Add `hackmd_push_note` (`workspace`, `note_ref`, `local_path`,
  `strategy: "safe" | "overwrite"`). In `safe` mode, fetch remote content and
  compare it and the local file to the sidecar base:
  - remote unchanged → PATCH local content;
  - local unchanged → report that there is nothing to push;
  - both changed → return a conflict with local/base paths and do not overwrite
    either side.
- [ ] Re-fetch immediately before every PATCH, compare the recheck body to the
  recorded baseline, and abort on any difference. This narrows—but cannot
  eliminate—the race between comparison and HackMD's non-transactional update.
  Only update the sidecar after a confirmed successful PATCH/read-back.
- [ ] In `overwrite` mode, PATCH the exact local file content only when the
  request supplies `confirm: true`; mark the tool destructive. Read back after
  a `202` response and update the sidecar only after the write is confirmed.
- [ ] Add read-only `hackmd_check_note_sync` taking `local_path`. It resolves
  the sidecar and returns `in_sync`, `remote_changed`, `local_changed`, or
  `conflict`, with remote timestamp/body hash and no filesystem writes.
- [ ] Add `hackmd_save_remote_snapshot` taking `local_path` and optional
  `overwrite_snapshot: false`. Fetch the tracked remote body and atomically
  create sibling `*.remote.md`; refuse an existing snapshot unless overwrite is
  explicitly requested. Never overwrite the working Markdown file.
- [ ] Make conflict results agent-actionable: include a bounded unified-diff
  summary, local/baseline absolute paths, a clear `merge_required` status, and
  instructions to call `hackmd_save_remote_snapshot`. When a snapshot exists,
  include its third absolute path. Do not silently reapply an agent edit unless
  a future workflow owns a uniquely delimited section and can prove the remote
  change is outside it.
- [ ] Add `hackmd_watch_note_sync` only as a bounded, caller-owned polling
  operation taking `local_path`, `interval_seconds` (1–10), and `max_checks`.
  Reject values whose product exceeds 20 seconds; reuse the read-only check and
  stop on change/conflict/cancellation. Do not add a daemon, filesystem watcher,
  or background database.
- [ ] Add a three-way merge helper only if agents repeatedly cannot resolve the
  returned three files themselves. If added, produce a separate
  `*.merge.md` with standard conflict markers; never auto-merge and push.
- [ ] Require `confirm_large_file: true` before pulling or pushing bodies over
  5 MB; refuse bodies over 50 MB until a verified HackMD limit and streaming
  design exist.
- [ ] Test clean pull, safe push, unchanged local/remote, remote-only update,
  concurrent local+remote edit, overwrite confirmation, `202` read-back,
  cancellation, atomic state persistence, snapshot overwrite refusal,
  team-slug resolution, ambiguous title handling, root-folder creation,
  large-body guards, and path validation with temporary directories and mocked
  API responses.

## P3 — efficiency and reliability

- [ ] Reuse one `reqwest::Client`; apply the 30-second timeout; retry only safe
  GETs plus explicitly idempotent PATCHes on transient network/5xx/429 errors,
  at most three times with 500 ms–5 s exponential backoff and full jitter.
  Honor `Retry-After`; never retry create/delete automatically.
- [ ] When a request is retried, include bounded structured retry metadata in
  the successful or final-error tool result (`attempts`, total waited seconds,
  and whether `429` occurred). Do not emit progress noise for an unretried
  request.
- [ ] Keep all responses context-efficient: slim list/history/folder summaries,
  explicit pagination metadata, and an opt-in full-content read only.
- [ ] Add a short TTL cache only for list/discovery GETs if profiling or rate
  limits justify it; invalidate affected keys after writes. Do not cache note
  bodies by default.
- [ ] Add request IDs and redacted structured logs to stderr, with opt-in
  tracing/OpenTelemetry. Keep operational diagnostics out of MCP stdout.
- [ ] Add a guarded live smoke test suite, disabled by default and enabled only
  with a dedicated token, that verifies profile, create/read/edit/no-op,
  folder move, and cleanup in an isolated test workspace.

## P4 — remote use

- [ ] Add `clap` and implement `--version` / `--help`, sample Claude
  Desktop/Codex configs, `.env.example`, and a clear token-security warning.
  Support dotenv only as a local convenience; inherited environment wins.
- [ ] Add Streamable HTTP only after stdio is stable. Require bearer/OAuth
  authentication, per-user token isolation, URL allowlisting, rate limits, and
  secure refresh-token storage/rotation before accepting user credentials over
  HTTP. Enable RMCP's `transport-streamable-http-server` feature only in this
  mode and mount its `StreamableHttpService` on `/mcp`; do not implement legacy
  two-endpoint HTTP+SSE.
- [ ] Keep GitHub sync out of the core server. Add it only as an opt-in feature
  after a concrete workflow requires it; it needs separate GitHub credentials,
  durable sync state, conflict handling, and frontmatter normalization.

## Research notes to preserve in docs/tests

- API title precedence is YAML `title:` in content, then leading H1, then the
  title field. Explain this on create/update tools.
- Note lists omit content and folder membership; note reads expose folder
  ancestry as `folderPaths`.
- Permission fields must be preserved when omitted; previous CLI regressions
  show that defaulting them in update payloads changes note visibility.
- Tags, descriptions, title updates, folders, image uploads, teams, history,
  trash/restore, pagination, useful errors, and MCP annotations are proven
  high-value coverage from the reviewed Python/Node servers and CLI.
- Search full note bodies only when an explicit product need justifies the
  extra GET-per-note cost; metadata search is the efficient default.

## Deliberately deferred

- Database, web UI, OAuth, token encryption, session cookies, proxying, and
  GitHub synchronization are not required for a secure local stdio server.
  Revisit them only with a remote multi-user deployment requirement.
