use clap::Parser;

/// Local-first MCP server for the `HackMD` API.
#[derive(Debug, Parser)]
#[command(version = hackmd_mcp::VERSION_TEXT, about)]
struct Cli {
    /// Print a JSON startup/configuration health report and exit.
    #[arg(long)]
    self_check: bool,
    /// Include a read-only authenticated `HackMD` /me request in --self-check.
    #[arg(long, requires = "self_check")]
    probe_api: bool,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    if cli.self_check {
        return match hackmd_mcp::run_self_check(cli.probe_api).await {
            Ok(report) => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report)
                        .expect("self-check report must remain serializable")
                );
                if report.is_ok() {
                    std::process::ExitCode::SUCCESS
                } else {
                    std::process::ExitCode::FAILURE
                }
            }
            Err(error) => {
                println!(
                    "{}",
                    serde_json::json!({"ok": false, "configuration_error": error.to_string()})
                );
                std::process::ExitCode::FAILURE
            }
        };
    }

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
