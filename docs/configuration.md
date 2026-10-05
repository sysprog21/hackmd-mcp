# Configuration

All configuration comes from environment variables, read once at startup:
restart the server (usually by restarting the agent) after changing one. The
only command-line flags are `--self-check` with its optional `--probe-api`,
`--version`, and `--help`.

## Environment variables

| Variable | Required | Read from `.env` | Purpose |
|----------|----------|------------------|---------|
| `HACKMD_API_TOKEN` | when a tool contacts HackMD | yes | API token |
| `HACKMD_API_URL` | no | yes, with a restriction | HTTPS API endpoint; defaults to `https://api.hackmd.io/v1`. Note URLs are parsed only for `hackmd.io`; elsewhere, pass note IDs |
| `HACKMD_MCP_WORKSPACE_ROOT` | no, recommended | yes, with a restriction | Absolute directory that local file access is confined to |
| `HACKMD_MCP_STATE_DIR` | no | never | Private directory for sync state |
| `RUST_LOG` | no | no | Log filter for stderr; defaults to `info` |
| `HACKMD_MCP_OTEL` | no | no | `1` exports spans over OTLP; needs a build with `--features otel` |

Values inherited from the environment take precedence over `.env`.

## The API token

Create a dedicated token for this server, so revoking it affects nothing else.
Pass it through the environment the MCP client is launched from. Never commit
it, paste it into a chat, or store it in a client config file other users can
read.

A missing or wrong token does not stop the server from starting; it fails only
the tools that contact HackMD, and `--self-check --probe-api` reports it. The
token is redacted from every log line, error message, and tool result. Error
bodies from HackMD are truncated and scrubbed of the token before they reach
the agent.

## The `.env` file

For local development, copy `.env.example` to `.env` in the server's working
directory and make it readable only by you (`chmod 600 .env`). The server reads
only the keys listed above, and it treats the file as untrusted, because a
`.env` in the working directory may belong to a repository someone else wrote:

- If `HACKMD_API_TOKEN` is inherited and only `.env` sets `HACKMD_API_URL`, the
  server refuses to start. Otherwise a checked-out repository could send your
  token to a host of its author's choosing. Set both in the same place.
- `HACKMD_MCP_STATE_DIR` is ignored in `.env`. That directory holds full note
  bodies, and a stranger's `.env` must not be able to redirect them.
- A `HACKMD_MCP_WORKSPACE_ROOT` from `.env` still confines, but counts as
  untrusted; see [Workspace root](#workspace-root) for what that changes.

## Workspace root

`HACKMD_MCP_WORKSPACE_ROOT` confines every tool that takes a local path:
`hackmd_pull_note`, `hackmd_push_note`, `hackmd_get_note` with `local_path`, and
`hackmd_upload_note_image`. It must be absolute. A path outside the tree is
refused, whatever a note tells the agent, and the confinement holds even if a
symlink inside the tree is swapped while an operation runs. The root is opened
once at startup; on Unix, if it is moved or replaced afterwards, operations are
refused until the server restarts (elsewhere they keep using the original
tree). A root that does not exist yet when the server starts is refused until
a restart on every platform, so create the directory first.

Without a root, any absolute path is accepted. In that case, and under a root
that came only from `.env` (which could be `/`), the server logs a warning at
startup and a pull refuses to write files coding agents load as instructions:
`CLAUDE.md`, `AGENTS.md`, `SKILL.md`, and the like, or anything under
`.claude/`, `.github/`, `.cursor/`, and similar directories. That list is best
effort, not a boundary (`CLAUDE.md` can import any Markdown file), so set a
root if an agent reads the tree you pull into.

`hackmd_upload_note_image` puts a local file behind a HackMD link, public
whenever the note is guest-readable, so it requires a root set in the server's
own environment, not only in `.env`, and refuses every upload otherwise.

## Sync state directory

Pulling a note records a JSON sidecar and an exact copy of the body as the
baseline for later three-way comparison. They live in:

| Platform | Default location |
|----------|------------------|
| Linux | `$XDG_STATE_HOME/hackmd-mcp` (usually `~/.local/state/hackmd-mcp`) |
| macOS | `~/Library/Application Support/hackmd-mcp` |
| Windows | `%LOCALAPPDATA%\hackmd-mcp` |

Override it with `HACKMD_MCP_STATE_DIR` in the inherited environment. State
files are written atomically with mode 0600 on Unix. The server never writes
state at startup, and no tool accepts a destination inside this directory.

On Unix the server refuses a state directory, or a record directory inside it,
that another account owns or that is group- or world-writable: a record planted
there could send a pushed file to someone else's note. ACLs are not inspected,
and other platforms do not check ownership, so keep the directory inside your
own profile.

## Self-check

```sh
hackmd-mcp --self-check
```

Prints a JSON report and exits without starting the MCP transport. It covers the
package version, whether a token is present, the API origin, whether the state
directory is writable, and whether the workspace root is configured and
accessible. Adding `--probe-api` makes one read-only `GET /me`, which reports
only success or a bounded error, never profile data; without it the checks stay
local. A failed check exits nonzero and still prints valid JSON, so editor
integrations can parse it.

## Logging

Diagnostics go to stderr as JSON lines; stdout belongs to the MCP protocol.
Adjust verbosity with `RUST_LOG`, for example `RUST_LOG=hackmd_mcp=debug`. Most
MCP clients save a server's stderr in their own log directory.
