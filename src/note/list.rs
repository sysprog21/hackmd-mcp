use std::cmp::Ordering;

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    client::HackmdClient,
    dto::NoteResponse,
    models::Workspace,
    paging::{InvalidLimit, PageMeta, default_limit, paginate, validate_limit},
};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListNotesInput {
    /// Which notes to list (default `workspace`).
    #[serde(default)]
    pub(crate) source: NoteSource,
    /// Team path (from `hackmd_get_me`) of a team workspace; omit for the
    /// personal workspace. Only the `workspace` source takes a team; history
    /// and trash are account-wide.
    #[serde(default, rename = "team_path", alias = "workspace")]
    pub(crate) workspace: Workspace,
    /// Maximum notes returned after filtering (default 20, maximum 100).
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1, max = 100))]
    pub(crate) limit: usize,
    /// Number of filtered notes to skip before returning the page.
    #[serde(default)]
    pub(crate) offset: usize,
    /// Case-insensitive metadata search over title, description, tags, ID, and
    /// `short_id`. Note bodies are not searched.
    pub(crate) query: Option<String>,
    /// Require every supplied tag, matched case-insensitively.
    #[serde(default)]
    pub(crate) tags: Vec<String>,
    /// Bypass the 60-second workspace cache and fetch a new list from `HackMD`.
    /// History and trash are never cached.
    #[serde(default)]
    pub(crate) refresh: bool,
    /// Note ordering. Omitted, the `workspace` source sorts by
    /// `last_changed_desc`, and history and trash keep `HackMD`'s own order
    /// (history is most recently viewed first).
    pub(crate) sort: Option<NoteSort>,
}

#[derive(
    Debug, Clone, Copy, Default, Deserialize, Serialize, schemars::JsonSchema, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NoteSource {
    /// The live notes of `workspace`.
    #[default]
    Workspace,
    /// Recently viewed notes from every workspace, with `last_visit`.
    History,
    /// Trashed personal notes; `hackmd_restore_note` brings one back.
    Trash,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NoteSort {
    LastChangedDesc,
    LastChangedAsc,
    CreatedDesc,
    CreatedAsc,
    TitleAsc,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct ListNotesOutput {
    #[serde(flatten)]
    pub(crate) meta: PageMeta,
    pub(crate) notes: Vec<NoteSummary>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct NoteSummary {
    pub(crate) id: String,
    pub(crate) short_id: Option<String>,
    pub(crate) title: String,
    pub(crate) description: Option<String>,
    pub(crate) tags: Vec<String>,
    #[serde(rename = "team_path")]
    pub(crate) workspace: Workspace,
    pub(crate) created_at: Option<i64>,
    pub(crate) last_changed_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_visit: Option<i64>,
    pub(crate) publish_link: Option<String>,
    pub(crate) permalink: Option<String>,
    pub(crate) read_permission: Option<crate::dto::NotePermission>,
    pub(crate) write_permission: Option<crate::dto::NotePermission>,
}

impl NoteSummary {
    pub(crate) fn new(note: &NoteResponse, workspace: Workspace) -> Self {
        Self {
            id: note.id.clone(),
            short_id: note.short_id.clone(),
            title: note.title.clone(),
            description: note.description.clone(),
            tags: note.tags.clone(),
            workspace,
            created_at: note.created_at,
            last_changed_at: note.last_changed_at,
            last_visit: note.last_visit,
            publish_link: note.publish_link.clone(),
            permalink: note.permalink.clone(),
            read_permission: note.read_permission,
            write_permission: note.write_permission,
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum ListNotesError {
    #[error(transparent)]
    Limit(#[from] InvalidLimit),
    #[error("source {0} is account-wide; omit team_path")]
    AccountWideSource(&'static str),
    #[error(transparent)]
    Api(#[from] crate::client::HackmdError),
}

impl crate::reply::ToolError for ListNotesError {
    fn kind(&self) -> crate::reply::ErrorKind {
        use crate::reply::ErrorKind;

        match self {
            Self::Limit(..) | Self::AccountWideSource(..) => ErrorKind::InvalidInput,
            Self::Api(error) => error.kind(),
        }
    }
}

pub(crate) async fn list_notes(
    client: &HackmdClient,
    input: ListNotesInput,
) -> Result<ListNotesOutput, ListNotesError> {
    validate_limit(input.limit)?;
    let account_wide = |name| {
        if input.workspace == Workspace::Personal {
            Ok(())
        } else {
            Err(ListNotesError::AccountWideSource(name))
        }
    };

    // Only a workspace list has a default order; history and trash keep the
    // order `HackMD` returns unless a sort is asked for.
    let sort = match input.source {
        NoteSource::Workspace => Some(input.sort.unwrap_or(NoteSort::LastChangedDesc)),
        NoteSource::History | NoteSource::Trash => input.sort,
    };
    match input.source {
        NoteSource::Workspace => {
            let notes = client.list_notes(&input.workspace, input.refresh).await?;
            Ok(filter_sort_page(&notes, &input, sort, |_| {
                input.workspace.clone()
            }))
        }
        NoteSource::History => {
            account_wide("history")?;
            let notes = client.get_history().await?;
            Ok(filter_sort_page(&notes, &input, sort, |note| {
                note.team_path
                    .as_ref()
                    .map_or(Workspace::Personal, |team_path| Workspace::Team {
                        team_path: team_path.clone(),
                    })
            }))
        }
        NoteSource::Trash => {
            account_wide("trash")?;
            let notes = client.list_trash().await?;
            Ok(filter_sort_page(&notes, &input, sort, |_| {
                Workspace::Personal
            }))
        }
    }
}

/// Filters, optionally sorts, and pages `notes`. `sort` is the resolved order,
/// not `input.sort`. Only the page is turned into summaries, so a large
/// workspace costs one clone per returned note.
fn filter_sort_page(
    notes: &[NoteResponse],
    input: &ListNotesInput,
    sort: Option<NoteSort>,
    workspace_of: impl Fn(&NoteResponse) -> Workspace,
) -> ListNotesOutput {
    let query = input
        .query
        .as_deref()
        .map(str::to_lowercase)
        .filter(|query| !query.is_empty());
    let required_tags = input
        .tags
        .iter()
        .map(|tag| tag.to_lowercase())
        .collect::<Vec<_>>();
    let mut matching = notes
        .iter()
        .filter(|note| {
            query
                .as_deref()
                .is_none_or(|query| matches_query(note, query))
                && has_tags(note, &required_tags)
        })
        .collect::<Vec<_>>();
    if let Some(sort) = sort {
        matching.sort_by(|left, right| compare_notes(left, right, sort));
    }
    let (page, meta) = paginate(matching, input.offset, input.limit);
    ListNotesOutput {
        meta,
        notes: page
            .into_iter()
            .map(|note| NoteSummary::new(note, workspace_of(note)))
            .collect(),
    }
}

fn has_tags(note: &NoteResponse, required: &[String]) -> bool {
    required
        .iter()
        .all(|required| note.tags.iter().any(|tag| equals_folded(tag, required)))
}

fn matches_query(note: &NoteResponse, query: &str) -> bool {
    let contains = |value: &str| contains_folded(value, query);
    contains(&note.id)
        || contains(&note.title)
        || note.short_id.as_deref().is_some_and(contains)
        || note.description.as_deref().is_some_and(contains)
        || note.tags.iter().any(|tag| contains(tag))
}

/// Whether `value` contains `needle`, which is already lowercase. Every note
/// field is searched on every query, so the common all-ASCII case compares in
/// place; anything else takes Unicode lowercasing, which can change length.
fn contains_folded(value: &str, needle: &str) -> bool {
    if value.is_ascii() && needle.is_ascii() {
        let (value, needle) = (value.as_bytes(), needle.as_bytes());
        return needle.is_empty()
            || value
                .windows(needle.len())
                .any(|window| window.eq_ignore_ascii_case(needle));
    }
    value.to_lowercase().contains(needle)
}

/// Whether `value` equals `lowered`, which is already lowercase, ignoring case.
fn equals_folded(value: &str, lowered: &str) -> bool {
    if value.is_ascii() && lowered.is_ascii() {
        return value.eq_ignore_ascii_case(lowered);
    }
    value.to_lowercase() == lowered
}

fn compare_notes(left: &NoteResponse, right: &NoteResponse, sort: NoteSort) -> Ordering {
    let primary = match sort {
        NoteSort::LastChangedDesc => right.last_changed_at.cmp(&left.last_changed_at),
        NoteSort::LastChangedAsc => left.last_changed_at.cmp(&right.last_changed_at),
        NoteSort::CreatedDesc => right.created_at.cmp(&left.created_at),
        NoteSort::CreatedAsc => left.created_at.cmp(&right.created_at),
        NoteSort::TitleAsc => left
            .title
            .chars()
            .flat_map(char::to_lowercase)
            .cmp(right.title.chars().flat_map(char::to_lowercase)),
    };
    primary.then_with(|| left.id.cmp(&right.id))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        ListNotesError, ListNotesInput, NoteSort, NoteSource, filter_sort_page, list_notes,
    };
    use crate::{
        client::HackmdClient,
        config::Config,
        dto::NoteResponse,
        fixture::{Scenario, SequenceServer},
        models::Workspace,
    };

    fn note(id: &str, title: &str, changed: i64, tags: &[&str]) -> NoteResponse {
        serde_json::from_value(json!({
            "id": id,
            "shortId": format!("short-{id}"),
            "title": title,
            "description": format!("description {id}"),
            "tags": tags,
            "createdAt": changed - 1,
            "lastChangedAt": changed
        }))
        .expect("note fixture should deserialize")
    }

    fn input() -> ListNotesInput {
        ListNotesInput {
            source: NoteSource::Workspace,
            workspace: Workspace::Personal,
            limit: 20,
            offset: 0,
            query: None,
            tags: Vec::new(),
            refresh: false,
            sort: None,
        }
    }

    fn page(notes: &[NoteResponse], input: &ListNotesInput) -> super::ListNotesOutput {
        filter_sort_page(notes, input, Some(NoteSort::LastChangedDesc), |_| {
            Workspace::Personal
        })
    }

    #[test]
    fn filters_metadata_and_requires_all_tags_case_insensitively() {
        let mut input = input();
        input.query = Some("ROAD".to_owned());
        input.tags = vec!["RUST".to_owned(), "mcp".to_owned()];
        let output = page(
            &[
                note("a", "Roadmap", 2, &["rust", "MCP"]),
                note("b", "Roadmap", 3, &["rust"]),
                note("c", "Other", 4, &["rust", "mcp"]),
            ],
            &input,
        );
        assert_eq!(output.meta.total, 1);
        assert_eq!(output.notes[0].id, "a");
    }

    #[test]
    fn folded_matching_agrees_with_unicode_lowercasing() {
        for (value, needle) in [
            ("Roadmap", "road"),
            ("ROADMAP", "map"),
            ("Straße", "straße"),
            ("ÉTÉ", "été"),
            ("abc", "abcd"),
            ("abc", ""),
        ] {
            let needle = needle.to_lowercase();
            assert_eq!(
                super::contains_folded(value, &needle),
                value.to_lowercase().contains(&needle),
                "{value} / {needle}"
            );
        }
        assert!(super::equals_folded("RUST", "rust"));
        assert!(super::equals_folded("ÉTÉ", "été"));
        assert!(!super::equals_folded("rusty", "rust"));
    }

    #[test]
    fn sorts_deterministically_then_pages_with_navigation_fields() {
        let mut input = input();
        input.limit = 2;
        input.offset = 1;
        let output = page(
            &[
                note("c", "C", 1, &[]),
                note("b", "B", 3, &[]),
                note("a", "A", 3, &[]),
                note("d", "D", 0, &[]),
            ],
            &input,
        );
        assert_eq!(output.meta.total, 4);
        assert_eq!(output.meta.count, 2);
        assert_eq!(output.meta.offset, 1);
        assert!(output.meta.has_more);
        assert_eq!(output.meta.next_offset, Some(3));
        assert_eq!(
            output
                .notes
                .iter()
                .map(|note| note.id.as_str())
                .collect::<Vec<_>>(),
            ["b", "c"]
        );
    }

    #[test]
    fn page_beyond_end_preserves_requested_offset() {
        let mut input = input();
        input.offset = 10;
        let output = page(&[note("a", "A", 1, &[])], &input);
        assert_eq!(output.meta.offset, 10);
        assert_eq!(output.meta.count, 0);
        assert!(!output.meta.has_more);
        assert_eq!(output.meta.next_offset, None);
    }

    #[test]
    fn slim_summary_omits_content_and_folder_paths() {
        let output = page(&[note("a", "A", 1, &[])], &input());
        let value = serde_json::to_value(&output).expect("output should serialize");
        let summary = &value["notes"][0];
        assert!(summary.get("content").is_none());
        assert!(summary.get("folder_paths").is_none());
        assert_eq!(summary["short_id"], "short-a");
        assert_eq!(value["next_offset"], serde_json::Value::Null);
    }

    #[test]
    fn materializes_only_the_requested_page() {
        let notes = (0..1_000)
            .map(|index| note(&format!("id-{index:04}"), "title", index, &["tag"]))
            .collect::<Vec<_>>();
        let mut input = input();
        input.offset = 400;
        input.limit = 3;

        let output = page(&notes, &input);

        assert_eq!(output.meta.total, 1_000);
        assert_eq!(output.notes.len(), 3);
        assert_eq!(output.notes.capacity(), 3);
    }

    #[tokio::test]
    async fn history_keeps_api_order_last_visit_and_each_note_workspace() {
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/history",
                200,
                r#"[
                    {"id":"newest","title":"Newest","lastVisit":30,"lastChangedAt":1},
                    {"id":"older","title":"Older","lastVisit":20,"lastChangedAt":9,"teamPath":"core"}
                ]"#,
            ),
            Scenario::new(
                "GET",
                "/v1/history",
                200,
                r#"{"history":[{"id":"one","title":"One","lastVisit":1}]}"#,
            ),
        ]);
        let client = fixture.client();
        let mut history = input();
        history.source = NoteSource::History;
        history.limit = 1;
        history.offset = 1;
        let output = list_notes(&client, history)
            .await
            .expect("history should succeed");
        assert_eq!(output.meta.total, 2);
        assert_eq!(output.notes[0].id, "older");
        assert_eq!(output.notes[0].last_visit, Some(20));
        assert!(matches!(
            output.notes[0].workspace,
            Workspace::Team { ref team_path } if team_path == "core"
        ));

        let mut wrapped = input();
        wrapped.source = NoteSource::History;
        let output = list_notes(&client, wrapped)
            .await
            .expect("wrapped history should succeed");
        assert_eq!(output.meta.count, 1);
        fixture.finish();
    }

    #[tokio::test]
    async fn trash_is_slim_paginated_and_accepts_empty_lists() {
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/trash",
                200,
                r#"[{"id":"a","title":"A","content":"secret","lastChangedAt":2},{"id":"b","title":"B","lastChangedAt":1}]"#,
            ),
            Scenario::new("GET", "/v1/trash", 200, "[]"),
        ]);
        let client = fixture.client();
        let mut trash = input();
        trash.source = NoteSource::Trash;
        trash.limit = 1;
        trash.offset = 1;
        let page = list_notes(&client, trash)
            .await
            .expect("trash page should succeed");
        assert_eq!(page.meta.total, 2);
        assert_eq!(page.notes[0].id, "b");
        assert_eq!(page.notes[0].workspace, Workspace::Personal);
        let serialized = serde_json::to_value(&page).expect("page should serialize");
        assert!(serialized["notes"][0].get("content").is_none());

        let mut trash = input();
        trash.source = NoteSource::Trash;
        let empty = list_notes(&client, trash)
            .await
            .expect("empty trash should succeed");
        assert_eq!(empty.meta.total, 0);
        assert_eq!(empty.meta.next_offset, None);
        fixture.finish();
    }

    #[tokio::test]
    async fn account_wide_sources_refuse_a_team_workspace_before_network_io() {
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        for source in [NoteSource::History, NoteSource::Trash] {
            let mut invalid = input();
            invalid.source = source;
            invalid.workspace = Workspace::Team {
                team_path: "core".to_owned(),
            };
            assert!(matches!(
                list_notes(&client, invalid).await,
                Err(ListNotesError::AccountWideSource(_))
            ));
        }
    }

    #[tokio::test]
    async fn rejects_out_of_range_limit_before_network_io() {
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        let mut invalid = input();
        invalid.limit = 101;
        assert!(matches!(
            list_notes(&client, invalid).await,
            Err(ListNotesError::Limit(_))
        ));
    }
}
