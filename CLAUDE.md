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

Live suite (`tests/live-smoke.rs`) is destructive and `#[ignore]`d. It needs a dedicated
throwaway account and the environment gates named at the top of that file; do not copy the
command here, it drifts.

`otel` is the only non-default feature; it swaps in an OTLP span exporter, gated at runtime
by `HACKMD_MCP_OTEL=1`.

## Architecture

Three layers, each with its own error enum, converted at the boundary:

1. `client.rs` speaks HTTP. It owns URL segment encoding, bearer auth, empty 202/204 bodies,
   retry/backoff, and the mapping from status codes to `HackmdError` variants whose messages
   name the fix, not just the code. Nothing above it touches `reqwest`.
2. Per-tool modules hold the input struct, the output struct, the domain error, and the tests
   for all of them. `src/note/` covers a single note or the note list (`crud`, `get`, `edit`,
   `list`, `history`, `trash`, `image`, `patch`, `reference`); `src/sync/` covers the
   local-first sync tools and their state store (`pull`, `push`, `check`, `snapshot`,
   `state`); `folders.rs` stands alone. One tool family per file; `server.rs` only dispatches.
3. `server.rs` declares every `#[tool]` with its RMCP annotations and converts results through
   `reply::{success, error}`. Handler bodies stay at match-and-format length.

Filenames carry no underscores: that is why related tools are grouped into directories rather
than named `pull_note.rs`.

Cross-cutting pieces:

- `models::Workspace` is an internally tagged enum (`personal` / `team{team_path}`). One tool
  family serves both route shapes; the personal-vs-team split is a single `match` on segments
  inside the client. Do not add parallel team tools.
- `note::reference::resolve_note_ref` accepts an internal ID or an `https://hackmd.io/@owner/slug`
  URL and returns `NoteResolution::{Resolved, NotFound, Ambiguous}`. Non-resolution is a
  successful tool result carrying candidates, not a tool error, so the agent can pick. That is
  why write paths return `Result<Result<Output, NoteResolution>, Error>`.
- `note::get::patch_path` mints `notes/{id}.md` or `teams/{team_path}/notes/{id}.md`
  deliberately unencoded. `hackmd_edit_note` refuses any patch whose `*** Update File:` header
  does not match exactly, so a patch written against one note can never land on another.
- `note::patch` implements the `*** Begin Patch` format directly. Hunk context must match exactly
  once; ambiguous or missing context is an error rather than a guess.
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
  summary and snapshot path) rather than overwriting. `overwrite` requires `confirm: true`.
- `retry.rs` uses a `tokio::task_local` so client-layer retries surface in the MCP
  result `_meta.retry` without threading a counter through every signature.
- `paging.rs` owns the limit/offset contract for every list tool: `HackMD` returns whole
  collections, so filtering, sorting, and paging all happen locally.
- `client::poll_readback` absorbs `HackMD`'s asynchronous write visibility. Any read-back after
  a write goes through it rather than trusting a single immediate GET.
- `local::LocalFiles` owns everything on this machine: the `StateStore` and the optional
  `HACKMD_MCP_WORKSPACE_ROOT` confinement. The server constructs it and passes it to the
  five tools that touch local paths; `HackmdClient` is HTTP only and knows nothing about
  the filesystem. Any new tool that accepts a caller-supplied path calls `files.allow`
  before doing anything else.
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
