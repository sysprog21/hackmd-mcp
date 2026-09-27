use std::collections::BTreeMap;

use rmcp::schemars;
use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::Value;
use thiserror::Error;

/// Who may read or directly edit a note, from least to most permissive.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NotePermission {
    Owner,
    SignedIn,
    Guest,
}

impl NotePermission {
    const fn permissiveness(self) -> u8 {
        match self {
            Self::Owner => 0,
            Self::SignedIn => 1,
            Self::Guest => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CommentPermission {
    Disabled,
    Forbidden,
    Owners,
    SignedInUsers,
    Everyone,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SuggestEditPermission {
    Disabled,
    Forbidden,
    Owners,
    SignedInUsers,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TeamVisibility {
    Public,
    Private,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum NotePublishType {
    Edit,
    View,
    Slide,
    Book,
}

/// Fields accepted when creating a note. Every optional field is omitted when
/// absent so account and team defaults remain untouched.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateNoteRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) read_permission: Option<NotePermission>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) write_permission: Option<NotePermission>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) comment_permission: Option<CommentPermission>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) suggest_edit_permission: Option<SuggestEditPermission>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) permalink: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parent_folder_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) note_features: Option<BTreeMap<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) origin: Option<String>,
}

impl CreateNoteRequest {
    pub(crate) fn validate(&self) -> Result<(), PayloadError> {
        validate_permission_order(self.read_permission, self.write_permission)
    }
}

/// Patchable note fields. Create-only permission fields are intentionally not
/// represented, preventing callers from silently sending unsupported data.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateNoteRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) read_permission: Option<NotePermission>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) write_permission: Option<NotePermission>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) permalink: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[allow(
        clippy::option_option,
        reason = "outer None omits PATCH field; inner None serializes explicit null"
    )]
    pub(crate) parent_folder_id: Option<Option<String>>,
}

impl UpdateNoteRequest {
    /// An empty request is refused one layer up, where the tool can say
    /// which inputs would have changed something.
    pub(crate) fn validate(&self) -> Result<(), PayloadError> {
        validate_permission_order(self.read_permission, self.write_permission)
    }
}

fn validate_permission_order(
    read: Option<NotePermission>,
    write: Option<NotePermission>,
) -> Result<(), PayloadError> {
    if let (Some(read), Some(write)) = (read, write)
        && write.permissiveness() > read.permissiveness()
    {
        return Err(PayloadError::WriteMorePermissiveThanRead);
    }
    Ok(())
}

/// A PATCH input field, distinguishing "caller said nothing" from "caller asked
/// to clear this". Serde alone cannot express that: a bare `Option` collapses
/// both to `None`.
#[derive(Debug, Default)]
pub(crate) enum PatchField {
    #[default]
    Unspecified,
    Set(Option<String>),
}

impl PatchField {
    /// Whether the caller supplied the field at all, `null` included.
    pub(crate) fn is_specified(&self) -> bool {
        matches!(self, Self::Set(_))
    }

    /// Converts to the request shape: an outer `None` omits the field, an
    /// inner `None` serializes an explicit JSON null.
    #[allow(
        clippy::option_option,
        reason = "the two nesting levels are the omit/clear distinction itself"
    )]
    pub(crate) fn into_request(self) -> Option<Option<String>> {
        match self {
            Self::Unspecified => None,
            Self::Set(value) => Some(value),
        }
    }
}

pub(crate) fn deserialize_patch_field<'de, D>(deserializer: D) -> Result<PatchField, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(PatchField::Set)
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum PayloadError {
    #[error("writePermission cannot be more permissive than readPermission")]
    WriteMorePermissiveThanRead,
}

impl crate::reply::ToolError for PayloadError {
    fn kind(&self) -> crate::reply::ErrorKind {
        crate::reply::ErrorKind::InvalidInput
    }
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
/// Read in `HackMD`'s camel case, written out in snake case like every other
/// tool field.
#[serde(rename_all(deserialize = "camelCase", serialize = "snake_case"))]
pub(crate) struct ProfileResponse {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) email: Option<String>,
    pub(crate) user_path: String,
    pub(crate) photo: Option<String>,
    #[serde(default)]
    pub(crate) teams: Vec<TeamResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) upgraded: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
/// Read in `HackMD`'s camel case, written out in snake case like every other
/// tool field.
#[serde(rename_all(deserialize = "camelCase", serialize = "snake_case"))]
pub(crate) struct TeamResponse {
    pub(crate) id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) owner_id: Option<String>,
    pub(crate) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) logo: Option<String>,
    pub(crate) path: String,
    pub(crate) description: Option<String>,
    pub(crate) hard_limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) visibility: Option<TeamVisibility>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_millis",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) upgraded: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
/// Read in `HackMD`'s camel case, written out in snake case like every other
/// tool field.
#[serde(rename_all(deserialize = "camelCase", serialize = "snake_case"))]
pub(crate) struct SimpleUserProfileResponse {
    pub(crate) name: String,
    pub(crate) user_path: String,
    pub(crate) photo: String,
    pub(crate) biography: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
/// A note as `HackMD` returns it. Only ever read: tools answer with their own
/// output types built from it.
#[serde(rename_all = "camelCase")]
pub(crate) struct NoteResponse {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) short_id: Option<String>,
    pub(crate) publish_link: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_millis")]
    pub(crate) created_at: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_optional_millis")]
    pub(crate) last_changed_at: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_optional_millis")]
    pub(crate) title_updated_at: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_optional_millis")]
    pub(crate) tags_updated_at: Option<i64>,
    pub(crate) last_change_user: Option<SimpleUserProfileResponse>,
    pub(crate) publish_type: Option<NotePublishType>,
    #[serde(default, deserialize_with = "deserialize_optional_millis")]
    pub(crate) published_at: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_optional_millis")]
    pub(crate) last_visit: Option<i64>,
    pub(crate) read_permission: Option<NotePermission>,
    pub(crate) write_permission: Option<NotePermission>,
    pub(crate) comment_permission: Option<CommentPermission>,
    pub(crate) content: Option<String>,
    #[serde(default)]
    pub(crate) tags: Vec<String>,
    pub(crate) description: Option<String>,
    pub(crate) permalink: Option<String>,
    pub(crate) user_path: Option<String>,
    pub(crate) team_path: Option<String>,
    #[serde(default)]
    pub(crate) folder_paths: Vec<FolderPathResponse>,
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "finite in-range OpenAPI double milliseconds are deliberately rounded to integer milliseconds"
)]
fn deserialize_optional_millis<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<f64>::deserialize(deserializer)?;
    value
        .map(|value| {
            if !value.is_finite()
                || !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&value)
            {
                return Err(de::Error::custom(
                    "timestamp must be a finite i64 millisecond value",
                ));
            }
            Ok(value.round() as i64)
        })
        .transpose()
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub(crate) enum HistoryResponse {
    Bare(Vec<NoteResponse>),
    Wrapped { history: Vec<NoteResponse> },
}

impl HistoryResponse {
    pub(crate) fn into_notes(self) -> Vec<NoteResponse> {
        match self {
            Self::Bare(notes) | Self::Wrapped { history: notes } => notes,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
/// One ancestor folder of a note. `HackMD` also sends its name, parent, icon,
/// and color; only the ID is ever used, so only the ID is kept.
pub(crate) struct FolderPathResponse {
    pub(crate) id: String,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
/// Read in `HackMD`'s camel case, written out in snake case like every other
/// tool field.
#[serde(rename_all(deserialize = "camelCase", serialize = "snake_case"))]
pub(crate) struct FolderResponse {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) description: Option<String>,
    pub(crate) icon: Option<String>,
    pub(crate) color: Option<String>,
    pub(crate) parent_folder_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_millis")]
    pub(crate) created_at: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_optional_millis")]
    pub(crate) updated_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ImageUploadResponse {
    pub(crate) data: ImageUploadData,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ImageUploadData {
    pub(crate) link: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateFolderRequest {
    pub(crate) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parent_folder_id: Option<String>,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateFolderRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[allow(clippy::option_option, reason = "outer None omits; inner None clears")]
    pub(crate) description: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[allow(clippy::option_option, reason = "outer None omits; inner None clears")]
    pub(crate) icon: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[allow(clippy::option_option, reason = "outer None omits; inner None clears")]
    pub(crate) color: Option<Option<String>>,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::{
        CommentPermission, CreateNoteRequest, NotePermission, NoteResponse, PayloadError,
        ProfileResponse, SuggestEditPermission, TeamResponse, UpdateNoteRequest,
    };

    #[test]
    fn note_timestamps_accept_integer_or_double_milliseconds() {
        let note: super::NoteResponse = serde_json::from_value(json!({
            "id": "note",
            "title": "Timestamp",
            "createdAt": 1_710_000_000_000.0,
            "lastChangedAt": 1_710_000_000_001_i64,
            "lastVisit": null
        }))
        .expect("OpenAPI double timestamps should deserialize");
        assert_eq!(note.created_at, Some(1_710_000_000_000));
        assert_eq!(note.last_changed_at, Some(1_710_000_000_001));
        assert_eq!(note.last_visit, None);
    }

    #[test]
    fn profile_accepts_an_absent_or_null_email() {
        for value in [
            json!({"id":"user","name":"User","userPath":"user"}),
            json!({"id":"user","name":"User","userPath":"user","email":null}),
        ] {
            let profile: ProfileResponse =
                serde_json::from_value(value).expect("email is optional in the current API model");
            assert_eq!(profile.email, None);
        }
    }

    #[test]
    fn absent_create_fields_do_not_override_account_defaults() {
        assert_eq!(
            serde_json::to_value(CreateNoteRequest::default())
                .expect("create payload should serialize"),
            json!({})
        );
    }

    #[test]
    fn create_payload_uses_camel_case_and_permission_wire_values() {
        let payload = CreateNoteRequest {
            title: Some("Title".to_owned()),
            content: Some("Body".to_owned()),
            tags: Some(vec!["rust".to_owned()]),
            description: Some("Description".to_owned()),
            read_permission: Some(NotePermission::Guest),
            write_permission: Some(NotePermission::Owner),
            comment_permission: Some(CommentPermission::Everyone),
            suggest_edit_permission: Some(SuggestEditPermission::SignedInUsers),
            permalink: Some("custom-link".to_owned()),
            parent_folder_id: Some("folder-id".to_owned()),
            note_features: Some(BTreeMap::from([("math".to_owned(), json!(true))])),
            origin: Some("hackmd-mcp".to_owned()),
        };

        payload
            .validate()
            .expect("permission ordering should be valid");
        assert_eq!(
            serde_json::to_value(payload).expect("create payload should serialize"),
            json!({
                "title": "Title",
                "content": "Body",
                "tags": ["rust"],
                "description": "Description",
                "readPermission": "guest",
                "writePermission": "owner",
                "commentPermission": "everyone",
                "suggestEditPermission": "signed_in_users",
                "permalink": "custom-link",
                "parentFolderId": "folder-id",
                "noteFeatures": {"math": true},
                "origin": "hackmd-mcp"
            })
        );
    }

    #[test]
    fn explicit_empty_content_is_kept_and_absent_fields_are_omitted() {
        let payload = UpdateNoteRequest {
            content: Some(String::new()),
            ..UpdateNoteRequest::default()
        };
        payload
            .validate()
            .expect("explicit empty content is a real patch");
        assert_eq!(
            serde_json::to_value(payload).expect("patch should serialize"),
            json!({"content": ""})
        );

        let clear_folder = UpdateNoteRequest {
            parent_folder_id: Some(None),
            ..UpdateNoteRequest::default()
        };
        assert_eq!(
            serde_json::to_value(clear_folder).expect("null folder patch should serialize"),
            json!({"parentFolderId": null})
        );
    }

    #[test]
    fn permission_order_rejects_more_public_writes() {
        for (read, write) in [
            (NotePermission::Owner, NotePermission::SignedIn),
            (NotePermission::Owner, NotePermission::Guest),
            (NotePermission::SignedIn, NotePermission::Guest),
        ] {
            let payload = UpdateNoteRequest {
                read_permission: Some(read),
                write_permission: Some(write),
                ..UpdateNoteRequest::default()
            };
            assert_eq!(
                payload.validate(),
                Err(PayloadError::WriteMorePermissiveThanRead)
            );
        }

        let create = CreateNoteRequest {
            read_permission: Some(NotePermission::Owner),
            write_permission: Some(NotePermission::Guest),
            ..CreateNoteRequest::default()
        };
        assert_eq!(
            create.validate(),
            Err(PayloadError::WriteMorePermissiveThanRead)
        );
    }

    #[test]
    fn permission_order_accepts_equal_or_more_restricted_writes() {
        for (read, write) in [
            (NotePermission::Owner, NotePermission::Owner),
            (NotePermission::SignedIn, NotePermission::Owner),
            (NotePermission::Guest, NotePermission::SignedIn),
            (NotePermission::Guest, NotePermission::Owner),
        ] {
            let payload = CreateNoteRequest {
                read_permission: Some(read),
                write_permission: Some(write),
                ..CreateNoteRequest::default()
            };
            payload.validate().expect("permission pair should be valid");
        }
    }

    #[test]
    fn typed_profile_team_and_note_fixtures_deserialize() {
        let profile: ProfileResponse = serde_json::from_value(json!({
            "id": "user-id",
            "name": "Alice",
            "email": "alice@example.test",
            "userPath": "alice",
            "photo": null,
            "upgraded": true,
            "teams": [{
                "id": "team-id",
                "ownerId": "user-id",
                "name": "Engineering",
                "logo": "https://example.test/logo.png",
                "path": "engineering",
                "description": "Team",
                "hardLimit": 100,
                "visibility": "private",
                "createdAt": 1.25,
                "upgraded": true
            }]
        }))
        .expect("profile fixture should deserialize");
        assert_eq!(profile.user_path, "alice");
        assert_eq!(profile.teams[0].path, "engineering");
        assert_eq!(profile.teams[0].created_at, Some(1));

        let teams: Vec<TeamResponse> = serde_json::from_value(json!([{
            "id": "team-id",
            "name": "Engineering",
            "path": "engineering",
            "description": null,
            "hardLimit": 100,
            "visibility": "private"
        }]))
        .expect("team fixture should deserialize");
        assert_eq!(teams[0].path, "engineering");

        let note: NoteResponse = serde_json::from_value(json!({
            "id": "note-id",
            "title": "Typed note",
            "shortId": "short",
            "publishLink": "https://hackmd.io/short",
            "createdAt": 1,
            "lastChangedAt": 2,
            "titleUpdatedAt": 2.4,
            "tagsUpdatedAt": null,
            "lastChangeUser": {
                "name": "Alice",
                "userPath": "alice",
                "photo": "https://example.test/alice.png",
                "biography": null
            },
            "publishType": "edit",
            "publishedAt": 3.6,
            "readPermission": "guest",
            "writePermission": "owner",
            "commentPermission": "everyone",
            "content": "# Body",
            "tags": ["rust"],
            "description": null,
            "permalink": "typed-note",
            "userPath": "alice",
            "teamPath": null,
            "folderPaths": [{
                "id": "folder-id",
                "name": "Folder",
                "parentId": null,
                "icon": null,
                "color": "#ffffff",
                "clientId": "client-folder-id"
            }]
        }))
        .expect("note fixture should deserialize");
        assert_eq!(note.read_permission, Some(NotePermission::Guest));
        assert_eq!(note.folder_paths[0].id, "folder-id");
        assert_eq!(note.title_updated_at, Some(2));
        assert_eq!(note.published_at, Some(4));
    }

    #[test]
    fn permission_enums_reject_unknown_wire_values() {
        assert!(serde_json::from_value::<NotePermission>(json!("public")).is_err());
        assert!(serde_json::from_value::<CommentPermission>(json!("all")).is_err());
        assert!(serde_json::from_value::<SuggestEditPermission>(json!("everyone")).is_err());
    }
}
