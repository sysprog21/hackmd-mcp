#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    hackmd_mcp::run_stdio().await
}
