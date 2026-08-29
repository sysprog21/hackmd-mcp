//! A local-first MCP server for the `HackMD` API.

/// The package version exposed by the server binary.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::VERSION;

    #[test]
    fn package_version_is_available() {
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
    }
}
