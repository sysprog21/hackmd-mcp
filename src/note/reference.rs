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
    #[serde(rename = "team_path")]
    pub(crate) workspace: Workspace,
    pub(crate) note_id: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct NoteCandidate {
    #[serde(rename = "team_path")]
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
    #[error(
        "note_ref URL must use https://hackmd.io and contain an ID, @owner/slug, or s/<shortId> path"
    )]
    InvalidUrl,
    #[error(
        "team_path {team_path:?} does not match the URL's owner @{owner}; omit team_path, since the URL names the workspace"
    )]
    WorkspaceMismatch { team_path: String, owner: String },
    #[error(
        "team_path names the personal workspace, but the URL's owner @{owner} is not this account; omit team_path, since the URL names the workspace"
    )]
    PersonalMismatch { owner: String },
    #[error(
        "{source}; this came from a team listing while searching every workspace for s/{short_id}, so pass team_path to search only the team that holds the note"
    )]
    ShortLinkTeam {
        short_id: String,
        source: HackmdError,
    },
    #[error(transparent)]
    Api(#[from] HackmdError),
}

impl crate::reply::ToolError for NoteRefError {
    fn kind(&self) -> crate::reply::ErrorKind {
        use crate::reply::ErrorKind;

        match self {
            Self::Empty
            | Self::InvalidUrl
            | Self::WorkspaceMismatch { .. }
            | Self::PersonalMismatch { .. } => ErrorKind::InvalidInput,
            Self::ShortLinkTeam { source: error, .. } | Self::Api(error) => error.kind(),
        }
    }
}

enum ParsedNoteRef {
    Direct(String),
    Scoped {
        owner: String,
        slug: String,
    },
    /// A legacy `https://hackmd.io/s/<shortId>` share link. It names no
    /// workspace, so the note is looked up by `shortId`.
    Short(String),
}

/// Resolves `note_ref`. `team_path` is `None` when the caller gave none: a
/// direct ID is then personal, and an `s/<shortId>` link is searched for in
/// every workspace.
pub(crate) async fn resolve_note_ref(
    client: &HackmdClient,
    team_path: Option<Workspace>,
    note_ref: &str,
    refresh: bool,
) -> Result<NoteResolution, NoteRefError> {
    resolve_note_ref_from(client, team_path, Workspace::Personal, note_ref, refresh).await
}

/// As [`resolve_note_ref`], with a direct ID given no `team_path` read in
/// `home` rather than the personal workspace.
pub(crate) async fn resolve_note_ref_from(
    client: &HackmdClient,
    team_path: Option<Workspace>,
    home: Workspace,
    note_ref: &str,
    refresh: bool,
) -> Result<NoteResolution, NoteRefError> {
    match parse_note_ref(note_ref)? {
        ParsedNoteRef::Direct(note_id) => Ok(NoteResolution::Resolved {
            note: ResolvedNoteRef {
                workspace: team_path.unwrap_or(home),
                note_id,
            },
        }),
        ParsedNoteRef::Short(short_id) => {
            resolve_short_id(client, team_path, &short_id, refresh).await
        }
        ParsedNoteRef::Scoped { owner, slug } => {
            // A URL names its own workspace. A team_path that names another is
            // a mistake to report, not a hint to drop.
            if let Some(Workspace::Team { team_path }) = &team_path
                && *team_path != owner
            {
                return Err(NoteRefError::WorkspaceMismatch {
                    team_path: team_path.clone(),
                    owner,
                });
            }
            let mine = *owner == *client.user_path(refresh).await?;
            if team_path == Some(Workspace::Personal) && !mine {
                return Err(NoteRefError::PersonalMismatch { owner });
            }
            let workspace = if mine {
                Workspace::Personal
            } else if client.has_team(&owner, refresh).await? {
                Workspace::Team { team_path: owner }
            } else {
                return Ok(NoteResolution::NotFound {
                    query: format!("@{owner}/{slug}"),
                });
            };
            let notes = client.list_notes(&workspace, refresh).await?;
            Ok(unique_matches(&workspace, &slug, &notes, |note, query| {
                note.short_id.as_deref() == Some(query) || note.permalink.as_deref() == Some(query)
            }))
        }
    }
}

/// Finds a `/s/<shortId>` link's note. A `team_path` limits the search to that
/// workspace; without one, personal notes and then every team are listed, and
/// a `shortId` found in more than one workspace is reported as ambiguous. A
/// team whose list fails fails the search, since the note may be there, but
/// the error says that `team_path` narrows it.
async fn resolve_short_id(
    client: &HackmdClient,
    team_path: Option<Workspace>,
    short_id: &str,
    refresh: bool,
) -> Result<NoteResolution, NoteRefError> {
    let matching = |workspace: &Workspace, notes: &[NoteResponse]| {
        notes
            .iter()
            .filter(|note| note.short_id.as_deref() == Some(short_id))
            .map(|note| NoteCandidate {
                workspace: workspace.clone(),
                note_id: note.id.clone(),
                title: note.title.clone(),
                short_id: note.short_id.clone(),
                permalink: note.permalink.clone(),
            })
            .collect::<Vec<_>>()
    };
    let mut candidates = Vec::new();
    if let Some(workspace) = team_path {
        candidates = matching(&workspace, &client.list_notes(&workspace, refresh).await?);
    } else {
        let teams = client.list_teams(refresh).await?;
        let personal = Workspace::Personal;
        candidates.extend(matching(
            &personal,
            &client.list_notes(&personal, refresh).await?,
        ));
        for team in teams.iter() {
            let workspace = Workspace::Team {
                team_path: team.path.clone(),
            };
            let notes = client
                .list_notes(&workspace, refresh)
                .await
                .map_err(|source| NoteRefError::ShortLinkTeam {
                    short_id: short_id.to_owned(),
                    source,
                })?;
            candidates.extend(matching(&workspace, &notes));
        }
    }
    let query = format!("s/{short_id}");
    Ok(match candidates.len() {
        0 => NoteResolution::NotFound { query },
        1 => {
            let only = candidates.remove(0);
            NoteResolution::Resolved {
                note: ResolvedNoteRef {
                    workspace: only.workspace,
                    note_id: only.note_id,
                },
            }
        }
        _ => NoteResolution::Ambiguous { query, candidates },
    })
}

/// Whether resolving `note_ref` searches workspace lists (`@owner/slug` or
/// `s/<shortId>`), which hold only live notes.
pub(crate) fn resolves_by_listing(note_ref: &str) -> bool {
    matches!(
        parse_note_ref(note_ref),
        Ok(ParsedNoteRef::Scoped { .. } | ParsedNoteRef::Short(_))
    )
}

fn parse_note_ref(note_ref: &str) -> Result<ParsedNoteRef, NoteRefError> {
    // The host is case-insensitive, as the URL parser treats it below.
    const HOST: &str = "hackmd.io/";
    let value = note_ref.trim();
    if value.is_empty() {
        return Err(NoteRefError::Empty);
    }
    let normalized_url = value
        .get(..HOST.len())
        .filter(|prefix| prefix.eq_ignore_ascii_case(HOST))
        .map(|_| format!("https://{HOST}{}", &value[HOST.len()..]));
    if !value.contains("://") && normalized_url.is_none() {
        return Ok(ParsedNoteRef::Direct(value.to_owned()));
    }

    let url = Url::parse(normalized_url.as_deref().unwrap_or(value))
        .map_err(|_| NoteRefError::InvalidUrl)?;
    if url.scheme() != "https" || url.host_str() != Some("hackmd.io") {
        return Err(NoteRefError::InvalidUrl);
    }

    // The parser leaves segments percent-encoded, and encodes a pasted
    // `hackmd.io/@team/中文` itself; `HackMD` reports permalinks decoded.
    let segments = url
        .path_segments()
        .ok_or(NoteRefError::InvalidUrl)?
        .filter(|segment| !segment.is_empty())
        .map(percent_decode)
        .collect::<Result<Vec<_>, _>>()?;
    // A trailing `download` is the published Markdown of the same note. It is
    // dropped only from a complete reference, so a slug that is itself
    // `download` still names its note.
    let segments = match segments.as_slice() {
        [id, last] if last == "download" && !id.starts_with('@') && id != "s" => &segments[..1],
        [_, _, last] if last == "download" => &segments[..2],
        all => all,
    };
    match segments {
        // `s` alone is the share-link prefix without its shortId.
        [note_id] if note_id == "s" => Err(NoteRefError::InvalidUrl),
        [note_id] if !note_id.starts_with('@') => Ok(ParsedNoteRef::Direct(note_id.clone())),
        [prefix, short_id] if prefix == "s" && !short_id.is_empty() => {
            Ok(ParsedNoteRef::Short(short_id.clone()))
        }
        [owner, slug] if owner.starts_with('@') && owner.len() > 1 && !slug.is_empty() => {
            Ok(ParsedNoteRef::Scoped {
                owner: owner[1..].to_owned(),
                slug: slug.clone(),
            })
        }
        _ => Err(NoteRefError::InvalidUrl),
    }
}

/// Decodes `%XX` escapes in one URL path segment. A malformed escape or a
/// result that is not UTF-8 is not a note reference.
fn percent_decode(segment: &str) -> Result<String, NoteRefError> {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            // Exactly two hex digits: `from_str_radix` would also take a sign.
            let digit = |at: usize| {
                bytes
                    .get(at)
                    .and_then(|&byte| char::from(byte).to_digit(16))
            };
            let (Some(high), Some(low)) = (digit(index + 1), digit(index + 2)) else {
                return Err(NoteRefError::InvalidUrl);
            };
            decoded.push(u8::try_from(high * 16 + low).map_err(|_| NoteRefError::InvalidUrl)?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| NoteRefError::InvalidUrl)
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
    use crate::{
        client::HackmdClient,
        config::Config,
        dto::NoteResponse,
        fixture::{Scenario, SequenceServer},
        models::Workspace,
    };

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
        assert!(
            matches!(parse_note_ref("https://hackmd.io/@core/slug/download"), Ok(ParsedNoteRef::Scoped { owner, slug }) if owner == "core" && slug == "slug")
        );
        assert!(
            matches!(parse_note_ref("https://hackmd.io/s/SkE33UTHf?type=view"), Ok(ParsedNoteRef::Short(id)) if id == "SkE33UTHf")
        );
        // A slug that is itself `download`, with and without the suffix.
        for url in [
            "https://hackmd.io/@core/download",
            "https://hackmd.io/@core/download/download",
        ] {
            assert!(
                matches!(parse_note_ref(url), Ok(ParsedNoteRef::Scoped { owner, slug }) if owner == "core" && slug == "download"),
                "{url}"
            );
        }
        assert!(
            matches!(parse_note_ref("https://hackmd.io/api-id/download"), Ok(ParsedNoteRef::Direct(id)) if id == "api-id")
        );
        assert!(
            matches!(parse_note_ref("https://hackmd.io/s/SkE33UTHf/download"), Ok(ParsedNoteRef::Short(id)) if id == "SkE33UTHf")
        );
        assert!(
            matches!(parse_note_ref("hackmd.io/s/SkE33UTHf"), Ok(ParsedNoteRef::Short(id)) if id == "SkE33UTHf")
        );
        // The host is case-insensitive, with or without a scheme; the path is not.
        assert!(
            matches!(parse_note_ref("HackMD.io/s/SkE33UTHf"), Ok(ParsedNoteRef::Short(id)) if id == "SkE33UTHf")
        );
        assert!(
            matches!(parse_note_ref("HACKMD.IO/@core/slug"), Ok(ParsedNoteRef::Scoped { owner, slug }) if owner == "core" && slug == "slug")
        );
    }

    #[test]
    fn percent_encoded_and_non_ascii_slugs_are_decoded() {
        for value in [
            "https://hackmd.io/@core/%E4%B8%AD%E6%96%87",
            "hackmd.io/@core/中文",
        ] {
            assert!(
                matches!(parse_note_ref(value), Ok(ParsedNoteRef::Scoped { owner, slug }) if owner == "core" && slug == "中文"),
                "{value:?}"
            );
        }
        for value in [
            "https://hackmd.io/@core/%E4%B8",
            "https://hackmd.io/@core/%zz",
            "https://hackmd.io/@core/%+1",
            "https://hackmd.io/@core/%+A",
        ] {
            assert!(parse_note_ref(value).is_err(), "accepted {value:?}");
        }
    }

    #[test]
    fn rejects_empty_foreign_and_malformed_urls() {
        for value in [
            "",
            "https://example.com/id",
            "http://hackmd.io/id",
            "https://hackmd.io/@owner",
            "https://hackmd.io/@core/slug/extra",
            "https://hackmd.io/s",
            "https://hackmd.io/s/",
            // Only a lowercase `download` is known to be the published copy.
            "https://hackmd.io/api-id/Download",
            "https://hackmd.io/s/SkE33UTHf/Download",
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
            let result = resolve_note_ref(&client, None, value, false)
                .await
                .expect("direct reference should resolve without an API token");
            assert!(
                matches!(&result, NoteResolution::Resolved { note } if note.workspace == Workspace::Personal && !note.note_id.is_empty())
            );
        }
        let core = Workspace::Team {
            team_path: "core".to_owned(),
        };
        let result = resolve_note_ref(&client, Some(core.clone()), "internal-id", false)
            .await
            .expect("a direct ID should resolve");
        assert!(matches!(&result, NoteResolution::Resolved { note } if note.workspace == core));
        // With no team_path, push reads a direct ID in the tracked note's own
        // workspace; a given team_path still wins.
        for (team_path, expected) in [
            (None, &core),
            (Some(Workspace::Personal), &Workspace::Personal),
        ] {
            let result = super::resolve_note_ref_from(
                &client,
                team_path,
                core.clone(),
                "internal-id",
                false,
            )
            .await
            .expect("a direct ID should resolve");
            assert!(
                matches!(&result, NoteResolution::Resolved { note } if note.workspace == *expected)
            );
        }
    }

    #[tokio::test]
    async fn an_explicit_personal_workspace_contradicting_the_url_is_refused() {
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/me",
            200,
            r#"{"id":"u","name":"User","userPath":"alice"}"#,
        )]);
        let error = resolve_note_ref(
            &fixture.client(),
            Some(Workspace::Personal),
            "https://hackmd.io/@core/slug",
            false,
        )
        .await
        .expect_err("a personal team_path cannot name @core's note");
        assert_eq!(
            error.to_string(),
            "team_path names the personal workspace, but the URL's owner @core is not this account; omit team_path, since the URL names the workspace"
        );
        fixture.finish();
    }

    #[tokio::test]
    async fn scoped_urls_resolve_personal_user_path_before_team_paths() {
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/me",
                200,
                r#"{"id":"u","name":"User","email":"u@example.com","userPath":"alice"}"#,
            ),
            Scenario::new(
                "GET",
                "/v1/notes",
                200,
                r#"[{"id":"personal-id","title":"Personal","permalink":"slug"}]"#,
            ),
        ]);
        let client = fixture.client();
        let result = resolve_note_ref(&client, None, "https://hackmd.io/@alice/slug", false)
            .await
            .expect("personal URL should resolve");
        assert!(
            matches!(result, NoteResolution::Resolved { note } if note.workspace == Workspace::Personal && note.note_id == "personal-id")
        );
        fixture.finish();
    }

    /// A `team_path` that contradicts the URL is refused before any request.
    #[tokio::test]
    async fn a_team_path_contradicting_the_url_is_refused() {
        let client = crate::client::HackmdClient::new(crate::config::Config::for_tests())
            .expect("client should build");
        let error = resolve_note_ref(
            &client,
            Some(Workspace::Team {
                team_path: "other".to_owned(),
            }),
            "https://hackmd.io/@alice/slug",
            false,
        )
        .await
        .expect_err("a contradicting team_path should be refused");
        assert_eq!(
            error.to_string(),
            "team_path \"other\" does not match the URL's owner @alice; omit team_path, since the URL names the workspace"
        );
    }

    #[tokio::test]
    async fn scoped_url_refresh_bypasses_a_cached_workspace_list() {
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/me",
                200,
                r#"{"id":"u","name":"User","userPath":"alice"}"#,
            ),
            Scenario::new(
                "GET",
                "/v1/notes",
                200,
                r#"[{"id":"old","title":"Old","permalink":"slug"}]"#,
            ),
            Scenario::new(
                "GET",
                "/v1/me",
                200,
                r#"{"id":"u","name":"User","userPath":"alice"}"#,
            ),
            Scenario::new(
                "GET",
                "/v1/notes",
                200,
                r#"[{"id":"new","title":"New","permalink":"slug"}]"#,
            ),
        ]);
        let client = fixture.client_with_cache();

        let first = resolve_note_ref(&client, None, "https://hackmd.io/@alice/slug", false)
            .await
            .expect("first URL should resolve");
        let refreshed = resolve_note_ref(&client, None, "https://hackmd.io/@alice/slug", true)
            .await
            .expect("refreshed URL should resolve");

        assert!(matches!(first, NoteResolution::Resolved { note } if note.note_id == "old"));
        assert!(matches!(refreshed, NoteResolution::Resolved { note } if note.note_id == "new"));
        fixture.finish();
    }

    #[tokio::test]
    async fn repeated_scoped_urls_reuse_the_cached_account_and_lists() {
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/me",
                200,
                r#"{"id":"u","name":"User","userPath":"alice"}"#,
            ),
            Scenario::new(
                "GET",
                "/v1/teams",
                200,
                r#"[{"id":"t","name":"Core","path":"core"}]"#,
            ),
            Scenario::new(
                "GET",
                "/v1/teams/core/notes",
                200,
                r#"[{"id":"team-id","title":"Team","shortId":"slug"}]"#,
            ),
            Scenario::new(
                "GET",
                "/v1/notes",
                200,
                r#"[{"id":"mine","title":"Mine","permalink":"slug"}]"#,
            ),
        ]);
        let client = fixture.client_with_cache();
        let resolve = |url: &'static str| resolve_note_ref(&client, None, url, false);

        for _ in 0..2 {
            let team = resolve("https://hackmd.io/@core/slug")
                .await
                .expect("team URL should resolve");
            assert!(matches!(team, NoteResolution::Resolved { note } if note.note_id == "team-id"));
        }
        // The cached user_path answers without another /me.
        let mine = resolve("https://hackmd.io/@alice/slug")
            .await
            .expect("personal URL should resolve");
        assert!(matches!(mine, NoteResolution::Resolved { note } if note.note_id == "mine"));
        // A fresh cached team list also answers "not a member".
        assert!(matches!(
            resolve("https://hackmd.io/@stranger/slug")
                .await
                .expect("unknown owner should resolve to not found"),
            NoteResolution::NotFound { .. }
        ));

        assert_eq!(fixture.finish().len(), 4, "one request per resource");
    }

    fn short_link_fixture() -> SequenceServer {
        SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/teams",
                200,
                r#"[{"id":"t","name":"Core","path":"core"}]"#,
            ),
            Scenario::new(
                "GET",
                "/v1/notes",
                200,
                r#"[{"id":"mine","title":"Mine","shortId":"other"}]"#,
            ),
            Scenario::new(
                "GET",
                "/v1/teams/core/notes",
                200,
                r#"[{"id":"team-id","title":"Team","shortId":"SkE33UTHf","permalink":"c-linked-list"}]"#,
            ),
        ])
    }

    #[tokio::test]
    async fn a_short_link_without_team_path_searches_personal_then_teams() {
        let fixture = short_link_fixture();
        let result = resolve_note_ref(
            &fixture.client(),
            None,
            "https://hackmd.io/s/SkE33UTHf",
            false,
        )
        .await
        .expect("short link should resolve");
        assert!(
            matches!(result, NoteResolution::Resolved { note } if note.workspace == Workspace::Team { team_path: "core".to_owned() } && note.note_id == "team-id")
        );
        fixture.finish();
    }

    #[tokio::test]
    async fn a_short_link_with_team_path_lists_only_that_team() {
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/teams/core/notes",
            200,
            r#"[{"id":"team-id","title":"Team","shortId":"SkE33UTHf"}]"#,
        )]);
        let result = resolve_note_ref(
            &fixture.client(),
            Some(Workspace::Team {
                team_path: "core".to_owned(),
            }),
            "https://hackmd.io/s/missing",
            false,
        )
        .await
        .expect("lookup should answer");
        assert!(matches!(result, NoteResolution::NotFound { query } if query == "s/missing"));
        fixture.finish();
    }

    #[tokio::test]
    async fn a_short_link_with_an_explicit_personal_workspace_lists_only_it() {
        // Only the older tagged form can say "personal" explicitly; teams are
        // then not listed at all.
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/notes",
            200,
            r#"[{"id":"mine","title":"Mine","shortId":"SkE33UTHf"}]"#,
        )]);
        let team_path = serde_json::from_value(serde_json::json!({"kind": "personal"}))
            .expect("the tagged personal form should deserialize");
        let result = resolve_note_ref(
            &fixture.client(),
            Some(team_path),
            "https://hackmd.io/s/SkE33UTHf",
            false,
        )
        .await
        .expect("short link should resolve");
        assert!(
            matches!(result, NoteResolution::Resolved { note } if note.workspace == Workspace::Personal && note.note_id == "mine")
        );
        assert_eq!(fixture.finish().len(), 1, "no team is listed");
    }

    #[tokio::test]
    async fn a_short_id_in_two_workspaces_is_ambiguous() {
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/teams",
                200,
                r#"[{"id":"t","name":"Core","path":"core"}]"#,
            ),
            Scenario::new(
                "GET",
                "/v1/notes",
                200,
                r#"[{"id":"mine","title":"Mine","shortId":"dup"}]"#,
            ),
            Scenario::new(
                "GET",
                "/v1/teams/core/notes",
                200,
                r#"[{"id":"team-id","title":"Team","shortId":"dup"}]"#,
            ),
        ]);
        let result = resolve_note_ref(&fixture.client(), None, "hackmd.io/s/dup", false)
            .await
            .expect("lookup should answer");
        assert!(
            matches!(&result, NoteResolution::Ambiguous { candidates, .. } if candidates.len() == 2),
            "{result:?}"
        );
        fixture.finish();
    }

    #[tokio::test]
    async fn a_short_download_link_to_a_personal_note_resolves_there() {
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new("GET", "/v1/teams", 200, "[]"),
            Scenario::new(
                "GET",
                "/v1/notes",
                200,
                r#"[{"id":"mine","title":"Mine","shortId":"SkE33UTHf"}]"#,
            ),
        ]);
        let result = resolve_note_ref(
            &fixture.client(),
            None,
            "https://hackmd.io/s/SkE33UTHf/download",
            false,
        )
        .await
        .expect("short link should resolve");
        assert!(
            matches!(result, NoteResolution::Resolved { note } if note.workspace == Workspace::Personal && note.note_id == "mine")
        );
        fixture.finish();
    }

    #[tokio::test]
    async fn a_short_id_twice_in_one_workspace_is_ambiguous() {
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/teams/core/notes",
            200,
            r#"[{"id":"a","title":"A","shortId":"dup"},{"id":"b","title":"B","shortId":"dup"}]"#,
        )]);
        let result = resolve_note_ref(
            &fixture.client(),
            Some(Workspace::Team {
                team_path: "core".to_owned(),
            }),
            "https://hackmd.io/s/dup",
            false,
        )
        .await
        .expect("lookup should answer");
        assert!(
            matches!(&result, NoteResolution::Ambiguous { query, candidates } if query == "s/dup" && candidates.len() == 2),
            "{result:?}"
        );
        fixture.finish();
    }

    #[tokio::test]
    async fn repeated_short_links_reuse_the_cache_until_refresh() {
        let teams = r#"[{"id":"t","name":"Core","path":"core"}]"#;
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new("GET", "/v1/teams", 200, teams),
            Scenario::new(
                "GET",
                "/v1/notes",
                200,
                r#"[{"id":"old","title":"Old","shortId":"SkE33UTHf"}]"#,
            ),
            Scenario::new("GET", "/v1/teams/core/notes", 200, "[]"),
            Scenario::new("GET", "/v1/teams", 200, teams),
            Scenario::new(
                "GET",
                "/v1/notes",
                200,
                r#"[{"id":"new","title":"New","shortId":"SkE33UTHf"}]"#,
            ),
            Scenario::new("GET", "/v1/teams/core/notes", 200, "[]"),
        ]);
        let client = fixture.client_with_cache();
        let resolve =
            |refresh| resolve_note_ref(&client, None, "https://hackmd.io/s/SkE33UTHf", refresh);
        for _ in 0..2 {
            let cached = resolve(false).await.expect("short link should resolve");
            assert!(matches!(cached, NoteResolution::Resolved { note } if note.note_id == "old"));
        }
        let refreshed = resolve(true).await.expect("short link should resolve");
        assert!(matches!(refreshed, NoteResolution::Resolved { note } if note.note_id == "new"));
        assert_eq!(
            fixture.finish().len(),
            6,
            "one request per resource per refresh"
        );
    }

    #[tokio::test]
    async fn a_failed_team_listing_says_team_path_narrows_the_search() {
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/teams",
                200,
                r#"[{"id":"t","name":"Core","path":"core"}]"#,
            ),
            Scenario::new(
                "GET",
                "/v1/notes",
                200,
                r#"[{"id":"mine","title":"Mine","shortId":"SkE33UTHf"}]"#,
            ),
            Scenario::new(
                "GET",
                "/v1/teams/core/notes",
                403,
                r#"{"error":"forbidden"}"#,
            ),
        ]);
        let error = resolve_note_ref(
            &fixture.client(),
            None,
            "https://hackmd.io/s/SkE33UTHf",
            false,
        )
        .await
        .expect_err("a team that cannot be listed may hold the note");
        assert!(
            matches!(&error, super::NoteRefError::ShortLinkTeam { short_id, .. } if short_id == "SkE33UTHf")
        );
        // The hint keeps the kind of the error it wraps.
        assert_eq!(
            crate::reply::ToolError::kind(&error),
            crate::reply::ErrorKind::Forbidden
        );
        assert!(
            error
                .to_string()
                .ends_with("; this came from a team listing while searching every workspace for s/SkE33UTHf, so pass team_path to search only the team that holds the note"),
            "{error}"
        );
        fixture.finish();
    }

    #[tokio::test]
    async fn scoped_team_slug_resolves_through_team_discovery() {
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/me",
                200,
                r#"{"id":"u","name":"User","email":"u@example.com","userPath":"alice"}"#,
            ),
            Scenario::new(
                "GET",
                "/v1/teams",
                200,
                r#"[{"id":"t","name":"Core","path":"core"}]"#,
            ),
            Scenario::new(
                "GET",
                "/v1/teams/core/notes",
                200,
                r#"[{"id":"team-id","title":"Team","shortId":"slug"}]"#,
            ),
        ]);
        let client = fixture.client();
        let result = resolve_note_ref(&client, None, "https://hackmd.io/@core/slug", false)
            .await
            .expect("team URL should resolve");
        assert!(
            matches!(result, NoteResolution::Resolved { note } if note.workspace == Workspace::Team { team_path: "core".to_owned() } && note.note_id == "team-id")
        );
        fixture.finish();
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
