# hackmd-mcp

An [MCP](https://modelcontextprotocol.io/) server that lets an AI agent read,
write, and organize your [HackMD](https://hackmd.io/) notes, and keep them in
sync with Markdown files on your own disk.

## Why put an agent on HackMD

HackMD is where meeting notes, lecture handouts, and team specs end up. Working
on them with an agent usually means copying a note into a chat, copying the
answer back, and hoping nobody edited the note in between. This server removes
that loop and the risks that come with it:

- Ask in plain language. "Summarize this week's meeting notes in the `ops`
  team", "fix the broken links in https://hackmd.io/@me/syllabus", or "move the
  action items into a new note under `Projects`". The agent finds notes by ID or
  by the URL you paste, across your personal and team workspaces.
- Edits touch only what they mean to. The agent changes a note with a
  context-checked patch, not by rewriting the whole body, so a typo fix stays a
  typo fix, and it can pass along the hash it read to refuse the write if the
  note changed in between. See [patch editing](docs/tools.md#editing-a-note).
- Notes become local files. Pull a note into a `.md` file, then edit it with
  your editor, grep it, diff it, or commit it to git. Push sends it back, and if
  the note also changed on HackMD the push stops and hands you both versions
  instead of overwriting either. See
  [sync](docs/tools.md#pull-edit-locally-push).
- Edits are confirmed, not assumed. HackMD applies some writes
  asynchronously, so body edits and folder changes are read back before they
  are reported as done. A write whose outcome is unknown is reported as such
  and never retried blindly, so you do not get duplicate notes.
- Local file access can be fenced in. Set a workspace root and every local path
  stays inside it; without one, a pull still refuses to write the files agents
  load as instructions. Your API token never appears in any output. See
  [configuration](docs/configuration.md#workspace-root).

The server runs on your machine as a child process of your agent and talks to
it over stdio. Nothing listens on a network port.

## Quick start

### 1. Install

Every push to `main` that passes CI replaces a rolling [`latest`
release](https://github.com/sysprog21/hackmd-mcp/releases/tag/latest).
Pick the archive for your platform:

| Platform | Archive |
|----------|---------|
| Linux x86_64, glibc 2.17 or newer | `hackmd-mcp-x86_64-unknown-linux-gnu.tar.gz` |
| macOS Apple silicon | `hackmd-mcp-aarch64-apple-darwin.tar.gz` |
| Windows x86_64 | `hackmd-mcp-x86_64-pc-windows-msvc.zip` |

On Linux or macOS, set `asset` to the archive from the table above (where
`sha256sum` is missing, as on older macOS, use `shasum -a 256` in its place):

```sh
asset=hackmd-mcp-x86_64-unknown-linux-gnu.tar.gz
base=https://github.com/sysprog21/hackmd-mcp/releases/download/latest
curl -sSfLO "$base/$asset" -O "$base/SHA256SUMS"
grep " $asset\$" SHA256SUMS | sha256sum -c - &&
    tar xzf "$asset" &&
    mkdir -p ~/.local/bin &&
    install -m 755 hackmd-mcp ~/.local/bin/
```

The checksum catches a corrupt download. To also confirm the archive was built
by this repository's CI from a commit on `main`, run:

```sh
gh attestation verify "$asset" --repo sysprog21/hackmd-mcp \
    --source-ref refs/heads/main \
    --signer-workflow sysprog21/hackmd-mcp/.github/workflows/ci.yml
```

On Windows, download the zip from the release page and extract
`hackmd-mcp.exe`. Other platforms build from source with Rust 1.88 or newer:

```sh
cargo install --git https://github.com/sysprog21/hackmd-mcp --locked
```

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
operation and enables image upload. The server reads both at startup, so
restart your agent after changing them. Keep the token out of chat, logs, and
shared config files; [docs/configuration.md](docs/configuration.md) has the
details.

### 3. Check the setup

```sh
~/.local/bin/hackmd-mcp --self-check --probe-api
```

It prints a JSON report and exits nonzero if anything is wrong, without ever
printing the token.

### 4. Connect your agent

Claude Code:

```sh
claude mcp add --scope user hackmd -- ~/.local/bin/hackmd-mcp
```

Claude Desktop, Codex, and other clients need a few more lines, mostly to pass
the environment through; see [docs/clients.md](docs/clients.md). Then ask your
agent something like "list my recent HackMD notes".

## What the agent can do

Fourteen tools, kept few on purpose so they cost the agent little context:

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
