use std::{
    fs,
    io::Write,
    path::Path,
    process::{Child, Command, Output, Stdio},
    thread,
    time::Duration,
};

/// The binary with none of its settings inherited, run by default from a
/// directory with no `.env`: it loads one from its working directory, and the
/// package root may hold a developer's real one. A test that needs a `.env`
/// sets its own `current_dir`.
fn server() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hackmd-mcp"));
    command
        .current_dir(env!("CARGO_TARGET_TMPDIR"))
        .env_remove("HACKMD_API_TOKEN")
        .env_remove("HACKMD_API_URL")
        .env_remove("HACKMD_MCP_STATE_DIR")
        .env_remove("HACKMD_MCP_WORKSPACE_ROOT")
        .env_remove("HACKMD_MCP_OTEL")
        .env_remove("RUST_LOG");
    command
}

fn spawn_server(
    token: Option<&str>,
    current_dir: Option<&Path>,
    state_dir: Option<&Path>,
) -> Child {
    let mut command = server();
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(token) = token {
        command.env("HACKMD_API_TOKEN", token);
    }
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }
    if let Some(state_dir) = state_dir {
        command.env("HACKMD_MCP_STATE_DIR", state_dir);
    }
    command.spawn().expect("server binary should start")
}

fn exchange_then_stop(mut child: Child, messages: &str) -> Output {
    child
        .stdin
        .as_mut()
        .expect("server stdin should be piped")
        .write_all(messages.as_bytes())
        .expect("protocol messages should be written");
    drop(child.stdin.take());
    child
        .wait_with_output()
        .expect("server output should be collected")
}

fn assert_waiting_then_stop(mut child: Child) -> Output {
    thread::sleep(Duration::from_millis(100));
    assert!(
        child
            .try_wait()
            .expect("server status should be readable")
            .is_none(),
        "server exited while its MCP input remained open"
    );

    // Close the transport instead of killing: the server then shuts down on its
    // own and every startup diagnostic it wrote is guaranteed to be collected,
    // however slow the machine was to reach that point.
    drop(child.stdin.take());
    child
        .wait_with_output()
        .expect("server output should be collected")
}

#[test]
fn help_and_version_exit_without_starting_the_transport() {
    let help = server().arg("--help").output().expect("help should run");
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).expect("help should be UTF-8");
    assert!(help.contains("Local-first MCP server for the HackMD API"));
    assert!(help.contains("--version"));
    assert!(help.contains("--self-check"));
    assert!(help.contains("--probe-api"));

    let version = server()
        .arg("--version")
        .output()
        .expect("version should run");
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8(version.stdout)
            .expect("version should be UTF-8")
            .trim(),
        concat!("hackmd-mcp ", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn self_check_prints_json_and_exits_before_transport_startup() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let output = server()
        .arg("--self-check")
        .env("HACKMD_MCP_STATE_DIR", directory.path().join("state"))
        .output()
        .expect("self-check should run");
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("self-check output should be JSON");
    assert_eq!(report["ok"], true);
    assert_eq!(report["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(report["token_present"], false);
    assert_eq!(report["api_origin"], "https://api.hackmd.io");
    assert_eq!(report["state_directory"]["writable"], true);
    assert_eq!(report["workspace_root"]["configured"], false);
    assert!(report["workspace_root"].get("confined").is_none());
    assert!(report.get("api_probe").is_none());
}

#[test]
fn api_probe_requires_self_check_mode() {
    let output = server()
        .arg("--probe-api")
        .output()
        .expect("invalid CLI invocation should exit");
    assert!(!output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
}

#[test]
fn requested_api_probe_reports_missing_token_as_json_failure() {
    let directory = tempfile::tempdir().expect("temporary directory should create");
    let output = server()
        .args(["--self-check", "--probe-api"])
        .env("HACKMD_MCP_STATE_DIR", directory.path().join("state"))
        .output()
        .expect("self-check probe should run");
    assert!(!output.status.success());
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("failure should remain JSON");
    assert_eq!(report["ok"], false);
    assert_eq!(report["token_present"], false);
    assert_eq!(report["api_probe"]["requested"], true);
    assert_eq!(report["api_probe"]["ok"], false);
    assert!(
        report["api_probe"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("HACKMD_API_TOKEN is not configured"))
    );
}

#[test]
fn server_waits_for_input_without_writing_transport_noise() {
    let output = assert_waiting_then_stop(spawn_server(None, None, None));
    assert!(output.stdout.is_empty(), "stdout is reserved for MCP");
    let stderr = String::from_utf8(output.stderr).expect("diagnostics should be UTF-8");
    assert!(stderr.contains("HACKMD_API_TOKEN is not set"));
}

#[test]
fn token_is_not_validated_or_logged_during_startup() {
    const SENTINEL_TOKEN: &str = "startup-only-secret-sentinel";

    let output = assert_waiting_then_stop(spawn_server(Some(SENTINEL_TOKEN), None, None));
    assert!(output.stdout.is_empty(), "stdout is reserved for MCP");
    let stderr = String::from_utf8(output.stderr).expect("diagnostics should be UTF-8");
    assert!(!stderr.contains(SENTINEL_TOKEN));
    assert!(!stderr.contains("HACKMD_API_TOKEN is not set"));
}

#[test]
fn token_loads_from_the_working_directory_dotenv_without_leaking() {
    const DOTENV_TOKEN: &str = "dotenv-secret-sentinel";
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    fs::write(
        directory.path().join(".env"),
        format!("HACKMD_API_TOKEN={DOTENV_TOKEN}\nUNRELATED_SECRET=ignored\n"),
    )
    .expect("dotenv fixture should be written");

    let output = assert_waiting_then_stop(spawn_server(None, Some(directory.path()), None));
    assert!(output.stdout.is_empty(), "stdout is reserved for MCP");
    let stderr = String::from_utf8(output.stderr).expect("diagnostics should be UTF-8");
    assert!(!stderr.contains(DOTENV_TOKEN));
    assert!(!stderr.contains("HACKMD_API_TOKEN is not set"));
}

#[test]
fn dotenv_cannot_redirect_an_inherited_token_to_another_host() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    fs::write(
        directory.path().join(".env"),
        "HACKMD_API_URL=https://attacker.example/v1\n",
    )
    .expect("dotenv fixture should be written");

    let output = spawn_server(
        Some("inherited-secret-sentinel"),
        Some(directory.path()),
        None,
    )
    .wait_with_output()
    .expect("server output should be collected");

    assert!(!output.status.success(), "server must refuse to start");
    let stderr = String::from_utf8(output.stderr).expect("diagnostics should be UTF-8");
    assert!(stderr.contains("sets HACKMD_API_URL"));
    assert!(stderr.contains("unset HACKMD_API_TOKEN"));
    assert!(!stderr.contains("inherited-secret-sentinel"));
}

#[test]
fn a_dotenv_workspace_root_confines_but_is_not_trusted() {
    // A root of `/` from a cloned repository's `.env` would confine nothing. It
    // is honored, so a user who confines the server there stays confined, but
    // the refusal to write agent instruction files stays on beneath it. The
    // directory itself stands in for the root: unlike `/`, it is absolute on
    // every platform.
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    fs::write(
        directory.path().join(".env"),
        format!(
            "HACKMD_MCP_WORKSPACE_ROOT=\"{}\"\n",
            directory.path().display()
        ),
    )
    .expect("dotenv fixture should be written");

    let output = assert_waiting_then_stop(spawn_server(None, Some(directory.path()), None));
    assert!(output.stdout.is_empty(), "stdout is reserved for MCP");
    let stderr = String::from_utf8(output.stderr).expect("diagnostics should be UTF-8");
    assert!(stderr.contains("local file tools are confined to this tree"));
    assert!(stderr.contains(
        "HACKMD_MCP_WORKSPACE_ROOT comes from the working-directory .env, not the environment"
    ));
}

#[test]
fn startup_does_not_create_the_configured_state_directory() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let state_dir = directory.path().join("state-must-not-exist");

    let _output = assert_waiting_then_stop(spawn_server(None, None, Some(&state_dir)));

    assert!(!state_dir.exists());
}

#[test]
fn tool_calls_emit_request_scoped_json_only_to_stderr() {
    const SENTINEL_TOKEN: &str = "request-secret-sentinel";
    let messages = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"stdio-test","version":"1"}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"hackmd_get_me","arguments":{}}}"#,
        "\n"
    );
    let output = exchange_then_stop(spawn_server(Some(SENTINEL_TOKEN), None, None), messages);
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).expect("MCP output should be UTF-8");
    assert!(stdout.contains(r#""id":2"#));
    assert!(!stdout.contains("mcp_tool_call"));

    let stderr = String::from_utf8(output.stderr).expect("diagnostics should be UTF-8");
    assert!(
        stderr
            .lines()
            .all(|line| serde_json::from_str::<serde_json::Value>(line).is_ok())
    );
    assert!(stderr.contains(r#""request_id""#));
    assert!(stderr.contains(r#""tool":"hackmd_get_me""#));
    assert!(!stderr.contains(SENTINEL_TOKEN));
}
