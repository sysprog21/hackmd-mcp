use crate::config::Config;
use crate::state::StateStore;

/// HTTP client shared by all `HackMD` tool handlers.
#[derive(Debug)]
pub(crate) struct HackmdClient {
    config: Config,
    #[allow(dead_code, reason = "used by the local sync tool tasks")]
    state: StateStore,
}

impl HackmdClient {
    pub(crate) fn new(config: Config) -> Self {
        let state = StateStore::new(config.state_dir().to_path_buf());
        Self { config, state }
    }

    pub(crate) fn has_api_token(&self) -> bool {
        self.config.has_api_token()
    }
}
