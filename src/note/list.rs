use std::cmp::Ordering;

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

use crate::{
    client::HackmdClient,
    dto::NoteResponse,
    local::LocalFiles,
    models::Workspace,
    paging::{InvalidLimit, PageMeta, contains_folded, default_limit, paginate, validate_limit},
    sync::tracking::{
        ListTrackedNotesInput, ListTrackedNotesOutput, TrackingError, list_tracked_notes,
    },
};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListNotesInput {
    /// Which notes to list (default `workspace`).
    #[serde(default)]
    pub(crate) source: NoteSource,
    /// Team path (from `hackmd_get_me`) of a team workspace; omit for the
    /// personal workspace. History and trash are account-wide and take no
    /// team; for `tracked` a team narrows the records to that team's notes,
    /// and omitting it lists every workspace's.
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
    /// `last_changed_desc`, and history and trash keep `HackMD`'s own order.
    /// Whether history and trash entries carry tags, descriptions, and dates
    /// is unmeasured. An entry without tags never passes a `tags` filter;
    /// `query` still matches whatever fields it has, its ID included; and a
    /// date sort puts entries without that date last when descending, first
    /// when ascending, ordered among themselves by ID.
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
    /// Trashed personal notes; `hackmd_delete_note` with `restore: true`
    /// brings one back.
    Trash,
    /// Local Markdown files tracked by `hackmd_pull_note`, with the note each
    /// syncs to and its baseline hash. Read from local state only: `query`
    /// matches the note ID or path, `team_path` narrows to one team, and
    /// `tags`, `sort`, and `refresh` do not apply.
    Tracked,
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
    /// See [`crate::note::get::note_url`]. Trashed notes, whose link answers
    /// 404, carry none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) note_url: Option<String>,
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
    pub(crate) fn new(site: Option<&Url>, note: &NoteResponse, workspace: Workspace) -> Self {
        Self {
            id: note.id.clone(),
            note_url: crate::note::get::note_url(site, &note.id),
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
    #[error("source tracked lists local sync records; tags, sort, and refresh do not apply")]
    TrackedFilters,
    #[error("source {0} is always fetched fresh; refresh only applies to source workspace")]
    UncachedRefresh(&'static str),
    #[error(transparent)]
    Tracking(#[from] TrackingError),
    #[error(transparent)]
    Api(#[from] crate::client::HackmdError),
}

impl crate::reply::ToolError for ListNotesError {
    fn kind(&self) -> crate::reply::ErrorKind {
        use crate::reply::ErrorKind;

        match self {
            Self::Limit(..)
            | Self::AccountWideSource(..)
            | Self::TrackedFilters
            | Self::UncachedRefresh(..) => ErrorKind::InvalidInput,
            Self::Tracking(error) => error.kind(),
            Self::Api(error) => error.kind(),
        }
    }
}

/// A remote note list, or the local tracked records: the same page shape
/// either way, with different entries.
/// Tagged with `mode` (`notes` or `tracked`) so a caller can tell the two
/// entry shapes apart without guessing.
#[derive(Debug, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub(crate) enum ListOutput {
    Notes(ListNotesOutput),
    Tracked(ListTrackedNotesOutput),
}

impl ListOutput {
    pub(crate) fn meta(&self) -> &PageMeta {
        match self {
            Self::Notes(output) => &output.meta,
            Self::Tracked(output) => &output.meta,
        }
    }
}

fn list_tracked(
    files: &LocalFiles,
    input: &ListNotesInput,
) -> Result<ListTrackedNotesOutput, ListNotesError> {
    if !input.tags.is_empty() || input.sort.is_some() || input.refresh {
        return Err(ListNotesError::TrackedFilters);
    }

    // An omitted team lists every workspace's records, not only personal ones:
    // `Workspace` cannot tell "omitted" from "personal", and listing everything
    // is what a caller without a team means here.
    Ok(list_tracked_notes(
        files,
        &ListTrackedNotesInput {
            limit: input.limit,
            offset: input.offset,
            query: input.query.as_deref(),
            team: Some(&input.workspace).filter(|team| **team != Workspace::Personal),
        },
    )?)
}

pub(crate) async fn list_notes(
    client: &HackmdClient,
    files: &LocalFiles,
    input: ListNotesInput,
) -> Result<ListOutput, ListNotesError> {
    if !matches!(input.source, NoteSource::Tracked) {
        validate_limit(input.limit)?;
    }
    // History and trash are account-wide and never cached.
    let account_wide = |name| {
        if input.workspace != Workspace::Personal {
            Err(ListNotesError::AccountWideSource(name))
        } else if input.refresh {
            Err(ListNotesError::UncachedRefresh(name))
        } else {
            Ok(())
        }
    };

    // Only a workspace list has a default order; history and trash keep the
    // order `HackMD` returns unless a sort is asked for.
    let sort = if matches!(input.source, NoteSource::Workspace) {
        Some(input.sort.unwrap_or(NoteSort::LastChangedDesc))
    } else {
        input.sort
    };
    let site = client.site_url();
    match input.source {
        NoteSource::Workspace => {
            let notes = client.list_notes(&input.workspace, input.refresh).await?;
            Ok(filter_sort_page(site, &notes, &input, sort, |_| {
                input.workspace.clone()
            }))
        }
        NoteSource::History => {
            account_wide("history")?;
            let notes = client.get_history().await?;
            Ok(filter_sort_page(site, &notes, &input, sort, |note| {
                note.team_path
                    .as_ref()
                    .map_or(Workspace::Personal, |team_path| Workspace::Team {
                        team_path: team_path.clone(),
                    })
            }))
        }
        NoteSource::Tracked => return list_tracked(files, &input).map(ListOutput::Tracked),
        NoteSource::Trash => {
            account_wide("trash")?;
            let notes = client.list_trash().await?;
            // A trashed note's link answers 404, so it gets none.
            Ok(filter_sort_page(None, &notes, &input, sort, |_| {
                Workspace::Personal
            }))
        }
    }
    .map(ListOutput::Notes)
}

/// Filters, optionally sorts, and pages `notes`. `sort` is the resolved order,
/// not `input.sort`. Only the page is turned into summaries, so a large
/// workspace costs one clone per returned note.
fn filter_sort_page(
    site: Option<&Url>,
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
            .map(|note| NoteSummary::new(site, note, workspace_of(note)))
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
        ListNotesError, ListNotesInput, ListNotesOutput, ListOutput, NoteSort, NoteSource,
        filter_sort_page, list_notes,
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

    /// A remote source, which must come back as a note list.
    async fn remote(
        client: &HackmdClient,
        input: ListNotesInput,
    ) -> Result<ListNotesOutput, ListNotesError> {
        match list_notes(client, &crate::fixture::scratch_files(), input).await? {
            ListOutput::Notes(output) => Ok(output),
            ListOutput::Tracked(_) => panic!("a remote source listed tracked records"),
        }
    }

    #[tokio::test]
    async fn tracked_source_lists_local_records_and_refuses_remote_filters() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        std::fs::write(&local_path, "baseline").expect("local fixture should write");
        let files =
            crate::fixture::tracked_files(directory.path(), "note-id", &local_path, "baseline");
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        let mut tracked = input();
        tracked.source = NoteSource::Tracked;
        tracked.query = Some("NOTE-ID".to_owned());
        let ListOutput::Tracked(output) = list_notes(&client, &files, tracked)
            .await
            .expect("tracked records should list without a request")
        else {
            panic!("the tracked source should list tracked records");
        };
        assert_eq!(output.notes.len(), 1);
        assert_eq!(output.notes[0].note_id, "note-id");

        // A team narrows the records; this one is personal.
        let mut other_team = input();
        other_team.source = NoteSource::Tracked;
        other_team.workspace = Workspace::Team {
            team_path: "core".to_owned(),
        };
        let ListOutput::Tracked(output) = list_notes(&client, &files, other_team)
            .await
            .expect("a team filter should list")
        else {
            panic!("the tracked source should list tracked records");
        };
        assert_eq!(output.notes, []);

        for refuse in [
            |input: &mut ListNotesInput| input.sort = Some(NoteSort::TitleAsc),
            |input: &mut ListNotesInput| input.tags = vec!["t".to_owned()],
            |input: &mut ListNotesInput| input.refresh = true,
        ] {
            let mut filtered = input();
            filtered.source = NoteSource::Tracked;
            refuse(&mut filtered);
            assert!(matches!(
                list_notes(&client, &files, filtered).await,
                Err(ListNotesError::TrackedFilters)
            ));
        }
    }

    fn page(notes: &[NoteResponse], input: &ListNotesInput) -> ListNotesOutput {
        filter_sort_page(None, notes, input, Some(NoteSort::LastChangedDesc), |_| {
            Workspace::Personal
        })
    }

    #[test]
    fn summary_links_only_when_the_site_is_known() {
        let site = crate::fixture::site();
        let linked = super::NoteSummary::new(
            Some(&site),
            &note("a", "A", 1, &[]),
            Workspace::Team {
                team_path: "team".to_owned(),
            },
        );
        let value = serde_json::to_value(linked).expect("summary should serialize");
        assert_eq!(value["note_url"], "https://hackmd.io/a");
        let unlinked = super::NoteSummary::new(None, &note("a", "A", 1, &[]), Workspace::Personal);
        let value = serde_json::to_value(unlinked).expect("summary should serialize");
        assert!(value.get("note_url").is_none());
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
                crate::paging::contains_folded(value, &needle),
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
        let output = remote(&client, history)
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
        let output = remote(&client, wrapped)
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
        let page = remote(&client, trash)
            .await
            .expect("trash page should succeed");
        assert_eq!(page.meta.total, 2);
        assert_eq!(page.notes[0].id, "b");
        assert_eq!(page.notes[0].workspace, Workspace::Personal);
        let serialized = serde_json::to_value(&page).expect("page should serialize");
        assert!(serialized["notes"][0].get("content").is_none());

        let mut trash = input();
        trash.source = NoteSource::Trash;
        let empty = remote(&client, trash)
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
                remote(&client, invalid).await,
                Err(ListNotesError::AccountWideSource(_))
            ));

            // Neither is cached, so there is nothing for refresh to bypass.
            let mut refreshed = input();
            refreshed.source = source;
            refreshed.refresh = true;
            assert!(matches!(
                remote(&client, refreshed).await,
                Err(ListNotesError::UncachedRefresh(_))
            ));
        }
    }

    #[tokio::test]
    async fn rejects_out_of_range_limit_before_network_io() {
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        let mut invalid = input();
        invalid.limit = 101;
        assert!(matches!(
            remote(&client, invalid).await,
            Err(ListNotesError::Limit(_))
        ));
    }
}
