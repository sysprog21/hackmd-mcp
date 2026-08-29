use std::{
    process::{Command, Stdio},
    thread,
    time::Duration,
};

#[test]
fn server_waits_for_input_without_writing_transport_noise() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_hackmd-mcp"))
        .env_remove("HACKMD_API_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("server binary should start");

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
    let output = child
        .wait_with_output()
        .expect("server output should be collected");
    assert!(output.stdout.is_empty(), "stdout is reserved for MCP");
    assert!(output.stderr.is_empty(), "idle startup should be quiet");
}
