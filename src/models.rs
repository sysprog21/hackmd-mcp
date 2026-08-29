use rmcp::schemars;
use serde::{Deserialize, Serialize};

/// Selects the personal account or one team while preserving a single tool
/// family for both route shapes.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Workspace {
    /// Operate on the authenticated user's personal workspace.
    #[default]
    Personal,
    /// Operate on the team identified by its API path.
    Team {
        /// Team path returned by `hackmd_list_teams`.
        team_path: String,
    },
}

#[cfg(test)]
mod tests {
    use super::Workspace;

    #[test]
    fn workspace_defaults_to_personal() {
        assert_eq!(Workspace::default(), Workspace::Personal);
    }

    #[test]
    fn workspace_deserializes_both_route_shapes() {
        assert_eq!(
            serde_json::from_value::<Workspace>(serde_json::json!({"kind": "personal"}))
                .expect("personal workspace should deserialize"),
            Workspace::Personal
        );
        assert_eq!(
            serde_json::from_value::<Workspace>(
                serde_json::json!({"kind": "team", "team_path": "core-team"})
            )
            .expect("team workspace should deserialize"),
            Workspace::Team {
                team_path: "core-team".to_owned()
            }
        );
    }

    #[test]
    fn workspace_rejects_missing_or_unknown_team_fields() {
        for invalid in [
            serde_json::json!({"kind": "team"}),
            serde_json::json!({"kind": "team", "team_path": "x", "extra": true}),
            serde_json::json!({"kind": "unknown"}),
        ] {
            assert!(serde_json::from_value::<Workspace>(invalid).is_err());
        }
    }
}
