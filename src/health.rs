use std::path::PathBuf;

use serde::Serialize;

use crate::{client::HackmdClient, config::Config, local::LocalFiles};

#[derive(Debug, Serialize)]
pub struct SelfCheckReport {
    ok: bool,
    version: &'static str,
    token_present: bool,
    api_origin: String,
    state_directory: StateDirectoryCheck,
    workspace_root: WorkspaceRootCheck,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_probe: Option<ApiProbeCheck>,
}

impl SelfCheckReport {
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        self.ok
    }
}

#[derive(Debug, Serialize)]
struct StateDirectoryCheck {
    path: PathBuf,
    writable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct WorkspaceRootCheck {
    /// Whether local file tools are confined to a root at all.
    configured: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<PathBuf>,
    accessible: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct ApiProbeCheck {
    requested: bool,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Builds a machine-readable startup diagnostic. The optional API operation is
/// GET /me; this function never mutates a remote `HackMD` resource.
///
/// # Errors
///
/// Returns a configuration error when local settings are invalid, or a client
/// error when `probe_api` is enabled and the read-only API probe cannot run.
pub async fn run_self_check(
    probe_api: bool,
) -> Result<SelfCheckReport, Box<dyn std::error::Error>> {
    self_check(Config::from_env()?, probe_api).await
}

async fn self_check(
    config: Config,
    probe_api: bool,
) -> Result<SelfCheckReport, Box<dyn std::error::Error>> {
    let token_present = config.has_api_token();
    let api_origin = config.api_url().origin().ascii_serialization();
    let state_path = config.state_dir().to_path_buf();
    let root_path = config.workspace_root().map(PathBuf::from);
    let files = LocalFiles::new(state_path.clone(), root_path.clone());

    let state_error = files
        .state()
        .probe_writable()
        .err()
        .map(|error| error.to_string());
    let root_error = files
        .probe_workspace_root()
        .err()
        .map(|error| error.to_string());
    let api_probe = if probe_api {
        let client = HackmdClient::new(config)?;
        let error = client.get_me().await.err().map(|error| error.to_string());
        Some(ApiProbeCheck {
            requested: true,
            ok: error.is_none(),
            error,
        })
    } else {
        None
    };
    let ok = state_error.is_none()
        && root_error.is_none()
        && api_probe.as_ref().is_none_or(|probe| probe.ok);
    Ok(SelfCheckReport {
        ok,
        version: crate::VERSION,
        token_present,
        api_origin,
        state_directory: StateDirectoryCheck {
            path: state_path,
            writable: state_error.is_none(),
            error: state_error,
        },
        workspace_root: WorkspaceRootCheck {
            configured: root_path.is_some(),
            accessible: root_path.as_ref().map(|_| root_error.is_none()),
            path: root_path,
            error: root_error,
        },
        api_probe,
    })
}

#[cfg(test)]
mod tests {
    use super::self_check;
    use crate::{
        config::Config,
        fixture::{Scenario, SequenceServer},
    };

    #[tokio::test]
    async fn optional_api_probe_uses_read_only_me_and_reports_no_identity_or_token() {
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/me",
            200,
            r#"{"id":"secret-user-id","name":"Alice","userPath":"alice"}"#,
        )]);
        let directory = tempfile::tempdir().expect("temporary directory should create");
        let config = Config::for_loopback_test(&fixture.api_url, Some("secret-token"))
            .with_local_paths_for_tests(directory.path().join("state"), None);

        let report = self_check(config, true)
            .await
            .expect("self-check should complete");
        assert!(report.is_ok());
        assert!(report.token_present);
        assert!(report.state_directory.writable);
        assert!(!report.workspace_root.configured);
        assert_eq!(report.workspace_root.accessible, None);
        assert!(report.api_probe.as_ref().is_some_and(|probe| probe.ok));
        let json = serde_json::to_string(&report).expect("report should serialize");
        assert!(!json.contains("secret-token"));
        assert!(!json.contains("secret-user-id"));
        fixture.finish();
    }
}
