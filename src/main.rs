use clap::Parser;

/// Local-first MCP server for the `HackMD` API.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let _cli = Cli::parse();

    // Report the message rather than returning the error: `Result` from `main`
    // prints its `Debug` form, which shows an enum variant name instead of the
    // fix the operator needs. Startup failures land on stderr because stdout
    // belongs to the MCP transport.
    if let Err(error) = hackmd_mcp::run_stdio().await {
        eprintln!("hackmd-mcp: {error}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
