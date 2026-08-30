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
`externals/` checkout in this workspace; refresh the sources before relying on
any single line. Sources:
`hackmd-cli` (official CLI), `hackmd-skills` (official skills + a live eval
run), `hackMD-skill`, `hackmd-mcp` (yuna0x0), `hackmd-mcp-server` (hbarcelos),
`py-hackmd-mcp`, `hackmd-agent-python`, `hackmd-mcp-proxy` (Rust, closest prior
art), `hackmd-api-client-rs` (Rust client plus commit history).

- Base URL `https://api.hackmd.io/v1`, bearer token auth. HackMD EE instances
  override the endpoint; the CLI calls that `HMD_API_ENDPOINT_URL`.
- List responses (`GET /notes`, `GET /teams/{team}/notes`) omit both `content`
  and `folderPaths` (hackMD-skill `api-endpoints.md`). Folder membership is only
  visible on a single-note read, as a `folderPaths` ancestor array, never as a
  scalar `parentFolderId`.
- `POST /notes` accepts `parentFolderId`: a guarded personal-workspace probe on
  2026-08-29 read the requested folder back in `folderPaths`. Older sources and
  one official eval observed or assumed it was dropped, so create sends the
  field, reads back, and uses PATCH only as a compatibility fallback.
- `POST /folders` rejects `"parentFolderId": null` with `Validation Failed`;
  omit the field for a root folder. That one is measured: it is a revision item
  from the live eval run above.
- Personal folder PATCH is not exposed by HackMD. Team folder PATCH updates
  metadata, but guarded personal and team probes on 2026-08-29 measured both a
  destination ID and `null` `parentFolderId` returning success while leaving
  the parent unchanged. Reject folder moves rather than reporting silent
  success.
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
test for each and record the answer here.

- Measured 2026-08-29: personal `DELETE /notes/{id}` moves the note into
  `GET /trash`, and `PUT /trash/{id}/restore` makes it readable again.
- Measured 2026-08-29: personal note POST assigns a supplied `parentFolderId`.
  Keep a readback-driven PATCH fallback for older deployments and unmeasured
  team behavior.
- Measured 2026-08-29: the inferred team image-upload route returns 404, so
  `hackmd_upload_note_image` correctly rejects team workspaces with a clear
  unsupported error.
- Measured 2026-08-29: folder-order accepts `PUT` with `{"order": map}`; GET
  returns the raw map.

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
  lists for 60 seconds, invalidating on write. Both behaviors are implemented;
  preserve their request and metadata contracts while optimizing them.
- `hackmd-api-client-rs` commit `3d2d8e1` corrected response models and added
  encoded path-segment, double-millisecond timestamp, error-body, and current
  endpoint contract tests; follow-up `4af0888` removed speculative raw/optional
  response APIs. Adopt its tolerant timestamps and HackMD
  `x-ratelimit-userreset` fallback, while retaining this server's safer rule
  against automatic POST/DELETE retries. Its nullable folder-parent type is
  serialization evidence only and is overruled by this project's personal and
  team live no-ops.
- `hackmd-skills/shared/README.md` defines the anti-clobber contract this
  server's push path implements: export baseline, edit locally, re-export,
  diff baseline against the re-export, and push only on no diff. Its
  `safe-sync.sh` exits 0 updated, 1 conflict, 2 usage error.
- `hackmd-skills/shared/scripts/resolve-note.sh` treats every
  `hackmd.io/@X/slug` URL as a team URL. It is not: `@X` is equally a personal
  user path. Resolve `@X` against the `/me` `userPath` first, then the team list.

## Active roadmap

The server already has the complete local-first workflow. New work must improve
measured request count, latency, peak resident memory, or dependency footprint
without weakening conflict detection. Before and after each item, record the
same workload and toolchain in the PR description. Do not add a cache,
dependency, or background task without a bound and an invalidation rule.

### P0 — efficient HackMD API communication

- [ ] Run the read-only validator probe with a dedicated token and team path:

  ```sh
  HACKMD_RUN_LIVE_READONLY_TESTS=1 HACKMD_LIVE_TEST_TOKEN=... \
      HACKMD_LIVE_TEST_TEAM_PATH=... \
      cargo test --test live-readonly -- --ignored --nocapture
  ```

  Copy its personal/team note GET/list `ETag`, `Last-Modified`, and conditional
  statuses into `DONE.md` with the date. Implement
  `If-None-Match` or `If-Modified-Since` only after a live `304` is demonstrated;
  otherwise record the cache as inapplicable and retain fresh body GETs.
- [ ] Add a benchmark fixture with 10,000 note summaries and concurrent callers.
  Track list-cache hit latency, miss coalescing, filtering/sorting time, request
  count, and allocations. Set regression thresholds only after three stable CI
  baselines; do not use wall-clock assertions in unit tests.

### P1 — cached tracking and differential note operations

- [ ] After the P0 live-header task demonstrates a usable remote validator, add
  a bounded `NoteSnapshotCache` keyed by `(Workspace, internal_id)`. Store only
  content, content hash, validator, remote timestamp, insertion time, and a
  generation. Configure both TTL and total content bytes; use LRU eviction and
  single-flight fills. Zero-byte capacity must disable it cleanly. If HackMD
  offers no validator, record the cache as inapplicable and retain one fresh
  GET per correctness-sensitive operation; never serve TTL-only note content.
- [ ] Before enabling the snapshot cache, define invalidation: any note write
  evicts that note before network I/O; delete/restore and workspace-changing
  writes also invalidate relevant list entries; pull/push may populate a
  snapshot only from a successful readback; stale in-flight fills must lose to
  a newer generation. Add race tests for all four cases.

### P2 — consolidate test coverage

- [ ] Replace repeated ad-hoc HTTP response tuples with a scenario builder that
  declares expected method, encoded path, selected headers, body predicate,
  response, and optional delay. Keep accepted sockets explicitly blocking and
  enforce one overall fixture deadline on every platform.
- [ ] Convert duplicated personal/team and status-code tests into table-driven
  cases. Retain separate tests only where route shape, permissions, or API
  behavior genuinely differs. Test names must describe the invariant rather
  than the implementation function.
- [ ] Move cross-module workflow coverage to integration tests through the MCP
  transport: resolve/get/edit, pull/check/push/conflict, folder placement, and
  delete/restore. Unit tests should own parsers, validation boundaries, cache
  races, state recovery, and pure sync classification; avoid asserting the same
  behavior at three layers.
- [ ] Add a coverage report in CI and ratchet changed-line coverage after the
  initial baseline. Exclude generated macro code, but do not exclude error,
  recovery, cache-eviction, or platform-specific branches. Coverage tooling
  must remain CI-only rather than a runtime dependency.

### P3 — reduce memory consumption

- [ ] Establish reproducible peak-RSS and allocation baselines for startup,
  listing 10,000 notes, a 10 MiB pull, safe push, and a three-way conflict. Run
  release builds with default features and with `otel`; report both separately.
- [ ] Enforce byte-based limits on every cache, not just entry counts. Account
  for note bodies by capacity, cap workspace-summary caches independently, and
  expose hit/miss/eviction counters without note titles, bodies, paths, or token
  data.
- [ ] Review long-lived `String`, `Vec`, and `Arc` fields with a heap profiler.
  Change representation only where the baseline shows retained memory; avoid
  speculative `Box<str>`/`Arc<str>` churn that merely moves allocations.

### P4 — eliminate unnecessary Rust dependencies

- [ ] Add `cargo machete` (or an equivalent unused-direct-dependency check) to
  CI and run `cargo tree -d` on dependency updates. Keep RustSec auditing. Pin
  the CI tool version so a new lint cannot break `main` without review.
- [ ] For every proposed removal, capture `cargo tree`, clean build time, release
  binary size, and test results before and after. Reject dependency churn that
  only replaces one direct crate with an equal or larger transitive graph.

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
