# hackmd-mcp

`hackmd-mcp` is a local-first MCP server for HackMD. It communicates over
stdio, writes protocol messages only to stdout, and keeps diagnostics on
stderr.

## Install and run

Build the current checkout:

```sh
cargo install --path . --locked
hackmd-mcp --version
hackmd-mcp --help
```

For a published release, pin the exact version you reviewed instead of
installing from a mutable branch or tag:

```sh
cargo install hackmd-mcp --version 0.1.0 --locked
```

Replace `0.1.0` with the exact release you intend to run.

## Token security

Create a dedicated, least-privilege HackMD API token. Prefer passing it through
the MCP client's inherited environment. Never commit a real token, paste one
into chat or logs, or store one in a world-readable client configuration.

For local development only, copy `.env.example` to `.env` in the server's
working directory and restrict its permissions. Inherited environment values
take precedence over `.env`. The server reads only these keys:

- `HACKMD_API_TOKEN` — required when a tool contacts HackMD.
- `HACKMD_API_URL` — optional HTTPS HackMD Enterprise endpoint; defaults to
  `https://api.hackmd.io/v1`.
- `HACKMD_MCP_STATE_DIR` — optional private sync-state directory.
- `HACKMD_MCP_WORKSPACE_ROOT` — optional tree that `hackmd_pull_note`,
  `hackmd_push_note`, `hackmd_check_note_sync`, `hackmd_save_remote_snapshot`, and
  `hackmd_upload_note_image` are confined to. It must be absolute. Set it and a note
  that tells an agent to access outside that tree is rejected; capability-relative
  operations stay confined if a symlink is swapped concurrently. Unset, any absolute
  path is accepted.

A `.env` in the working directory may not redirect a token that came from the environment: if
`HACKMD_API_TOKEN` is inherited and only that file sets `HACKMD_API_URL`, the server refuses to
start. Otherwise a checked-out repository could point your token at a host of its author's
choosing. A token inside the file does not change this, because an inherited value takes
precedence over it. Set both keys in the same place.

The token is loaded lazily, redacted from diagnostics, and never validated at
startup. Local sync state can contain note content and must also remain private.

## MCP client configuration

Install a pinned binary first, then reference that stable executable. Use an
absolute path when the client's executable search path is uncertain.

Codex reads MCP server entries from `~/.codex/config.toml`:

```toml
[mcp_servers.hackmd]
command = "/absolute/path/to/hackmd-mcp"
```

Launch Codex from an environment containing `HACKMD_API_TOKEN`. See the
[official Codex MCP configuration documentation](https://developers.openai.com/codex/mcp/)
for the current configuration schema.

Claude Desktop uses a JSON MCP server entry:

```json
{
  "mcpServers": {
    "hackmd": {
      "command": "/absolute/path/to/hackmd-mcp",
      "args": []
    }
  }
}
```

If a desktop-launched client does not inherit your shell environment, use a
private `.env` in the configured working directory or the client's supported
secret-management mechanism. Avoid placing the token directly in JSON or TOML.

## Transport scope

This server intentionally supports stdio only. A remote HTTP deployment would
need authentication, per-user token isolation, endpoint allowlisting, rate
limits, and encrypted credential storage with secure rotation. GitHub sync is
also excluded from the core server because it requires separate credentials,
durable conflict state, and frontmatter policy. Both should remain opt-in,
separately reviewed features if a concrete workflow eventually requires them.

## Validation

```sh
make check
cargo clippy --all-targets --all-features -- -D warnings
```

`make check` is `cargo test --all-targets --all-features`. The Makefile also carries `make`
(release build), `make clean`, and `make indent`, which formats the sources with rustfmt plus
`commentflow` and `shfmt` when those are installed.

The destructive live suite in `tests/live-smoke.rs` is ignored by default. Run
it only with a dedicated test account and the explicit environment gates
documented at the top of that file.
