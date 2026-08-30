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

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListNotesInput {
    /// Personal account or team whose notes should be listed.
    #[serde(default)]
    pub(crate) workspace: Workspace,
    /// Maximum notes returned after filtering (default 20, maximum 100).
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1, max = 100))]
    pub(crate) limit: usize,
    /// Number of filtered notes to skip before returning the page.
    #[serde(default)]
    pub(crate) offset: usize,
    /// Case-insensitive metadata search over title, description, tags, ID, and
    /// shortId.
    pub(crate) query: Option<String>,
    /// Require every supplied tag, matched case-insensitively.
    #[serde(default)]
    pub(crate) tags: Vec<String>,
    /// Bypass the 60-second workspace cache and fetch a new list from `HackMD`.
    #[serde(default)]
    pub(crate) refresh: bool,
    /// Deterministic note ordering (default `last_changed_desc`).
    #[serde(default)]
    #[schemars(default = "default_sort")]
    pub(crate) sort: NoteSort,
}

const fn default_sort() -> NoteSort {
    NoteSort::LastChangedDesc
}

#[derive(
    Debug, Clone, Copy, Default, Deserialize, Serialize, schemars::JsonSchema, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NoteSort {
    #[default]
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
#[serde(rename_all = "camelCase")]
pub(crate) struct NoteSummary {
    pub(crate) id: String,
    pub(crate) short_id: Option<String>,
    pub(crate) title: String,
    pub(crate) description: Option<String>,
    pub(crate) tags: Vec<String>,
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

#[derive(Debug, Error)]
pub(crate) enum ListNotesError {
    #[error(transparent)]
    Limit(#[from] InvalidLimit),
    #[error(transparent)]
    Api(#[from] crate::client::HackmdError),
}

pub(crate) async fn list_notes(
    client: &HackmdClient,
    input: ListNotesInput,
) -> Result<ListNotesOutput, ListNotesError> {
    validate_limit(input.limit)?;
    let notes = client.list_notes(&input.workspace, input.refresh).await?;
    Ok(filter_sort_page(&notes, &input))
}

fn filter_sort_page(notes: &[NoteResponse], input: &ListNotesInput) -> ListNotesOutput {
    let normalized_query = input
        .query
        .as_deref()
        .map(normalize)
        .filter(|q| !q.is_empty());
    let required_tags = input
        .tags
        .iter()
        .map(|tag| normalize(tag))
        .collect::<Vec<_>>();
    let mut notes = notes
        .iter()
        .filter(|note| {
            normalized_query
                .as_deref()
                .is_none_or(|query| matches_query(note, query))
                && required_tags.iter().all(|required| {
                    note.tags
                        .iter()
                        .any(|candidate| normalize(candidate) == *required)
                })
        })
        .map(|note| note_summary(note.clone(), input.workspace.clone()))
        .collect::<Vec<_>>();
    notes.sort_by(|left, right| compare_notes(left, right, input.sort));

    page_summaries(notes, input.offset, input.limit)
}

pub(crate) fn note_summary(note: NoteResponse, workspace: Workspace) -> NoteSummary {
    NoteSummary {
        id: note.id,
        short_id: note.short_id,
        title: note.title,
        description: note.description,
        tags: note.tags,
        workspace,
        created_at: note.created_at,
        last_changed_at: note.last_changed_at,
        last_visit: note.last_visit,
        publish_link: note.publish_link,
        permalink: note.permalink,
        read_permission: note.read_permission,
        write_permission: note.write_permission,
    }
}

pub(crate) fn page_summaries(
    notes: Vec<NoteSummary>,
    offset: usize,
    limit: usize,
) -> ListNotesOutput {
    let (notes, meta) = paginate(notes, offset, limit);
    ListNotesOutput { meta, notes }
}

fn normalize(value: &str) -> String {
    value.to_lowercase()
}

fn matches_query(note: &NoteResponse, query: &str) -> bool {
    normalize(&note.id).contains(query)
        || normalize(&note.title).contains(query)
        || note
            .short_id
            .as_deref()
            .is_some_and(|value| normalize(value).contains(query))
        || note
            .description
            .as_deref()
            .is_some_and(|value| normalize(value).contains(query))
        || note
            .tags
            .iter()
            .any(|value| normalize(value).contains(query))
}

fn compare_notes(left: &NoteSummary, right: &NoteSummary, sort: NoteSort) -> Ordering {
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

    use super::{ListNotesError, ListNotesInput, NoteSort, filter_sort_page, list_notes};
    use crate::{client::HackmdClient, config::Config, dto::NoteResponse, models::Workspace};

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
            workspace: Workspace::Personal,
            limit: 20,
            offset: 0,
            query: None,
            tags: Vec::new(),
            refresh: false,
            sort: NoteSort::LastChangedDesc,
        }
    }

    #[test]
    fn filters_metadata_and_requires_all_tags_case_insensitively() {
        let mut input = input();
        input.query = Some("ROAD".to_owned());
        input.tags = vec!["RUST".to_owned(), "mcp".to_owned()];
        let output = filter_sort_page(
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
    fn sorts_deterministically_then_pages_with_navigation_fields() {
        let mut input = input();
        input.limit = 2;
        input.offset = 1;
        let output = filter_sort_page(
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
        let output = filter_sort_page(&[note("a", "A", 1, &[])], &input);
        assert_eq!(output.meta.offset, 10);
        assert_eq!(output.meta.count, 0);
        assert!(!output.meta.has_more);
        assert_eq!(output.meta.next_offset, None);
    }

    #[test]
    fn slim_summary_omits_content_and_folder_paths() {
        let output = filter_sort_page(&[note("a", "A", 1, &[])], &input());
        let value = serde_json::to_value(&output).expect("output should serialize");
        let summary = &value["notes"][0];
        assert!(summary.get("content").is_none());
        assert!(summary.get("folderPaths").is_none());
        assert_eq!(summary["shortId"], "short-a");
        assert_eq!(value["next_offset"], serde_json::Value::Null);
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
