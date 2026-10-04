"""Install hackmd-mcp into $BINDIR and register it with Claude Code and Codex.

usage: install.py install <cargo build command...>
       install.py register

Runs on Python 3.9, which is what macOS ships; only reading a Codex config
needs 3.11, for tomllib.
"""

import contextlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile


def bindir():
    # Absolute so the registered command does not depend on the client's
    # working directory. zsh leaves the ~ in `make install BINDIR=~/bin`
    # unexpanded, which would otherwise install under the repository.
    return Path(os.environ["BINDIR"]).expanduser().absolute()


def build(command):
    # json-render-diagnostics, not json: plain json sends rustc's errors to
    # stdout as JSON, where nobody would see them.
    cargo = subprocess.run(
        [*command, "--message-format=json-render-diagnostics"],
        stdout=subprocess.PIPE, text=True,
    )
    if cargo.returncode:
        raise SystemExit(cargo.returncode)
    executable = None
    for line in cargo.stdout.splitlines():
        message = json.loads(line)
        if (
            message.get("reason") == "compiler-artifact"
            and message["target"]["name"] == "hackmd-mcp"
            and "bin" in message["target"]["kind"]
            and message.get("executable")
        ):
            executable = message["executable"]
    if executable is None:
        raise SystemExit("error: Cargo did not report a hackmd-mcp executable")
    return executable


def install(command):
    executable = build(command)
    target = bindir() / "hackmd-mcp"
    target.parent.mkdir(parents=True, exist_ok=True)
    # Copying over the installed binary in place breaks it on macOS: the
    # kernel keeps the code-signature blob of an image that has already run on
    # that vnode, so the new bytes fail validation and every exec dies with
    # SIGKILL. Stage under a temp name in the same directory, ad-hoc sign it,
    # prove it runs, then rename over the target so it gets a fresh inode.
    fd, staged = tempfile.mkstemp(prefix="hackmd-mcp.", dir=target.parent)
    try:
        with os.fdopen(fd, "wb") as out, open(executable, "rb") as built:
            shutil.copyfileobj(built, out)
        os.chmod(staged, 0o755)
        if shutil.which("codesign"):
            subprocess.run(["codesign", "-f", "-s", "-", staged], check=True)
        if subprocess.run([staged, "--version"], stdout=subprocess.DEVNULL).returncode:
            raise SystemExit("error: built binary failed to run, not installing")
        try:
            os.replace(staged, target)
        except OSError as error:
            raise SystemExit(f"error: cannot install {target}: {error.strerror}") from None
    finally:
        with contextlib.suppress(FileNotFoundError):
            os.unlink(staged)
    print(f"installed {target}")
    register()


# An existing entry in either client is left as is: it may carry env or args
# the user added. Neither entry carries the token; the server reads it from
# the environment the agent was launched in (docs/clients.md).
def register():
    command = str(bindir() / "hackmd-mcp")
    register_claude(command)
    register_codex(command)


def register_claude(command):
    if shutil.which("claude") is None:
        print("register: claude not found, skipping")
        return
    # From /, not the repository: get also reports local and project scope
    # entries for its working directory, and a development entry for this
    # checkout would otherwise stand in for the user scope one never added.
    present = subprocess.run(
        ["claude", "mcp", "get", "hackmd"], cwd="/",
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    if present.returncode == 0:
        print("register: Claude Code already has hackmd, left as is")
        return
    added = subprocess.run(["claude", "mcp", "add", "--scope", "user", "hackmd", "--", command])
    if added.returncode:
        raise SystemExit("error: claude mcp add failed")
    print("registered with Claude Code")


# Appended rather than written through `codex mcp add`, whose --env stores
# values: forwarding the token by name needs env_vars, which only the file has.
def register_codex(command):
    # Codex reads $CODEX_HOME/config.toml, defaulting to ~/.codex.
    config = Path(os.environ.get("CODEX_HOME") or Path.home() / ".codex") / "config.toml"
    if not config.is_file():
        print(f"register: no {config}, skipping")
        return
    try:
        import tomllib
    except ModuleNotFoundError:
        raise SystemExit(
            f"error: {config} found, but reading it needs Python 3.11+; "
            "rerun with PYTHON=/path/to/python3.11 or add [mcp_servers.hackmd] by hand"
        ) from None
    # TOML is UTF-8 by definition, so a decoding failure is invalid TOML too.
    try:
        original = config.read_bytes().decode("utf-8")
        servers = tomllib.loads(original).get("mcp_servers", {})
    except (UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
        raise SystemExit(f"error: {config} is not valid TOML, left as is: {error}") from None
    if not isinstance(servers, dict):
        raise SystemExit("error: mcp_servers must be a TOML table")
    if "hackmd" in servers:
        print("register: codex already has [mcp_servers.hackmd], left as is")
        return
    # JSON basic strings also work in TOML; escape DEL, which TOML forbids raw.
    quoted = json.dumps(command, ensure_ascii=False).replace("\x7f", "\\u007f")
    entry = (
        f"\n[mcp_servers.hackmd]\ncommand = {quoted}\n"
        'env_vars = ["HACKMD_API_TOKEN", "HACKMD_MCP_WORKSPACE_ROOT"]\n'
    )
    # Validate the combined document before touching the original bytes.
    try:
        tomllib.loads(original + entry)
    except tomllib.TOMLDecodeError as error:
        raise SystemExit(
            f"error: [mcp_servers.hackmd] cannot be appended to {config}: {error}"
        ) from None
    with config.open("a", encoding="utf-8", newline="") as destination:
        destination.write(entry)
    print("registered with codex")


if __name__ == "__main__":
    # SIGTERM and SIGHUP end the run through the finally above, as Ctrl-C
    # already does, so no staged file is left in the bin directory.
    for signum in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, lambda *_: sys.exit(1))
    if sys.argv[1:2] == ["install"] and len(sys.argv) > 2:
        install(sys.argv[2:])
    elif sys.argv[1:] == ["register"]:
        register()
    else:
        raise SystemExit(__doc__)
