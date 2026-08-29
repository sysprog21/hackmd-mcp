use std::{
    process::{Child, Command, Output, Stdio},
    thread,
    time::Duration,
};

fn spawn_server(token: Option<&str>) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hackmd-mcp"));
    command
        .env_remove("HACKMD_API_TOKEN")
        .env_remove("HACKMD_API_URL")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(token) = token {
        command.env("HACKMD_API_TOKEN", token);
    }
    command.spawn().expect("server binary should start")
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
    let output = assert_waiting_then_stop(spawn_server(None));
    assert!(output.stdout.is_empty(), "stdout is reserved for MCP");
    let stderr = String::from_utf8(output.stderr).expect("diagnostics should be UTF-8");
    assert!(stderr.contains("HACKMD_API_TOKEN is not set"));
}

#[test]
fn token_is_not_validated_or_logged_during_startup() {
    const SENTINEL_TOKEN: &str = "startup-only-secret-sentinel";

    let output = assert_waiting_then_stop(spawn_server(Some(SENTINEL_TOKEN)));
    assert!(output.stdout.is_empty(), "stdout is reserved for MCP");
    let stderr = String::from_utf8(output.stderr).expect("diagnostics should be UTF-8");
    assert!(!stderr.contains(SENTINEL_TOKEN));
    assert!(!stderr.contains("HACKMD_API_TOKEN is not set"));
}
