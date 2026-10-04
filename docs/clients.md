# Connecting an MCP client

The client spawns `hackmd-mcp` and speaks MCP over its stdin and stdout, so
there is nothing to start beforehand. Run the binary from a terminal and it
simply waits on stdin.

Point every client at a stable, absolute path to the binary (for example
`~/.local/bin/hackmd-mcp`), not at a build directory.

## Environment inheritance

The server reads its token and settings from its environment (see
[configuration.md](configuration.md)), and clients differ in what they pass on:

| Client | What the server receives |
|--------|--------------------------|
| Claude Code | the environment Claude Code was started in |
| Codex | only variables listed in `env_vars` (or set in `env`) |
| Claude Desktop | the app's environment, which does not include your shell profile |

Prefer passing the token through the environment over writing it into a
client's JSON or TOML, which is easy to share by accident. Claude Desktop is
the exception, since it has no other way in.

## Claude Code

```sh
claude mcp add --scope user hackmd -- ~/.local/bin/hackmd-mcp
```

`--scope user` makes the server available in every project; `--scope project`
records it in the repository's `.mcp.json` instead. Start Claude Code from a
shell where `HACKMD_API_TOKEN` is set, and run `/mcp` inside it to confirm the
server is connected.

## Codex

Add an entry to `~/.codex/config.toml`, naming the variables to forward:

```toml
[mcp_servers.hackmd]
command = "/absolute/path/to/hackmd-mcp"
env_vars = ["HACKMD_API_TOKEN", "HACKMD_MCP_WORKSPACE_ROOT"]
```

Then launch Codex from an environment that has them. The
[Codex MCP documentation](https://learn.chatgpt.com/docs/extend/mcp?surface=cli)
has the current schema.

## Claude Desktop

A desktop app launched from the Dock, Start menu, or a launcher does not see
your shell profile, and it starts servers in an unspecified working directory,
so a `.env` file is no help either. Set the variables in the entry's `env` in
`claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "hackmd": {
      "command": "/absolute/path/to/hackmd-mcp",
      "args": [],
      "env": {
        "HACKMD_API_TOKEN": "...",
        "HACKMD_MCP_WORKSPACE_ROOT": "/absolute/path/to/notes"
      }
    }
  }
}
```

The file now holds the token, so make it readable only by you
(`chmod 600` on macOS and Linux) and leave it out of backups and dotfile
repositories you share.

## Other clients

Any client that supports stdio MCP servers works: give it the binary path, no
arguments, and an environment with the token.

## Troubleshooting

1. Run `hackmd-mcp --self-check --probe-api` from the same environment the
   client uses; see [configuration.md](configuration.md#self-check).
2. If the server connects but every tool reports a missing token, the client
   did not pass your environment through; see
   [Environment inheritance](#environment-inheritance).
3. The server logs to stderr, which clients keep in their MCP logs;
   `RUST_LOG=hackmd_mcp=debug` makes it more verbose.
