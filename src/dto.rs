#![allow(
    dead_code,
    reason = "DTOs are consumed by the following API tool tasks"
)]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Who may read or directly edit a note, from least to most permissive.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CommentPermission {
    Disabled,
    Forbidden,
    Owners,
    SignedInUsers,
    Everyone,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SuggestEditPermission {
    Disabled,
    Forbidden,
    Owners,
    SignedInUsers,
}

/// Fields accepted when creating a note. Every optional field is omitted when
/// absent so account and team defaults remain untouched.
#[derive(Debug, Default, Serialize)]
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
#[derive(Debug, Default, Serialize)]
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
    pub(crate) parent_folder_id: Option<String>,
}

impl UpdateNoteRequest {
    pub(crate) fn validate(&self) -> Result<(), PayloadError> {
        if self.is_empty() {
            return Err(PayloadError::EmptyPatch);
        }
        validate_permission_order(self.read_permission, self.write_permission)
    }

    fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.content.is_none()
            && self.tags.is_none()
            && self.description.is_none()
            && self.read_permission.is_none()
            && self.write_permission.is_none()
            && self.permalink.is_none()
            && self.parent_folder_id.is_none()
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

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum PayloadError {
    #[error("PATCH body must contain at least one explicitly supplied field")]
    EmptyPatch,
    #[error("writePermission cannot be more permissive than readPermission")]
    WriteMorePermissiveThanRead,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProfileResponse {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) email: String,
    pub(crate) user_path: String,
    pub(crate) photo: Option<String>,
    #[serde(default)]
    pub(crate) teams: Vec<TeamResponse>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TeamResponse {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) description: Option<String>,
    pub(crate) hard_limit: Option<u64>,
    pub(crate) visibility: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NoteResponse {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) short_id: Option<String>,
    pub(crate) publish_link: Option<String>,
    pub(crate) created_at: Option<i64>,
    pub(crate) last_changed_at: Option<i64>,
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

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FolderPathResponse {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) parent_id: Option<String>,
    pub(crate) icon: Option<String>,
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
    fn empty_patch_is_rejected_and_absent_fields_are_omitted() {
        assert_eq!(
            UpdateNoteRequest::default().validate(),
            Err(PayloadError::EmptyPatch)
        );
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
            "teams": [{
                "id": "team-id",
                "name": "Engineering",
                "path": "engineering",
                "description": "Team",
                "hardLimit": 100,
                "visibility": "private"
            }]
        }))
        .expect("profile fixture should deserialize");
        assert_eq!(profile.user_path, "alice");
        assert_eq!(profile.teams[0].path, "engineering");

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
                "icon": null
            }]
        }))
        .expect("note fixture should deserialize");
        assert_eq!(note.read_permission, Some(NotePermission::Guest));
        assert_eq!(note.folder_paths[0].id, "folder-id");
    }

    #[test]
    fn permission_enums_reject_unknown_wire_values() {
        assert!(serde_json::from_value::<NotePermission>(json!("public")).is_err());
        assert!(serde_json::from_value::<CommentPermission>(json!("all")).is_err());
        assert!(serde_json::from_value::<SuggestEditPermission>(json!("everyone")).is_err());
    }
}
