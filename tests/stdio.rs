use std::{
    fs,
    path::Path,
    process::{Child, Command, Output, Stdio},
    thread,
    time::Duration,
};

fn spawn_server(token: Option<&str>, current_dir: Option<&Path>) -> Child {
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
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
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
    let output = assert_waiting_then_stop(spawn_server(None, None));
    assert!(output.stdout.is_empty(), "stdout is reserved for MCP");
    let stderr = String::from_utf8(output.stderr).expect("diagnostics should be UTF-8");
    assert!(stderr.contains("HACKMD_API_TOKEN is not set"));
}

#[test]
fn token_is_not_validated_or_logged_during_startup() {
    const SENTINEL_TOKEN: &str = "startup-only-secret-sentinel";

    let output = assert_waiting_then_stop(spawn_server(Some(SENTINEL_TOKEN), None));
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

    let output = assert_waiting_then_stop(spawn_server(None, Some(directory.path())));
    assert!(output.stdout.is_empty(), "stdout is reserved for MCP");
    let stderr = String::from_utf8(output.stderr).expect("diagnostics should be UTF-8");
    assert!(!stderr.contains(DOTENV_TOKEN));
    assert!(!stderr.contains("HACKMD_API_TOKEN is not set"));
}
