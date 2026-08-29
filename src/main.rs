use clap::Parser;

/// Local-first MCP server for the `HackMD` API.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _cli = Cli::parse();
    hackmd_mcp::run_stdio().await
}
