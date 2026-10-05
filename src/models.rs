use rmcp::schemars;
use serde::{Deserialize, Serialize};

/// Selects the personal account or one team while preserving a single tool
/// family for both route shapes.
///
/// On the wire it is a nullable team path: `null` for the personal workspace,
/// the team's path for a team. That is the shape tool input takes (as
/// `team_path`) and tool output reports, so an agent can pass back what it was
/// given. The older tagged object (`{"kind": "team", "team_path": ...}`) is
/// still read; only the sync state store still writes it, through [`tagged`].
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Workspace {
    /// Operate on the authenticated user's personal workspace.
    #[default]
    Personal,
    /// Operate on the team identified by its API path.
    Team {
        /// Team path from `hackmd_get_me`.
        team_path: String,
    },
}

/// The older tagged form, still accepted on input and still written to sync
/// state, whose on-disk format predates the flat one.
#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Tagged {
    Personal,
    Team { team_path: String },
}

impl From<Tagged> for Workspace {
    fn from(tagged: Tagged) -> Self {
        match tagged {
            Tagged::Personal => Self::Personal,
            Tagged::Team { team_path } => Self::Team { team_path },
        }
    }
}

impl Serialize for Workspace {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Personal => serializer.serialize_none(),
            Self::Team { team_path } => serializer.serialize_some(team_path),
        }
    }
}

impl<'de> Deserialize<'de> for Workspace {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        const EXPECTED: &str = "team_path must be a team path string from hackmd_get_me; omit it for the personal workspace";

        // Anything else lands in `Other`, so a wrong shape gets the sentence
        // above instead of serde's "did not match any variant".
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Arg {
            Path(String),
            Tagged(Tagged),
            Other(serde::de::IgnoredAny),
        }
        let path = match Option::<Arg>::deserialize(deserializer)? {
            None | Some(Arg::Tagged(Tagged::Personal)) => return Ok(Self::Personal),
            Some(Arg::Path(path) | Arg::Tagged(Tagged::Team { team_path: path })) => path,
            Some(Arg::Other(_)) => return Err(serde::de::Error::custom(EXPECTED)),
        };

        // Either spelling is judged once. A control character is no team's
        // path, and the URL library drops tabs and newlines, so `.\t.` would
        // reach it as `..`; the client refuses those too.
        if path.chars().any(char::is_control) {
            return Err(serde::de::Error::custom(EXPECTED));
        }
        let path = path.trim();
        if path.is_empty() {
            return Err(serde::de::Error::custom(EXPECTED));
        }
        Ok(Self::Team {
            team_path: path.to_owned(),
        })
    }
}

impl schemars::JsonSchema for Workspace {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        "TeamPath".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        <Option<String>>::json_schema(generator)
    }
}

/// `#[serde(with = "crate::models::tagged")]` for sync state, which keeps
/// writing the tagged form so a sidecar stays readable by older builds.
pub(crate) mod tagged {
    use serde::{Deserialize, Serialize};

    use super::{Tagged, Workspace};

    pub(crate) fn serialize<S: serde::Serializer>(
        workspace: &Workspace,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match workspace {
            Workspace::Personal => Tagged::Personal,
            Workspace::Team { team_path } => Tagged::Team {
                team_path: team_path.clone(),
            },
        }
        .serialize(serializer)
    }

    pub(crate) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Workspace, D::Error> {
        Workspace::deserialize(deserializer)
    }
}

impl std::fmt::Display for Workspace {
    /// Names the workspace the way a tool argument would, so an error can tell
    /// a caller where to look without them decoding a `Debug` dump.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Personal => formatter.write_str("the personal workspace"),
            Self::Team { team_path } => write!(formatter, "team {team_path}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Workspace;

    #[test]
    fn team_path_argument_accepts_a_path_nothing_or_the_older_object() {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Input {
            #[serde(default, rename = "team_path", alias = "workspace")]
            workspace: Workspace,
        }
        let read = |value| serde_json::from_value::<Input>(value).map(|input| input.workspace);
        let core = Workspace::Team {
            team_path: "core".to_owned(),
        };
        assert_eq!(
            read(serde_json::json!({})).expect("omitted"),
            Workspace::Personal
        );
        assert_eq!(
            read(serde_json::json!({"team_path": null})).expect("null"),
            Workspace::Personal
        );
        assert_eq!(
            read(serde_json::json!({"team_path": "core"})).expect("path"),
            core
        );
        assert_eq!(
            read(serde_json::json!({"workspace": {"kind": "team", "team_path": "core"}}))
                .expect("legacy object"),
            core
        );
        for invalid in [
            serde_json::json!({"team_path": ".\t."}),
            serde_json::json!({"team_path": "core\n"}),
            serde_json::json!({"workspace": {"kind": "team", "team_path": ".\r."}}),
            serde_json::json!({"team_path": " "}),
            serde_json::json!({"team_path": 7}),
            serde_json::json!({"workspace": {"kind": "team"}}),
            serde_json::json!({"workspace": {"kind": "team", "team_path": "x", "extra": true}}),
            serde_json::json!({"workspace": {"kind": "unknown"}}),
        ] {
            let error = read(invalid).expect_err("invalid team_path").to_string();
            assert!(
                error.starts_with("team_path must be a team path string"),
                "{error}"
            );
        }
    }

    #[test]
    fn output_is_flat_while_state_keeps_the_tagged_form() {
        #[derive(serde::Serialize)]
        struct State {
            #[serde(with = "super::tagged")]
            workspace: Workspace,
        }
        let core = Workspace::Team {
            team_path: "core".to_owned(),
        };
        assert_eq!(serde_json::json!(core), serde_json::json!("core"));
        assert_eq!(
            serde_json::json!(Workspace::Personal),
            serde_json::Value::Null
        );
        assert_eq!(
            serde_json::json!(State { workspace: core }),
            serde_json::json!({"workspace": {"kind": "team", "team_path": "core"}})
        );
    }

    #[test]
    fn workspace_display_names_the_tool_argument() {
        assert_eq!(Workspace::Personal.to_string(), "the personal workspace");
        assert_eq!(
            Workspace::Team {
                team_path: "core-team".to_owned()
            }
            .to_string(),
            "team core-team"
        );
    }

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
}
