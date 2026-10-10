# hackmd-mcp

An [MCP](https://modelcontextprotocol.io/) server written in Rust and released
under the [MIT license](LICENSE). It lets an AI agent read, edit, and organize
[HackMD](https://hackmd.io/) notes, and sync them with local Markdown files.
It runs on your machine, communicates with the agent over standard input and
output (stdio), and accesses HackMD with an API token.

## Why this server exists

HackMD already provides an [official hosted MCP
server](https://hackmd.io/@docs/hackmd-mcp-setup), with OAuth sign-in and tools
for personal and team notes. It is a convenient choice for accessing HackMD
from an agent without installing a local server.

This project serves workflows where documents need the same care as code:
small, reviewable edits, checks for collaborators' changes, local tooling,
and confirmation that a write took effect. These matter when an agent edits
a lecture handout, a shared meeting record, or a technical procedure that
others are still updating.

### Controlled edits for shared documents

Fixing one command in a long handout should preserve the surrounding examples
and explanations. Asking a model to regenerate the entire document can omit
unrelated content; writing an older copy can overwrite a collaborator's work.

As of October 2026, the official setup guide lists `content` as the body
input for both note update tools and describes the personal update as a full
overwrite. The guide does not list patch inputs, expected-content hashes, or a
local sync workflow. This comparison concerns the documented tool interface;
it does not assume how the hosted server handles writes internally.

`hackmd-mcp` makes the intended edit checkable:

- A patch must name the exact target note, and every hunk's context must match
  the current body exactly once (after its `@@` anchor, if any). Missing or
  ambiguous context stops the edit.
- The agent can pass the `body_hash` from its read as `expected_hash`. If the
  body has changed before the check, the server refuses the write so the agent
  can read again and revise its edit.

Patches are checked locally before the resulting body is sent to HackMD;
this is not an atomic patch API. Hash checks are optional, and an edit landing
between the check and the write can still be overwritten. `content` replaces
the whole body instead. See [patch editing](docs/tools.md#editing-a-note).

### Local sync for development and review

Pull a note into a Markdown file, edit it with your editor, search it, run
checks, or review its diff in Git, then push it back. A handout's code,
commands, and experiment steps can be maintained alongside the programs they
describe, while collaborators continue using HackMD.

The server saves a sync baseline and compares local and remote content with
it. With the default safe strategy, conflicting changes stop the push and
produce a diff and a remote snapshot for merging. After merging, the agent
passes the conflict's `remote_body_hash` back as `expected_remote_hash`, so the
push proceeds only if the remote has not changed again. This gives the
workflow a defined conflict-resolution step. See
[pull, edit, and push](docs/tools.md#pull-edit-locally-push).

### Confirmation before another write

A successful request does not always mean the new state is visible yet.
Body edits, folder updates, and a new note's folder placement are read back to
confirm their results. When a dropped connection, a 5xx, or an unreadable
success reply leaves a write's outcome uncertain, the tool reports it as
unconfirmed and tells the agent to look, not retry. This helps avoid duplicate
notes or another overwrite. See
[confirmation details](docs/tools.md#editing-a-note) for operation-specific
limits and the image-upload exception.

### Local deployment with explicit boundaries

The server runs as a local stdio process and opens no network listening port.
You choose the binary version and can confine local file operations to a
workspace root.

Notes still live on HackMD, and the agent may send their content to your
chosen model service. Local execution gives you deployment and file-access
control; the workflow still depends on those services.

Choose the official hosted server for convenient remote access with OAuth.
Choose `hackmd-mcp` when your workflow needs context-checked edits, local
Markdown sync, conflict handling, and explicit write confirmation.

## Quick start

### 1. Download a prebuilt binary

Download from [GitHub Releases](https://github.com/sysprog21/hackmd-mcp/releases);
no Rust installation is needed. The rolling `latest` release is updated after
CI passes on `main`.

| Platform | Download |
|----------|----------|
| Linux x86_64 (glibc 2.17+) | [`.tar.gz`](https://github.com/sysprog21/hackmd-mcp/releases/download/latest/hackmd-mcp-x86_64-unknown-linux-gnu.tar.gz) |
| macOS Apple silicon | [`.tar.gz`](https://github.com/sysprog21/hackmd-mcp/releases/download/latest/hackmd-mcp-aarch64-apple-darwin.tar.gz) |
| Windows x86_64 | [`.zip`](https://github.com/sysprog21/hackmd-mcp/releases/download/latest/hackmd-mcp-x86_64-pc-windows-msvc.zip) |

On Linux or macOS, the commands below download, verify, and install the binary
into `~/.local/bin`. On macOS, change `asset` to
`hackmd-mcp-aarch64-apple-darwin.tar.gz` and use `shasum -a 256 -c -` in place
of `sha256sum -c -`.

```sh
asset=hackmd-mcp-x86_64-unknown-linux-gnu.tar.gz
base=https://github.com/sysprog21/hackmd-mcp/releases/download/latest
curl -fLO "$base/$asset" &&
    curl -fLO "$base/SHA256SUMS" &&
    grep " $asset\$" SHA256SUMS | sha256sum -c - &&
    tar xzf "$asset" &&
    mkdir -p ~/.local/bin &&
    install -m 755 hackmd-mcp ~/.local/bin/
```

On Windows, the PowerShell commands below download the ZIP and print `True`
when its checksum matches; then extract `hackmd-mcp.exe` to a permanent
location.

```powershell
$zip = 'hackmd-mcp-x86_64-pc-windows-msvc.zip'
$base = 'https://github.com/sysprog21/hackmd-mcp/releases/download/latest'
Invoke-WebRequest "$base/$zip" -OutFile $zip -UseBasicParsing
Invoke-WebRequest "$base/SHA256SUMS" -OutFile SHA256SUMS -UseBasicParsing
$want = ((Select-String -SimpleMatch " $zip" SHA256SUMS).Line -split ' ')[0]
(Get-FileHash $zip -Algorithm SHA256).Hash -eq $want
```

Use the full path to `hackmd-mcp.exe` in your
[client configuration](docs/clients.md); the examples below use the
Linux/macOS install path.

<details>
<summary>Verify build provenance or build from source</summary>

Checksums detect corrupt downloads. To also verify the archive came from this
repository's CI on `main`, use the GitHub CLI (on Windows, replace `$asset`
with the ZIP's name):

```sh
gh attestation verify "$asset" --repo sysprog21/hackmd-mcp \
    --source-ref refs/heads/main \
    --signer-workflow sysprog21/hackmd-mcp/.github/workflows/ci.yml
```

For other platforms, build with Rust 1.88 or newer:

```sh
cargo install --git https://github.com/sysprog21/hackmd-mcp --locked
```

Cargo installs into `~/.cargo/bin` by default, so replace `~/.local/bin` with
`~/.cargo/bin` in the steps below.
See [development](docs/development.md) for building a checkout.

</details>

### 2. Set up the environment

Create an API token under HackMD's Settings, API, and pick a directory for
notes you pull to disk. Put both in the environment your agent is launched
from, such as your shell profile:

```sh
export HACKMD_API_TOKEN=...
export HACKMD_MCP_WORKSPACE_ROOT=$HOME/notes
mkdir -p "$HACKMD_MCP_WORKSPACE_ROOT"
```

The workspace root is optional but recommended: it confines every local file
operation and enables uploading local images. The server reads both at startup, so
restart your agent after changing them. Keep the token out of chat, logs, and
shared config files; [docs/configuration.md](docs/configuration.md) has the
details.

### 3. Check the setup

```sh
~/.local/bin/hackmd-mcp --self-check --probe-api
```

The check reports configuration and API access errors without printing the
token. After updating the binary or environment, restart your agent to load
the changes. See [self-check details](docs/configuration.md#self-check).

### 4. Connect your agent

Claude Code:

```sh
claude mcp add --scope user hackmd -- ~/.local/bin/hackmd-mcp
```

Claude Desktop, Codex, and other clients need a few more lines, mostly to pass
the environment through; see [docs/clients.md](docs/clients.md). Then ask your
agent something like "list my recent HackMD notes".

## What the agent can do

Fourteen tools cover account access, notes, folders, and local sync, kept few
on purpose so they cost the agent little context:

| Area | Tools |
|------|-------|
| Account | `hackmd_get_me` (profile and teams) |
| Notes | `hackmd_list_notes`, `hackmd_get_note`, `hackmd_create_note`, `hackmd_update_note`, `hackmd_delete_note`, `hackmd_upload_note_image` |
| Folders | `hackmd_list_folders`, `hackmd_create_folder`, `hackmd_update_folder`, `hackmd_delete_folder` |
| Local sync | `hackmd_pull_note`, `hackmd_push_note`, `hackmd_untrack_note` |

[docs/tools.md](docs/tools.md) walks through the editing and sync workflows.

## Documentation

- [docs/configuration.md](docs/configuration.md): environment variables, token
  handling, state directory, workspace root, self-check, logging.
- [docs/clients.md](docs/clients.md): wiring the server into Claude Code,
  Claude Desktop, Codex, and other MCP clients.
- [docs/tools.md](docs/tools.md): the tools, patch editing, and the
  pull/edit/push cycle with conflict handling.
- [docs/development.md](docs/development.md): building, testing, the live API
  suites, and why the server is stdio only.

## License

MIT. See [LICENSE](LICENSE).
