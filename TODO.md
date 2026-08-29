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
4. **Track remote changes** — on demand, detect a tracked note change, write the
   remote version locally, and return a merge-ready conflict result for the AI
   agent. Stdio MCP cannot spontaneously prompt an agent, so the client must
   invoke the check tool.

## Verified API facts

Every claim here is documented or exercised by at least one reviewed
implementation, and names its evidence so a future change can re-check the same
source. Two grades appear: measured, meaning a live run against the real API
recorded the behavior, and documented, meaning a source states it. The one
inference in this section is labeled as such. These notes were taken from an
`externals/` checkout that is no longer in the tree; re-clone the sources before
relying on any single line. Sources:
`hackmd-cli` (official CLI), `hackmd-skills` (official skills + a live eval
run), `hackMD-skill`, `hackmd-mcp` (yuna0x0), `hackmd-mcp-server` (hbarcelos),
`py-hackmd-mcp`, `hackmd-agent-python`, `hackmd-mcp-proxy` (Rust, closest prior
art).

- Base URL `https://api.hackmd.io/v1`, bearer token auth. HackMD EE instances
  override the endpoint; the CLI calls that `HMD_API_ENDPOINT_URL`.
- List responses (`GET /notes`, `GET /teams/{team}/notes`) omit both `content`
  and `folderPaths` (hackMD-skill `api-endpoints.md`). Folder membership is only
  visible on a single-note read, as a `folderPaths` ancestor array, never as a
  scalar `parentFolderId`.
- `POST /notes` silently drops `parentFolderId`. hackMD-skill's note-field table
  asserts this flatly; it is not one of that skill's version-verified coverage
  notes, so treat it as a strong claim, not a measurement. Placing a new note in
  a folder is POST, then PATCH with `parentFolderId`, then read back. A live eval
  run in `hackmd-skills/push-to-hackmd/evals/iteration-1` did exactly that and
  confirmed the result through `folderPaths[].name`.
- `POST /folders` rejects `"parentFolderId": null` with `Validation Failed`;
  omit the field for a root folder. That one is measured: it is a revision item
  from the live eval run above.
- `PATCH /folders/{id}` accepts `null` for `description`, `icon`, `color`, and
  `parentFolderId` to clear them, so `null` there is the documented way to move a
  folder to top level. Source is hackMD-skill `api-endpoints.md`, corroborated by
  `hackmd-mcp-server`'s `UpdateFolderInput` typing those four as
  `string | null`. Not covered by the eval run.
- Note PATCH accepts `title`, `content`, `readPermission`, `writePermission`,
  `tags`, `description`, `permalink`, `parentFolderId`. `commentPermission`,
  `suggestEditPermission`, and `noteFeatures` are create-time only — confirmed
  independently by `hackMD-skill`, `py-hackmd-mcp`, and `hackmd-mcp-server`'s
  typed `UpdateNoteInput`, which omits both permission fields.
- Permission enums: `readPermission`/`writePermission` are
  `owner | signed_in | guest`; `commentPermission` is
  `disabled | forbidden | owners | signed_in_users | everyone`;
  `suggestEditPermission` is the same set minus `everyone`.
- `writePermission` may not be more permissive than `readPermission`
  (hackMD-skill `permissions-guide.md`). Reject that pair before the request.
- `permalink` must be unique per account or team and allows only alphanumerics,
  hyphens, and underscores; a taken slug returns `409`. That is the concrete
  meaning to attach to a 409 in the error mapping.
- Folder fields are `name`, `description`, `icon` (a Unicode codepoint string
  such as `1F600`), `color` (hex such as `#4F46E5`), and `parentFolderId`.
- Folder ordering is its own endpoint: `GET`/`PUT /folders/folder-order`, body
  `{"order": {...}}`, where the map is `parentFolderId` (or the literal `root`)
  to an ordered list of child folder ids. PUT replaces the whole map, so it is
  strictly get, mutate one entry, put. The official CLI exposes exactly this
  shape as `hackmd-cli folders order --order='{"root":["id"]}'`. The team form is
  presumably `/teams/{team_path}/folders/folder-order` by the same mirroring rule
  the docs state for every other folder route, but that exact path was not read
  verbatim — confirm it before use.
- Image upload is `POST /notes/{note_id}/images`, `multipart/form-data`, field
  name `image`, response `{"data": {"link": "https://..."}}`.
- Note updates return `202` with no body; deletes return `204`. Both need
  empty-response handling rather than a JSON parse.
- Rate limits are 100 requests per 5 minutes, with a monthly quota of 2,000 on
  free and 20,000 on Prime (`py-hackmd-mcp` error hints). Use those numbers in
  the 429 error text and to size backoff.
- Title precedence is YAML `title:` in content, then a leading H1, then the
  `title` field. Every create/update tool description must say so.

## Contradictions to settle against the live API before shipping

These come from sources that disagree. Do not encode a guess; add a guarded live
test (see P3) for each and record the answer here.

- Does `DELETE /notes/{id}` trash or permanently delete?
  `hackMD-skill/api-endpoints.md` says permanent; `py-hackmd-mcp` says it trashes
  and `PUT /trash/{id}/restore` recovers it. The existence of `GET /trash` favors
  trashing. The tool stays destructive either way, but the description must be
  right.
- Is `parentFolderId` really dropped on note POST? The official
  `hackmd-cli/SKILL.md` advertises `notes create --parentFolderId=<id>` as
  working, while `hackMD-skill` states POST silently drops it and the official
  eval run used POST-then-PATCH anyway. Implement POST + PATCH + read-back, which
  is correct under both readings, and delete the extra PATCH only after a live
  test proves POST assigns the folder.
- Is there a team image-upload route? Every source documents only
  `/notes/{id}/images`. If none exists, `hackmd_upload_note_image` must reject a
  team workspace with a clear unsupported error rather than a 404; it still needs
  the `workspace` argument to resolve a bare team note ID at all.
- Does folder-order use `PUT` or `PATCH`? `hackMD-skill` documents `PUT`; the CLI
  hides it behind an SDK call.

## Lessons taken from the reviewed implementations

- The Rust proxy (`hackmd-mcp-proxy`) filters `list_notes` by `folder_id`
  against `folderPaths` extracted from the list response. List responses have no
  `folderPaths`, so that filter silently returns nothing. Do not offer folder
  filtering on a list tool; make folder membership a `get_note` field only.
- That proxy's `hackmd_edit_note` takes a Codex-style envelope
  (`*** Begin Patch` / `*** Update File: <path>` / `@@` hunks /
  `*** End Patch`), not a unified diff, and it rejects a hunk whose context
  matches more than one location. Both choices are right: agents already emit
  that envelope, and unique-context matching is what stops a hunk from landing on
  the wrong repeated block. Adopt them.
- That proxy builds `patch_path` without percent-encoding while encoding real
  URL segments. Keep it that way. The patch target is an opaque token the agent
  echoes back; encoding it only invites an agent to decode it and produce a
  mismatch.
- `hackmd-cli` issue #107: one shared `Flags.string()` instance backed both
  `--readPermission` and `--writePermission`, so setting both made them clobber
  each other. The lesson is about shared mutable parameter definitions, not about
  payload defaults.
- The separate payload lesson is `py-hackmd-mcp`'s: its create tool forced
  `comment_permission` to `signed_in_users`, silently overriding the account or
  team default, and it now sends only supplied fields. Note the exact scope —
  this was the create payload, not update, and it changed who could comment, not
  who could read. The two failures are real and distinct; the fix for both is to
  build payloads from explicitly supplied fields only. `py-hackmd-mcp` still
  defaults `read_permission`/`write_permission` to `owner` on create, so "never
  default a permission" is this project's stricter choice, not inherited
  practice.
- `hackmd-mcp-server` probes IPv6 reachability to `api.hackmd.io` at startup and
  forces IPv4-first resolution when the probe fails, because an advertised but
  unroutable IPv6 address hangs requests. Give the Rust client a connect timeout
  distinct from the 30-second request timeout, and confirm `reqwest`'s
  happy-eyeballs behavior covers this before adding anything more.
- `hackmd-mcp` (yuna0x0) ships duplicate `list_user_notes`/`list_team_notes`
  families and dumps raw API JSON into tool results via `JSON.stringify(notes)`.
  What that payload costs is `py-hackmd-mcp`'s observation about the same API
  response: 15+ fields per note, including the last editor's photo and biography.
  One workspace-parameterized tool family plus a slim projection is the
  correction; `py-hackmd-mcp` keeps 9 fields (`id`, `title`, `tags`,
  `createdAt`, `lastChangedAt`, `publishLink`, `readPermission`,
  `writePermission`, `teamPath`) and that is a good starting set, plus `shortId`
  and `permalink`.
- The `hackmd_` tool-name prefix is not cosmetic: `py-hackmd-mcp` renamed every
  tool because clients flatten all servers into one namespace and `get_me` or
  `delete_note` collide.
- `hackmd-agent-python` returns retry state to the agent in a `_meta` field
  (`was_rate_limited`, `total_attempts`, `total_wait_seconds`) and caches note
  lists for 60 seconds, invalidating on write. Both are the shape P3 wants.
- `hackmd-skills/shared/README.md` defines the anti-clobber contract this
  server's push path implements: export baseline, edit locally, re-export,
  diff baseline against the re-export, and push only on no diff. Its
  `safe-sync.sh` exits 0 updated, 1 conflict, 2 usage error.
- `hackmd-skills/shared/scripts/resolve-note.sh` treats every
  `hackmd.io/@X/slug` URL as a team URL. It is not: `@X` is equally a personal
  user path. Resolve `@X` against the `/me` `userPath` first, then the team list.

## P0 — RMCP 3.x foundation

- [ ] Create the `hackmd-mcp` Cargo crate with `license = "MIT"` on a Rust
  version supported by `rmcp` 3.x. Pin `rmcp = { version = "3",
  default-features = false, features = ["macros", "server", "transport-io"]
  }`; use its transitive `schemars` rather than a second MCP framework or
  hand-written JSON-RPC dispatcher. The Rust prior art hand-rolled JSON-RPC
  dispatch, `initialize`, and every `inputSchema` literal (about 400 lines in
  `protocol.rs` plus `schema.rs`); RMCP's derive macros delete all of it.
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
- [ ] Model `workspace` as an internally tagged enum
  (`{"kind":"personal"}` / `{"kind":"team","team_path":"x"}`) defaulting to
  personal, as the Rust proxy does. It keeps one tool family for both
  workspaces and makes the route choice a single match.
- [ ] Run the local server with `HackmdServer::serve(rmcp::transport::stdio())`
  then `waiting().await`; reserve stdout for the transport and write logs only
  to stderr.
- [ ] Add `Config` for `HACKMD_API_TOKEN`, optional
  `HACKMD_API_URL` (default `https://api.hackmd.io/v1`), a 30-second request
  timeout, a shorter connect timeout, and retry configuration (at most three
  retries; 500 ms initial, 5-second maximum backoff). Defer token validation
  until a tool call so MCP startup stays usable and reports a helpful
  missing-token error; warn once on stderr at startup when the token is absent.
- [ ] Load a working-directory `.env` only as a quiet local convenience: never
  print dotenv diagnostics to stdout, let inherited environment variables take
  precedence, and read only the keys this server defines, the way
  `hackmd-mcp-server` allowlists its `SUPPORTED_ENV_KEYS`.
- [ ] Add a local-only `HACKMD_MCP_STATE_DIR`, defaulting to the platform state
  directory plus `hackmd-mcp` via `directories`. Store one JSON sidecar and a
  private baseline file per tracked note: internal ID, workspace, local path,
  baseline body hash, last observed remote timestamp, and local file identity.
  Create state only through pull/push; never store API tokens.
- [ ] Reject non-HTTPS `HACKMD_API_URL` overrides in normal operation. Permit
  loopback HTTP only through a test-only config constructor; defer
  request-supplied URL allowlisting to the remote-HTTP task. `hackmd-mcp` added
  an API-URL allowlist specifically to close an SSRF hole once it accepted the
  URL from a request header.
- [ ] Implement a single `HackmdClient`: URL-encode every path segment, attach
  bearer auth, handle empty `204`/`202` responses, parse JSON once, and map
  network/timeout/401/403/404/409/429/5xx failures to concise, actionable MCP
  errors. Include request method/path and status, never the token. Model the
  hint text on `py-hackmd-mcp`'s `_ERROR_HINTS`: name the fix, not just the
  code, and truncate an unrecognized 4xx body to about 300 characters.
- [ ] Use typed request/response DTOs with `serde` rename rules and permission
  enums. Reject empty PATCH bodies. Build payloads from explicitly supplied
  fields only, and never default a permission field. Validate the
  read/write permission ordering before sending.
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
  required `workspace.team_path` for all team routes, and keep `/me`'s
  `userPath` for note-reference resolution.
- [ ] Define `note_ref: String` for note tools. Accept an internal API ID or a
  HackMD URL. Parse a bare `hackmd.io/<id>` URL directly. For `hackmd.io/@X/slug`,
  resolve `@X` against the caller's own `userPath` first and only then against
  the team list, matching `shortId` or `permalink`; cache the resulting internal
  ID for the call. Direct internal IDs never list notes. Provide a separate
  `title` search only where requested and accept exactly one exact match;
  otherwise return a disambiguation result.
- [ ] Add `hackmd_list_notes` with `workspace`, `limit` (default 20, max 100),
  `offset`, optional case-insensitive metadata `query` over title, description,
  tags, id, and shortId, tag-all filtering, and deterministic `sort` (default
  `lastChangedAt` descending). The API list is unpaged, so fetch once,
  filter/sort locally, then slice. Return `total`, `count`, `offset`,
  `has_more`, `next_offset`, and a slim note summary. Do not offer folder
  filtering here: list responses carry no `folderPaths`.
- [ ] Add `hackmd_get_note` with full content and normalized metadata. Include
  `patch_path` exactly `notes/{id}.md` for personal notes or
  `teams/{team_path}/notes/{id}.md` for team notes, unencoded, for safe content
  edits. Normalize `folderPaths` into a `folder_ids` array here, since this is
  the only response that carries folder ancestry.
- [ ] Add `hackmd_create_note`, `hackmd_update_note`, and `hackmd_delete_note`.
  Use `workspace` for both `/notes` and `/teams/{team_path}/notes`. Create
  accepts title, content, tags, description, permalink, read/write/comment/
  suggest-edit permissions, and folder placement; update accepts the PATCH
  subset only and must reject `comment_permission` and `suggest_edit_permission`
  with an explanation rather than silently dropping them.
- [ ] Set tool annotations accurately: read tools are read-only/idempotent;
  create is non-idempotent; delete and explicit overwrite are destructive; full
  content replacement is destructive because it overwrites unversioned text.
  Assert the generated `readOnlyHint`, `destructiveHint`, and `idempotentHint`
  for every tool in protocol tests, the way the Rust proxy asserts them in its
  tool-list test.
- [ ] Add `hackmd_edit_note` as the default body-edit tool: GET current body,
  accept a Codex-style patch envelope with exactly one `*** Update File:` target
  matching `patch_path`, and apply it only when every hunk context matches
  exactly one location. Reject ambiguous context, missing context, unsupported
  add/delete/move operations, malformed hunk lines, and a wrong target with
  distinct errors, and return a tool error without PATCH. Preserve the body's
  original trailing-newline state. PATCH only if content changed and report
  `changed: false` for a no-op. Keep `hackmd_update_note` for metadata and
  explicit full replacement, and say so in both tool descriptions so agents pick
  the patch tool by default.
- [ ] Add `rmcp` dev features `client` and `transport-worker` for in-process
  protocol tests. Assert generated schemas, tool annotations, `tools/list`,
  tool calls, workspace routes, pagination/search, no-op edits, and patch
  conflicts without parsing stdio by hand.

## P2 — complete daily HackMD workflow

- [ ] Add `hackmd_get_history` (`GET /history`) with the same slim, client-side
  pagination as note lists. Tolerate both a bare array and a wrapped
  `{"history": [...]}` response, as `py-hackmd-mcp` does.
- [ ] Add personal `hackmd_list_trash` (`GET /trash`) and
  `hackmd_restore_note` (`PUT /trash/{note_id}/restore`) with the same slim
  pagination as note lists. Mark restore non-destructive/idempotent; keep delete
  destructive. Describe delete according to whichever behavior the live test in
  "Contradictions" confirms. Mock both routes and their empty/accepted responses.
- [ ] Add folder tools: `hackmd_list_folders`, `hackmd_get_folder`,
  `hackmd_create_folder`, `hackmd_update_folder`, `hackmd_delete_folder`, and
  `hackmd_set_folder_order`, all workspace-aware. Folder create/update carry
  `name`, `description`, `icon`, `color`, and `parent_folder_id`.
- [ ] Omit `parentFolderId` when creating a root folder; do not send `null`,
  which POST rejects. On folder update, `null` is the correct way to clear
  `parentFolderId`, `description`, `icon`, and `color`, so the update input
  needs a tri-state (absent, null, value) rather than `Option<String>`. Confirm
  a requested team path exists before creating content or folders in it.
- [ ] Before moving a folder, reject self/descendant moves by walking the
  folder parent chain. Before deleting a non-empty folder, return its child
  count without changing state; delete only when the same request supplies
  `confirm: true`.
- [ ] Implement `hackmd_set_folder_order` as GET `folder-order`, replace only
  the requested parent entry (`root` for top level), PUT the whole map back.
  The endpoint replaces the entire map, so preserving every unrelated key is a
  correctness requirement, not a nicety.
- [ ] Treat HackMD folder operations as asynchronous where applicable: after a
  note move or other `202` PATCH, read back to verify.
- [ ] Create a note in a folder as POST followed by PATCH and read-back. Keep
  the read-back regardless of how the POST-drops-`parentFolderId` question
  resolves; drop only the extra PATCH if a live test proves POST assigns it.
- [ ] Add `hackmd_upload_note_image` taking `workspace`, `note_ref`, and an
  absolute `image_path`; stream multipart data under field name `image`, warn
  through a tool error above 5 MB unless `confirm_large_file: true`, refuse above
  10 MB, map upstream 413 to a resize hint, and return only `data.link`. Keep
  `workspace` even though only `/notes/{id}/images` is documented: `note_ref`
  cannot resolve a bare team note ID without it. Until a team route is confirmed,
  a team workspace returns an explicit unsupported error rather than a 404.
- [ ] Add tests for all known quirks: `202` updates, POST folder assignment,
  folder-path normalization, order-map merge preserving unrelated keys,
  move-cycle rejection, root-folder creation omitting `parentFolderId`, and
  folder names containing `/`.

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
  This is the `safe-sync.sh` contract from the official skills, with the
  baseline held in the sidecar instead of a temp file.
- [ ] Re-fetch immediately before every PATCH, compare the recheck body to the
  recorded baseline, and abort on any difference. This narrows, but cannot
  eliminate, the race between comparison and HackMD's non-transactional update.
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
  explicitly requested. Never overwrite the working Markdown file. Keep it
  separate from the check tool so the check tool stays annotated read-only.
- [ ] Make conflict results agent-actionable: include a bounded unified-diff
  summary, local/baseline absolute paths, a clear `merge_required` status, and
  instructions to call `hackmd_save_remote_snapshot`. When a snapshot exists,
  include its third absolute path. Do not silently reapply an agent edit.
- [ ] Require `confirm_large_file: true` before pulling or pushing bodies over
  5 MB; refuse bodies over 50 MB. These are the thresholds the official
  `push-to-hackmd` skill already uses, still pending a verified HackMD limit.
- [ ] Test clean pull, safe push, unchanged local/remote, remote-only update,
  concurrent local+remote edit, overwrite confirmation, `202` read-back,
  atomic state persistence, snapshot overwrite refusal, team-slug resolution,
  personal `@userPath` resolution, ambiguous title handling, root-folder
  creation, large-body guards, and path validation with temporary directories
  and mocked API responses.

## P3 — efficiency and reliability

- [ ] Reuse one `reqwest::Client`; apply the 30-second request timeout and a
  short connect timeout; retry only safe GETs plus explicitly idempotent
  PATCHes on transient network/5xx/429 errors, at most three times with
  500 ms–5 s exponential backoff and full jitter. Honor `Retry-After`; never
  retry create/delete automatically.
- [ ] When a request is retried, include bounded structured retry metadata in
  the successful or final-error tool result (`attempts`, total waited seconds,
  and whether `429` occurred). Do not emit progress noise for an unretried
  request.
- [ ] Keep all responses context-efficient: slim list/history/folder summaries,
  explicit pagination metadata, and an opt-in full-content read only.
- [ ] Add a short TTL cache only for list/discovery GETs if profiling or the
  100-requests-per-5-minutes limit justifies it; invalidate affected keys after
  writes. Do not cache note bodies by default.
- [ ] Add request IDs and redacted structured logs to stderr, with opt-in
  tracing/OpenTelemetry. Keep operational diagnostics out of MCP stdout.
- [ ] Add a guarded live smoke test suite, disabled by default and enabled only
  with a dedicated token, that verifies profile, create/read/edit/no-op, folder
  create and move, delete-then-restore, and cleanup in an isolated test
  workspace. Every item under "Contradictions to settle" is answered here.

## P4 — remote use

- [ ] Add `clap` and implement `--version` / `--help`, sample Claude
  Desktop/Codex configs, `.env.example`, and a clear token-security warning.
  Support dotenv only as a local convenience; inherited environment wins.
  Document pinning an exact released version in MCP client config rather than a
  mutable tag, since a client silently starts whatever the tag resolves to.
- [ ] Add Streamable HTTP only after stdio is stable. Require bearer/OAuth
  authentication, per-user token isolation, URL allowlisting, rate limits, and
  secure refresh-token storage/rotation before accepting user credentials over
  HTTP. Enable RMCP's `transport-streamable-http-server` feature only in this
  mode and mount its `StreamableHttpService` on `/mcp`; do not implement legacy
  two-endpoint HTTP+SSE. `hackmd-mcp-proxy` is the worked example of what this
  costs: SQLite-backed client/session/token stores, ChaCha20-Poly1305 credential
  encryption, PKCE, and refresh-token rotation. Its `http/`, `store/`, `oauth.rs`,
  and `crypto.rs` come to roughly 1,900 lines that a local stdio server needs
  none of.
- [ ] Keep GitHub sync out of the core server. Add it only as an opt-in feature
  after a concrete workflow requires it; it needs separate GitHub credentials,
  durable sync state, conflict handling, and frontmatter normalization.
  `hackmd-mcp-server` carries all four and its frontmatter normalization needed
  its own bug fix.

## Deliberately deferred

- A bounded `hackmd_watch_note_sync` polling tool. A watch capped at 20 seconds
  of polling is not a watch, and the client can simply call
  `hackmd_check_note_sync` again. Revisit only if a client demonstrates it
  cannot re-invoke the check tool itself.
- A three-way merge helper. Add it only if agents repeatedly fail to resolve the
  three files a conflict already hands them; if added, produce a separate
  `*.merge.md` with standard conflict markers and never auto-merge and push.
- Searching full note bodies. It costs one GET per note; metadata search is the
  efficient default. `hackmd-agent-python` needed a 60-second list cache and
  fuzzy relevance scoring to make body search bearable, which is the price tag.
- Database, web UI, OAuth, token encryption, session cookies, proxying, and
  GitHub synchronization. Not required for a secure local stdio server; revisit
  only with a remote multi-user deployment requirement.
