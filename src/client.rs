use crate::config::Config;

/// HTTP client shared by all `HackMD` tool handlers.
#[derive(Debug)]
pub(crate) struct HackmdClient {
    config: Config,
}

impl HackmdClient {
    pub(crate) fn new(config: Config) -> Self {
        Self { config }
    }

    pub(crate) fn has_api_token(&self) -> bool {
        self.config.has_api_token()
    }
}
