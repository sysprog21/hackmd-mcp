# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```sh
make check                                        # cargo test --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
make indent                                       # rustfmt, then commentflow and shfmt if installed
cargo test edit_conflict_is_a_tool_error          # single test by name substring
cargo test --lib note::patch                      # one module's tests
cargo test --test stdio                           # one integration target
```

`make check` covers the tests only; clippy is a separate gate and both must pass. `make` alone
builds the release binary and `make clean` removes `target/`.

The live suites (`tests/live-readonly.rs`, `tests/live-destructive.rs`) are `#[ignore]`d. The
destructive one needs a dedicated throwaway account and the environment gates named at the top
of that file; do not copy the command here, it drifts.

`otel` is the only non-default feature; it swaps in an OTLP span exporter, gated at runtime
by `HACKMD_MCP_OTEL=1`.

## Architecture

Three layers, each with its own error enum, converted at the boundary:

1. `client.rs` speaks HTTP. It owns URL segment encoding, bearer auth, empty 202/204 bodies,
   retry/backoff, and the mapping from status codes to `HackmdError` variants whose messages
   name the fix, not just the code. Nothing above it touches `reqwest`.
2. Per-tool modules hold the input struct, the output struct, the domain error, and the tests
   for all of them. `src/note/` covers a single note or the note list (`crud`, `get`, `edit`,
   `list`, `image`, `patch`, `reference`); `src/sync/` covers the local-first sync
   tools and their state store (`pull`, `push`, `check`, `tracking`, `state`); `folders.rs`
   stands alone. One tool family per file; `server.rs` only dispatches.
3. `src/server/{account,note,folder,sync}.rs` declare the `#[tool]`s, one named router per
   family, combined by `HackmdServer::router`. `server.rs` itself holds only the wiring:
   construction and the `ServerHandler` impl; the shared reply helpers live in `reply.rs`.
   Each tool carries its RMCP annotations and converts results through `reply::respond`
   (or `respond_resolved` when a `note_ref` may not resolve), so a handler body is one call
   and its summary. A success carries a one-line summary, the
   output as JSON text, and the same output as flat `structuredContent` (no wrapper key), so a
   client that reads only `content` still sees the data. A failure carries its message and
   `_meta.error_kind`, a stable `reply::ErrorKind` name: every domain error enum implements
   `reply::ToolError` with an exhaustive `match`, so a new variant cannot compile unclassified.
   Kind names are a contract; add, never rename.
4. The surface is 14 tools, kept small on purpose. Lists other than a workspace's are
   `source` values on `hackmd_list_notes` (`history`, `trash`, and the local `tracked`
   records); teams come back with `hackmd_get_me`; `hackmd_get_note` with a `local_path`
   reports sync state; a `patch` on `hackmd_update_note` is the body edit; `restore: true`
   on `hackmd_delete_note` undoes a personal deletion; `child_order` on
   `hackmd_update_folder` sets folder order; and a conflicted push writes its own
   `*.remote.md` snapshot. Prefer a parameter on an existing tool over a new tool with the
   same shape. A merged tool carries the more cautious MCP hints of the tools it absorbed,
   and a mode parameter that makes others meaningless refuses them rather than ignoring
   them.

Filenames carry no underscores: that is why related tools are grouped into directories rather
than named `pull_note.rs`.

Cross-cutting pieces:

- `models::Workspace` is an internally tagged enum (`personal` / `team{team_path}`). One tool
  family serves both route shapes; the personal-vs-team split is a single `match` on segments
  inside the client. Do not add parallel team tools. On the wire it is a flat, nullable team
  path: tools take it as `team_path` (the older tagged `workspace` object is still read) and
  report it the same way. `Workspace`'s own serde and schema impls carry that; only sync
  state keeps writing the tagged form, via `#[serde(with = "crate::models::tagged")]`, so
  sidecars stay readable by older builds.
- Tool output is snake_case throughout, and names a workspace `team_path`, `null` for
  personal. Response DTOs read `HackMD`'s camelCase and write
  snake_case (`rename_all(deserialize = "camelCase", serialize = "snake_case")`); request
  DTOs stay camelCase because the API reads them.
- `note::reference::resolve_note_ref` accepts an internal ID or an `https://hackmd.io/@owner/slug`
  URL and returns `NoteResolution::{Resolved, NotFound, Ambiguous}`. Non-resolution is a
  successful tool result carrying candidates, not a tool error, so the agent can pick. That is
  why write paths return `Result<Result<Output, NoteResolution>, Error>`.
- `note::patch::patch_path` mints `notes/{id}.md` or `teams/{team_path}/notes/{id}.md`
  deliberately unencoded. A `hackmd_update_note` patch is refused unless its
  `*** Update File:` header matches exactly, so a patch written against one note can never
  land on another. `hackmd_get_note` reports a `body_hash`; passed back as `expected_hash`
  with a patch or `content`, it refuses the write if the body changed since. `HackMD` has no
  conditional write, so a change landing between that check and the PATCH still wins; the
  hash catches every change made before the check, which is where a stale agent's edits
  come from. A hunk closed by `*** End of File` must match the end of the body.
- `note::patch` implements the `*** Begin Patch` format directly. Hunk context must match exactly
  once; ambiguous or missing context is an error rather than a guess. Text after `@@` is an
  anchor that must match exactly one line; the hunk is then matched only from that line on,
  and an anchored addition-only hunk goes directly below it.
- `sync::state` keeps a `by-path/<hash>` pointer from each tracked file to its sidecar, so a
  lookup is one read rather than a scan. It is a hint only: the loader verifies what it finds
  and falls back to scanning, which is also what keeps state from older builds loadable.
  One file has at most one record: `persist_from_sync` removes any other note's record for the
  same file before writing, and a scan that finds two refuses rather than picking one, because
  the loser would push this file's body to the wrong note. Keys escape `-` so the `--`
  separator is unambiguous; records under the older hyphen-bare key are still found, checked
  against their own identity, and moved on the next sync of that note.
- `sync::state` is the local sync store under `HACKMD_MCP_STATE_DIR`. Per tracked note it keeps a
  JSON sidecar plus a baseline copy of the body. State files go through `write_private_atomic`
  (0600 on unix); the user's own Markdown file goes through `write_local_atomic`, which
  preserves whatever permissions it already had. Lookup is by canonicalized local path, and the
  baseline is verified against the hash in its sidecar on load, so a torn pair is refused rather
  than silently poisoning every later three-way comparison. Build tracked state only through
  `TrackedNoteState::capture`. Only pull/push write state; startup never does. Tokens never go
  in it.
- Sync is baseline three-way: `sync::check` compares local, baseline, and remote to yield
  `in_sync | local_changed | remote_changed | conflict`. `sync::push` in `safe` strategy
  re-reads the remote right before writing and downgrades to a conflict result (with a diff
  summary and a freshly written `*.remote.md` snapshot) rather than overwriting. `overwrite`
  requires `confirm: true`. A merged push passes the conflict's `remote_body_hash` back as
  `expected_remote_hash`, which stands in for the baseline only while the remote still has that
  exact body. Push takes its target from the sidecar; `note_ref` is only an
  optional cross-check. The sidecar records the hash of the `*.remote.md` a conflict last
  wrote (`remote_snapshot_hash`), and a later conflict replaces that file only while it still
  has exactly that content, so a user's own file is never overwritten. The hash is saved with
  `StateStore::update_sidecar` (the baseline is not rewritten), and `persist_from_sync`
  carries it forward for the same note and file. Safe push always classifies against the baseline first; `expected_remote_hash`
  only lets a genuine two-sided conflict through, never a remote-only change. Pull checks
  its destination again after fetching, just before the write. `sync::pull` refuses non-`.md` destinations and will not overwrite a
  tracked file whose edits were never pushed unless `discard_local_changes: true`.
- `retry.rs` uses a `tokio::task_local` so client-layer retries surface in the MCP
  result `_meta.retry` without threading a counter through every signature.
- `client/cache.rs` holds both client caches. `NotesCache` keeps a workspace's note list for 60 seconds and drops every entry on
  any note write. This reverses the deferral recorded in commit `0a61e3b`: URL-based note
  references list the whole workspace, so an agent working through links paid for the same
  list repeatedly. `AccountCache` keeps `userPath` and the team list for the same TTL,
  so an `@owner/slug` lookup does not also pay two discovery requests; `refresh` bypasses
  both. Tests disable them (`Config::for_loopback_test` sets a zero TTL) so their request
  counts stay meaningful.
- `paging.rs` owns the limit/offset contract for every list tool: `HackMD` returns whole
  collections, so filtering, sorting, and paging all happen locally.
- `HackmdClient::poll_readback` (its polling policy in `client/readback.rs`) absorbs `HackMD`'s
  asynchronous write visibility. Any read-back after a write goes through it rather than trusting a single
  immediate GET; it takes the written body's size, and its window grows with it. Folder order
  is a whole-map PUT with no conditional write, so `update_folder`'s `child_order` reads it back and reports
  an order another client overwrote rather than claiming success. `client/error.rs` holds
  `HackmdError` and the status-to-error mapping, including redaction.
- `local::LocalFiles` owns everything on this machine: the `StateStore` and the optional
  `HACKMD_MCP_WORKSPACE_ROOT` confinement. A root from a working-directory `.env` is honored,
  so a user who confines the server there stays confined, but it is not trusted
  (`Config::workspace_root_trusted`): that file may be someone else's, with a root of `/`,
  so the instruction-file refusal stays on beneath it (`LocalFiles::guarding_instructions`).
  The root is canonicalized and opened once at startup, and both the policy check and every capability operation use that one handle; a
  root moved or replaced afterwards is refused until restart. The
  server passes it to the tools that touch local paths or sync state; `HackmdClient` is HTTP only and
  knows nothing about the filesystem. Any new tool that accepts a caller-supplied path calls
  `files.allow` before doing anything else, or `files.allow_write` when it will write there:
  with no root, or a root from `.env`, that also refuses files agents load as instructions (`CLAUDE.md`,
  `AGENTS.md`, `SKILL.md`, `.claude/`, `.github/`, ...). The blocking `LocalFiles` and
  `StateStore` methods run through `local::offload` themselves; a handler only wraps other
  heavy work, such as hashing a body.
- `observability.rs` sends JSON tracing to stderr only. Stdout belongs to the MCP transport;
  a stray `println!` corrupts the protocol and `tests/stdio.rs` will catch it.

## Contracts learned from the live API

TODO.md's "Verified API facts" section is the source of truth, each line labeled measured or
documented with its evidence. Re-check it before changing a request shape. The ones that bite:

- `commentPermission` and `suggestEditPermission` are create-only. PATCH silently ignores them,
  so `update_note` rejects them before the network.
- `writePermission` may not exceed `readPermission`. Validated pre-flight.
- `POST /folders` rejects `"parentFolderId": null`; omit the field for a root folder.
- Folder moves report success while doing nothing, so `update_folder` rejects them outright.
  Personal folder PATCH is not exposed at all.
- List endpoints omit `content` and `folderPaths`; folder membership only appears on a
  single-note read as an ancestor array.
- Writes that HackMD applies asynchronously are read back and polled; a mismatch becomes a
  `ReadbackMismatch` error instead of a false success.

## Test conventions

- `fixture::SequenceServer` is the test double: hand it an array of `(status, body)` or
  `(status, body, headers)` and it replays them in order on a loopback port, then `finish()`
  returns the captured raw requests so assertions can check method, path, and payload.
- `Config::for_loopback_test*` constructors are the only way to allow plain HTTP. Production
  config rejects non-HTTPS `HACKMD_API_URL`.
- Tests live next to the code in `#[cfg(test)] mod tests`. `tests/stdio.rs` covers process-level
  behavior: stdout purity, missing-token startup, `.env` loading and its API-URL guard, state
  directory handling. It closes the child's stdin to stop it, never kills it, so startup
  diagnostics are always collected.
- Assert on the exact user-visible error string when the message is the contract.

## Constraints

- `unsafe_code = "forbid"`; clippy `pedantic` is on. Silence a lint only with
  `#[allow(..., reason = "...")]` carrying a real reason.
- Stdio is the only transport by design. Remote HTTP and GitHub sync were rejected, with the
  reasoning in README.md; do not add either without revisiting that.
- Never log, echo, or return the API token. `SecretToken` has a redacting `Debug`, and upstream
  error bodies are truncated with the token replaced before they reach a caller.
- Filenames contain no underscores.
