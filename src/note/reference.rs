use serde::Serialize;
use thiserror::Error;
use url::Url;

use crate::{
    client::{HackmdClient, HackmdError},
    dto::NoteResponse,
    models::Workspace,
};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ResolvedNoteRef {
    pub(crate) workspace: Workspace,
    pub(crate) note_id: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct NoteCandidate {
    pub(crate) workspace: Workspace,
    pub(crate) note_id: String,
    pub(crate) title: String,
    pub(crate) short_id: Option<String>,
    pub(crate) permalink: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum NoteResolution {
    Resolved {
        note: ResolvedNoteRef,
    },
    NotFound {
        query: String,
    },
    Ambiguous {
        query: String,
        candidates: Vec<NoteCandidate>,
    },
}

#[derive(Debug, Error)]
pub(crate) enum NoteRefError {
    #[error("note_ref must be a non-empty internal note ID or https://hackmd.io URL")]
    Empty,
    #[error("note_ref URL must use https://hackmd.io and contain an ID or @owner/slug path")]
    InvalidUrl,
    #[error(transparent)]
    Api(#[from] HackmdError),
}

enum ParsedNoteRef {
    Direct(String),
    Scoped { owner: String, slug: String },
}

pub(crate) async fn resolve_note_ref(
    client: &HackmdClient,
    workspace: Workspace,
    note_ref: &str,
    refresh: bool,
) -> Result<NoteResolution, NoteRefError> {
    match parse_note_ref(note_ref)? {
        ParsedNoteRef::Direct(note_id) => Ok(NoteResolution::Resolved {
            note: ResolvedNoteRef { workspace, note_id },
        }),
        ParsedNoteRef::Scoped { owner, slug } => {
            let profile = client.get_me().await?;
            let workspace = if owner == profile.user_path {
                Workspace::Personal
            } else {
                let teams = client.list_teams().await?;
                let Some(team) = teams.into_iter().find(|team| team.path == owner) else {
                    return Ok(NoteResolution::NotFound {
                        query: format!("@{owner}/{slug}"),
                    });
                };
                Workspace::Team {
                    team_path: team.path,
                }
            };
            let notes = client.list_notes(&workspace, refresh).await?;
            Ok(unique_matches(&workspace, &slug, &notes, |note, query| {
                note.short_id.as_deref() == Some(query) || note.permalink.as_deref() == Some(query)
            }))
        }
    }
}

fn parse_note_ref(note_ref: &str) -> Result<ParsedNoteRef, NoteRefError> {
    let value = note_ref.trim();
    if value.is_empty() {
        return Err(NoteRefError::Empty);
    }
    let normalized_url = value
        .strip_prefix("hackmd.io/")
        .map(|path| format!("https://hackmd.io/{path}"));
    if !value.contains("://") && normalized_url.is_none() {
        return Ok(ParsedNoteRef::Direct(value.to_owned()));
    }

    let url = Url::parse(normalized_url.as_deref().unwrap_or(value))
        .map_err(|_| NoteRefError::InvalidUrl)?;
    if url.scheme() != "https" || url.host_str() != Some("hackmd.io") {
        return Err(NoteRefError::InvalidUrl);
    }
    let segments = url
        .path_segments()
        .ok_or(NoteRefError::InvalidUrl)?
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    match segments.as_slice() {
        [note_id] if !note_id.starts_with('@') => Ok(ParsedNoteRef::Direct((*note_id).to_owned())),
        [owner, slug] if owner.starts_with('@') && owner.len() > 1 && !slug.is_empty() => {
            Ok(ParsedNoteRef::Scoped {
                owner: owner[1..].to_owned(),
                slug: (*slug).to_owned(),
            })
        }
        _ => Err(NoteRefError::InvalidUrl),
    }
}

fn unique_matches<F>(
    workspace: &Workspace,
    query: &str,
    notes: &[NoteResponse],
    matches: F,
) -> NoteResolution
where
    F: Fn(&NoteResponse, &str) -> bool,
{
    let candidates = notes
        .iter()
        .filter(|note| matches(note, query))
        .map(|note| NoteCandidate {
            workspace: workspace.clone(),
            note_id: note.id.clone(),
            title: note.title.clone(),
            short_id: note.short_id.clone(),
            permalink: note.permalink.clone(),
        })
        .collect::<Vec<_>>();
    match candidates.as_slice() {
        [] => NoteResolution::NotFound {
            query: query.to_owned(),
        },
        [candidate] => NoteResolution::Resolved {
            note: ResolvedNoteRef {
                workspace: workspace.clone(),
                note_id: candidate.note_id.clone(),
            },
        },
        _ => NoteResolution::Ambiguous {
            query: query.to_owned(),
            candidates,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{NoteResolution, ParsedNoteRef, parse_note_ref, resolve_note_ref, unique_matches};
    use crate::{client::HackmdClient, config::Config, dto::NoteResponse, models::Workspace};

    fn note(
        id: &str,
        title: &str,
        short_id: Option<&str>,
        permalink: Option<&str>,
    ) -> NoteResponse {
        serde_json::from_value(serde_json::json!({
            "id": id, "title": title, "shortId": short_id, "permalink": permalink
        }))
        .expect("minimal note fixture should deserialize")
    }

    #[test]
    fn parses_internal_ids_and_supported_url_shapes() {
        assert!(
            matches!(parse_note_ref("api-id"), Ok(ParsedNoteRef::Direct(id)) if id == "api-id")
        );
        assert!(
            matches!(parse_note_ref("https://hackmd.io/api-id?view"), Ok(ParsedNoteRef::Direct(id)) if id == "api-id")
        );
        assert!(
            matches!(parse_note_ref("hackmd.io/api-id"), Ok(ParsedNoteRef::Direct(id)) if id == "api-id")
        );
        assert!(
            matches!(parse_note_ref("https://hackmd.io/@core/slug"), Ok(ParsedNoteRef::Scoped { owner, slug }) if owner == "core" && slug == "slug")
        );
    }

    #[test]
    fn rejects_empty_foreign_and_malformed_urls() {
        for value in [
            "",
            "https://example.com/id",
            "http://hackmd.io/id",
            "https://hackmd.io/@owner",
        ] {
            assert!(parse_note_ref(value).is_err(), "accepted {value:?}");
        }
    }

    #[tokio::test]
    async fn direct_ids_and_bare_urls_never_require_or_list_notes() {
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        for value in [
            "internal-id",
            "https://hackmd.io/short-id",
            "hackmd.io/short-id",
        ] {
            let result = resolve_note_ref(&client, Workspace::Personal, value, false)
                .await
                .expect("direct reference should resolve without an API token");
            assert!(matches!(result, NoteResolution::Resolved { .. }));
        }
    }

    #[tokio::test]
    async fn scoped_urls_resolve_personal_user_path_before_team_paths() {
        let fixture = crate::fixture::SequenceServer::spawn([
            (
                200,
                r#"{"id":"u","name":"User","email":"u@example.com","userPath":"alice"}"#,
            ),
            (
                200,
                r#"[{"id":"personal-id","title":"Personal","permalink":"slug"}]"#,
            ),
        ]);
        let client = fixture.client();
        let result = resolve_note_ref(
            &client,
            Workspace::Team {
                team_path: "ignored".to_owned(),
            },
            "https://hackmd.io/@alice/slug",
            false,
        )
        .await
        .expect("personal URL should resolve");
        assert!(
            matches!(result, NoteResolution::Resolved { note } if note.workspace == Workspace::Personal && note.note_id == "personal-id")
        );
        let requests = fixture.finish();
        assert!(requests[0].starts_with("GET /v1/me HTTP/1.1\r\n"));
        assert!(requests[1].starts_with("GET /v1/notes HTTP/1.1\r\n"));
    }

    #[tokio::test]
    async fn scoped_url_refresh_bypasses_a_cached_workspace_list() {
        let fixture = crate::fixture::SequenceServer::spawn([
            (200, r#"{"id":"u","name":"User","userPath":"alice"}"#),
            (200, r#"[{"id":"old","title":"Old","permalink":"slug"}]"#),
            (200, r#"{"id":"u","name":"User","userPath":"alice"}"#),
            (200, r#"[{"id":"new","title":"New","permalink":"slug"}]"#),
        ]);
        let client = fixture.client_with_cache();

        let first = resolve_note_ref(
            &client,
            Workspace::Personal,
            "https://hackmd.io/@alice/slug",
            false,
        )
        .await
        .expect("first URL should resolve");
        let refreshed = resolve_note_ref(
            &client,
            Workspace::Personal,
            "https://hackmd.io/@alice/slug",
            true,
        )
        .await
        .expect("refreshed URL should resolve");

        assert!(matches!(first, NoteResolution::Resolved { note } if note.note_id == "old"));
        assert!(matches!(refreshed, NoteResolution::Resolved { note } if note.note_id == "new"));
        assert_eq!(fixture.finish().len(), 4);
    }

    #[tokio::test]
    async fn scoped_team_slug_resolves_through_team_discovery() {
        let fixture = crate::fixture::SequenceServer::spawn([
            (
                200,
                r#"{"id":"u","name":"User","email":"u@example.com","userPath":"alice"}"#,
            ),
            (200, r#"[{"id":"t","name":"Core","path":"core"}]"#),
            (200, r#"[{"id":"team-id","title":"Team","shortId":"slug"}]"#),
        ]);
        let client = fixture.client();
        let result = resolve_note_ref(
            &client,
            Workspace::Personal,
            "https://hackmd.io/@core/slug",
            false,
        )
        .await
        .expect("team URL should resolve");
        assert!(
            matches!(result, NoteResolution::Resolved { note } if note.workspace == Workspace::Team { team_path: "core".to_owned() } && note.note_id == "team-id")
        );
        let requests = fixture.finish();
        assert!(requests[1].starts_with("GET /v1/teams HTTP/1.1\r\n"));
        assert!(requests[2].starts_with("GET /v1/teams/core/notes HTTP/1.1\r\n"));
    }

    #[test]
    fn short_id_permalink_and_title_matches_must_be_unique() {
        let notes = vec![
            note("one", "Same", Some("slug"), None),
            note("two", "Same", None, Some("slug")),
        ];
        let by_slug = unique_matches(&Workspace::Personal, "slug", &notes, |note, query| {
            note.short_id.as_deref() == Some(query) || note.permalink.as_deref() == Some(query)
        });
        assert!(
            matches!(by_slug, NoteResolution::Ambiguous { candidates, .. } if candidates.len() == 2)
        );

        let by_title = unique_matches(
            &Workspace::Personal,
            "Unique",
            &[note("three", "Unique", None, None)],
            |note, query| note.title == query,
        );
        assert!(matches!(by_title, NoteResolution::Resolved { note } if note.note_id == "three"));
    }
}
