use std::{
    fs,
    io::Write,
    path::Path,
    process::{Child, Command, Output, Stdio},
    thread,
    time::Duration,
};

fn spawn_server(
    token: Option<&str>,
    current_dir: Option<&Path>,
    state_dir: Option<&Path>,
) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hackmd-mcp"));
    command
        .env_remove("HACKMD_API_TOKEN")
        .env_remove("HACKMD_API_URL")
        .env_remove("HACKMD_MCP_STATE_DIR")
        .env_remove("HACKMD_MCP_OTEL")
        .env_remove("RUST_LOG")
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
    child
        .kill()
        .expect("server should stop after the assertion");
    child
        .wait_with_output()
        .expect("server output should be collected")
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
