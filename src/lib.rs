//! A local-first MCP server for the `HackMD` API.

mod client;
mod config;
mod crud;
mod dto;
mod edit_note;
mod folders;
mod get_note;
mod history;
mod list_notes;
mod models;
mod note_ref;
mod patch;
mod server;
mod state;
mod tool_result;

#[cfg(test)]
mod test_support;

/// The package version exposed by the server binary.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Runs the MCP server over standard input and output until the client closes
/// the transport.
///
/// Standard output is owned exclusively by the MCP transport. Operational
/// diagnostics must use standard error through the tracing configuration.
///
/// # Errors
///
/// Returns an error if the stdio transport cannot start or terminates with a
/// protocol or I/O failure.
pub async fn run_stdio() -> Result<(), Box<dyn std::error::Error>> {
    let _subscriber = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .without_time()
        .try_init();
    server::run_stdio().await
}

#[cfg(test)]
mod tests {
    use super::VERSION;

    #[test]
    fn package_version_is_available() {
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
    }
}
