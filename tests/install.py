"""Installer regressions: run with python3 tests/install.py."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import tomllib


ROOT = Path(__file__).resolve().parents[1]
GOOD = "echo freshly-built\n"


def script(path, body):
    path.write_text("#!/bin/sh\n" + body)
    path.chmod(0o755)


with tempfile.TemporaryDirectory() as temporary:
    home = Path(temporary)
    config = home / ".codex/config.toml"
    config.parent.mkdir()
    tools = home / "tools"
    tools.mkdir()
    binary = home / "custom target/release/hackmd-mcp"
    binary.parent.mkdir(parents=True)
    script(binary, GOOD)
    # Cargo reports the executable wherever its target directory is.
    artifact = {"reason": "compiler-artifact", "executable": str(binary),
                "target": {"name": "hackmd-mcp", "kind": ["bin"]}}
    script(tools / "cargo", f"cat <<'JSON'\n{json.dumps(artifact)}\nJSON\n"
           'exit "${BUILD_STATUS:-0}"\n')
    script(tools / "codesign", "exit 0\n")
    # Logs every call; "mcp get" succeeds only while the marker file exists.
    claude_log = home / "claude.log"
    claude_has = home / "claude-has-hackmd"
    script(tools / "claude", f'echo "$*" >>"{claude_log}"\n'
           f'if [ "$1 $2" = "mcp get" ]; then test "$PWD" = / && test -f "{claude_has}"; exit; fi\n')
    # Shadows the stdlib module, as on the macOS system Python 3.9.
    no_tomllib = home / "no-tomllib"
    no_tomllib.mkdir()
    (no_tomllib / "tomllib.py").write_text("raise ModuleNotFoundError('tomllib')\n")

    bindir = home / 'bin "quoted" \\ unicode-😀'
    installed = bindir / "hackmd-mcp"
    env = dict(os.environ, HOME=str(home), PATH=f'{tools}:{os.environ["PATH"]}',
               BINDIR=str(bindir))
    env.pop("CODEX_HOME", None)

    def make(target, *args, **extra):
        return subprocess.run(
            ["make", "--no-print-directory", target, "CARGO=cargo", *args],
            cwd=ROOT, env=env | extra, capture_output=True, text=True,
        )

    # The TOML cases need no build or make, so they call the script directly.
    def register(**extra):
        return subprocess.run(
            [sys.executable, "scripts/install.py", "register"],
            cwd=ROOT, env=env | extra, capture_output=True, text=True,
        )

    for header in (
        "[mcp_servers.hackmd]", "[mcp_servers.'hackmd']",
        '["mcp_servers" . "hackmd"]', "[mcp_servers . hackmd]",
        '[mcp_servers."ha\\u0063kmd"]',
    ):
        original = (header + '\ncommand = "existing"\n').encode()
        config.write_bytes(original)
        result = register()
        assert result.returncode == 0, result.stderr
        assert config.read_bytes() == original

    original = b'# preserved\r\nmodel = "example"\r\n'
    config.write_bytes(original)
    result = make("install")
    assert result.returncode == 0, result.stderr
    assert config.read_bytes().startswith(original)
    data = tomllib.loads(config.read_text())
    assert data["mcp_servers"]["hackmd"]["command"] == str(installed)
    assert installed.read_bytes() == binary.read_bytes()
    before = config.read_bytes()
    assert make("register").returncode == 0
    assert config.read_bytes() == before

    # Claude Code: added when absent, never replaced when present, since an
    # existing entry may carry env or args the user gave it.
    calls = claude_log.read_text().splitlines()
    assert f"mcp add --scope user hackmd -- {installed}" in calls, calls
    assert not any(c.startswith("mcp remove") for c in calls), calls
    claude_log.unlink()
    claude_has.touch()
    assert register().returncode == 0
    assert not any(c.startswith("mcp add") for c in claude_log.read_text().splitlines())
    claude_has.unlink()

    # Header-shaped text in a multiline string is not an existing table.
    config.write_text('description = """\n[mcp_servers.hackmd]\n"""\n')
    assert register().returncode == 0
    assert "hackmd" in tomllib.loads(config.read_text())["mcp_servers"]

    # Relative paths are registered absolute, resolved from the repository.
    for path in ("relative bin", str(home / "controls-\t\n\x7f")):
        config.write_text("")
        result = make("register", BINDIR=path)
        assert result.returncode == 0, result.stderr
        command = tomllib.loads(config.read_text())["mcp_servers"]["hackmd"]["command"]
        assert command == str((ROOT / path) / "hackmd-mcp")

    # A leading dash must not be read as an option anywhere.
    dashed = ROOT / "-bin"
    assert not dashed.exists(), f"{dashed} exists; the test would remove it"
    try:
        result = make("install", BINDIR="-bin")
        assert result.returncode == 0, result.stderr
        assert (dashed / "hackmd-mcp").read_bytes() == binary.read_bytes()
    finally:
        shutil.rmtree(dashed, ignore_errors=True)

    # A directory at the destination is refused, not installed into.
    target = home / "dir-bin"
    (target / "hackmd-mcp").mkdir(parents=True)
    result = make("install", BINDIR=str(target))
    assert result.returncode != 0
    assert "Is a directory" in result.stderr, result.stderr
    assert not any((target / "hackmd-mcp").iterdir())
    assert not list(target.glob("hackmd-mcp.*"))

    # A ~ the shell left unexpanded is the home directory, not a directory
    # named ~ under the repository.
    config.write_text("")
    result = make("register", BINDIR="~/tilde-bin")
    assert result.returncode == 0, result.stderr
    command = tomllib.loads(config.read_text())["mcp_servers"]["hackmd"]["command"]
    assert command == str(home / "tilde-bin/hackmd-mcp"), command

    # CODEX_HOME moves the Codex config, as it does for Codex itself.
    codex_home = home / "codex-home"
    codex_home.mkdir()
    (codex_home / "config.toml").write_text("")
    assert register(CODEX_HOME=str(codex_home)).returncode == 0
    assert "hackmd" in tomllib.loads((codex_home / "config.toml").read_text())["mcp_servers"]

    # Without tomllib, install works when there is no Codex config, and a
    # config that exists is refused with a reason rather than appended to.
    config.unlink()
    result = make("install", PYTHONPATH=str(no_tomllib))
    assert result.returncode == 0, result.stderr
    config.write_text("")
    result = register(PYTHONPATH=str(no_tomllib))
    assert result.returncode != 0
    assert "needs Python 3.11+" in result.stderr, result.stderr
    assert config.read_text() == ""

    # No artifact reported is an error, not an empty install.
    script(tools / "cargo", "exit 0\n")
    assert make("install").returncode != 0
    script(tools / "cargo", f"cat <<'JSON'\n{json.dumps(artifact)}\nJSON\n"
           'exit "${BUILD_STATUS:-0}"\n')

    # Failed builds and failed executable checks must preserve the installed file.
    script(binary, "exit 1\n")
    before = installed.read_bytes()
    assert make("install", BUILD_STATUS="1").returncode != 0
    assert installed.read_bytes() == before
    assert make("install").returncode != 0
    assert installed.read_bytes() == before
    assert not list(bindir.glob("hackmd-mcp.*"))

    for invalid in (b'[broken', b'mcp_servers = "wrong type"',
                    b'mcp_servers = { other = {} }', b'model = "\xff"\n'):
        config.write_bytes(invalid)
        result = register()
        assert result.returncode != 0
        assert "Traceback" not in result.stderr, result.stderr
        assert config.read_bytes() == invalid

print("installer regressions passed")
