use std::{
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
};

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    client::{HackmdClient, HackmdError},
    local::{LocalAccessError, LocalFiles},
    models::Workspace,
    note::reference::{NoteRefError, NoteResolution},
};

const IMAGE_WARNING_BYTES: u64 = 5 * 1024 * 1024;
const IMAGE_MAX_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct UploadNoteImageInput {
    /// Accepted, as on every other tool, so older callers are not rejected
    /// outright; a team here is refused like any team note. Not advertised.
    #[serde(default, rename = "team_path", alias = "workspace")]
    #[schemars(skip)]
    pub(crate) workspace: Workspace,
    /// Internal ID of a personal note, `hackmd.io/<id>`, or a
    /// `hackmd.io/@owner/slug` URL. Only personal notes accept uploads, so a
    /// URL that names a team note is refused.
    pub(crate) note_ref: String,
    /// Bypass the 60-second account and note-list caches when resolving an
    /// `@owner/slug` URL.
    #[serde(default)]
    pub(crate) refresh: bool,
    /// Absolute path to a local image file.
    pub(crate) image_path: PathBuf,
    /// Required for files larger than 5 MiB.
    #[serde(default)]
    pub(crate) confirm_large_file: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct UploadNoteImageOutput {
    pub(crate) link: String,
}

#[derive(Debug, Error)]
pub(crate) enum UploadNoteImageError {
    #[error(transparent)]
    Access(#[from] LocalAccessError),
    #[error("image_path must be absolute")]
    RelativePath,
    #[error("image_path is not a readable regular file")]
    InvalidFile,
    #[error("image_path is not a PNG, JPEG, GIF, or WebP image")]
    UnsupportedFormat,
    #[error(
        "image is {size_bytes} bytes; files above {} MiB are refused",
        IMAGE_MAX_BYTES / 1024 / 1024
    )]
    TooLarge { size_bytes: u64 },
    #[error(
        "image is {size_bytes} bytes; retry with confirm_large_file: true or resize below {} MiB",
        IMAGE_WARNING_BYTES / 1024 / 1024
    )]
    ConfirmationRequired { size_bytes: u64 },
    #[error("team image upload is not documented by HackMD; use a personal-workspace note")]
    TeamUnsupported,
    #[error(transparent)]
    Reference(#[from] NoteRefError),
    #[error(transparent)]
    Api(#[from] HackmdError),
}

impl crate::reply::ToolError for UploadNoteImageError {
    fn kind(&self) -> crate::reply::ErrorKind {
        use crate::reply::ErrorKind;

        match self {
            Self::Access(error) => error.kind(),
            Self::RelativePath
            | Self::InvalidFile
            | Self::UnsupportedFormat
            | Self::TeamUnsupported => ErrorKind::InvalidInput,
            Self::TooLarge { .. } => ErrorKind::TooLarge,
            Self::ConfirmationRequired { .. } => ErrorKind::ConfirmationRequired,
            Self::Reference(error) => error.kind(),
            Self::Api(error) => error.kind(),
        }
    }
}

/// Checks the file's magic bytes.
///
/// The tool hands a local file to a remote CDN that answers with a public link,
/// so the file has to be what the caller says it is. Without this, one confused
/// or coerced tool call publishes a private key as readily as a screenshot.
fn ensure_supported_image(header: &[u8]) -> Result<(), UploadNoteImageError> {
    let supported = header.starts_with(b"\x89PNG\r\n\x1a\n")
        || header.starts_with(b"\xff\xd8\xff")
        || header.starts_with(b"GIF87a")
        || header.starts_with(b"GIF89a")
        || (header.len() == 12 && header.starts_with(b"RIFF") && &header[8..12] == b"WEBP");
    supported
        .then_some(())
        .ok_or(UploadNoteImageError::UnsupportedFormat)
}

pub(crate) async fn upload_note_image(
    client: &HackmdClient,
    files: &LocalFiles,
    input: UploadNoteImageInput,
) -> Result<Result<UploadNoteImageOutput, NoteResolution>, UploadNoteImageError> {
    if !input.image_path.is_absolute() {
        return Err(UploadNoteImageError::RelativePath);
    }
    files.allow(&input.image_path)?;
    let mut image = files
        .open_read(&input.image_path)
        .map_err(|_| UploadNoteImageError::InvalidFile)?;
    let metadata = image
        .metadata()
        .map_err(|_| UploadNoteImageError::InvalidFile)?;
    if !metadata.is_file() {
        return Err(UploadNoteImageError::InvalidFile);
    }
    let size_bytes = metadata.len();
    if size_bytes > IMAGE_MAX_BYTES {
        return Err(UploadNoteImageError::TooLarge { size_bytes });
    }
    if size_bytes > IMAGE_WARNING_BYTES && !input.confirm_large_file {
        return Err(UploadNoteImageError::ConfirmationRequired { size_bytes });
    }
    let mut header = [0_u8; 12];
    let header_len = image
        .read(&mut header)
        .map_err(|_| UploadNoteImageError::InvalidFile)?;
    ensure_supported_image(&header[..header_len])?;
    image
        .seek(SeekFrom::Start(0))
        .map_err(|_| UploadNoteImageError::InvalidFile)?;
    let resolution = crate::note::reference::resolve_note_ref(
        client,
        input.workspace,
        &input.note_ref,
        input.refresh,
    )
    .await?;
    let NoteResolution::Resolved { note } = resolution else {
        return Ok(Err(resolution));
    };
    if matches!(note.workspace, Workspace::Team { .. }) {
        return Err(UploadNoteImageError::TeamUnsupported);
    }
    let file_name = input
        .image_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("image");
    let response = client
        .upload_note_image(
            &note.note_id,
            file_name,
            tokio::fs::File::from_std(image),
            size_bytes,
        )
        .await?;
    Ok(Ok(UploadNoteImageOutput {
        link: response.data.link,
    }))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use serde_json::json;

    use super::{
        IMAGE_MAX_BYTES, IMAGE_WARNING_BYTES, UploadNoteImageError, UploadNoteImageInput,
        upload_note_image,
    };

    /// Local access with no configured root, matching the default deployment.
    fn files() -> crate::local::LocalFiles {
        crate::fixture::scratch_files()
    }

    /// A PNG signature followed by a marker the multipart assertions can find.
    const PNG_FIXTURE: &[u8] = b"\x89PNG\r\n\x1a\nfixture-image";
    use crate::{
        client::HackmdClient,
        config::Config,
        fixture::{Scenario, SequenceServer},
    };

    #[tokio::test]
    async fn uploads_streaming_multipart_and_returns_only_link() {
        let mut image = tempfile::NamedTempFile::new().expect("temp image should create");
        image.write_all(PNG_FIXTURE).expect("image should write");
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "POST",
            "/v1/notes/note%2Fid/images",
            201,
            r#"{"data":{"link":"https://hackmd.io/_uploads/image.png"}}"#,
        )
        .expect_body("multipart image bytes and field", |body| {
            body.contains("name=\"image\"") && body.contains("fixture-image")
        })]);
        let client = fixture.client();
        let output = upload_note_image(
            &client,
            &files(),
            UploadNoteImageInput {
                workspace: crate::models::Workspace::Personal,
                note_ref: "note/id".to_owned(),
                refresh: false,
                image_path: image.path().to_path_buf(),
                confirm_large_file: false,
            },
        )
        .await
        .expect("upload should succeed")
        .expect("direct note should resolve");
        assert_eq!(output.link, "https://hackmd.io/_uploads/image.png");
        fixture.finish();
    }

    #[tokio::test]
    async fn refuses_to_upload_a_file_that_is_not_an_image() {
        let mut secret = tempfile::NamedTempFile::new().expect("temp file should create");
        secret
            .write_all(b"-----BEGIN OPENSSH PRIVATE KEY-----\n")
            .expect("file should write");
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        let input = UploadNoteImageInput {
            workspace: crate::models::Workspace::Personal,
            note_ref: "note-id".to_owned(),
            refresh: false,
            image_path: secret.path().to_path_buf(),
            confirm_large_file: false,
        };

        // Rejected before the note reference is resolved, so nothing leaves the
        // machine and no request is made.
        assert!(matches!(
            upload_note_image(&client, &files(), input).await,
            Err(UploadNoteImageError::UnsupportedFormat)
        ));
    }

    #[tokio::test]
    async fn rejects_relative_team_and_oversize_inputs_before_upload() {
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        let relative = serde_json::from_value(json!({
            "note_ref": "id",
            "image_path": "image.png"
        }))
        .expect("input should deserialize");
        assert!(matches!(
            upload_note_image(&client, &files(), relative).await,
            Err(UploadNoteImageError::RelativePath)
        ));

        let oversized = tempfile::NamedTempFile::new().expect("temp image should create");
        oversized
            .as_file()
            .set_len(IMAGE_MAX_BYTES + 1)
            .expect("sparse image should resize");
        let input = UploadNoteImageInput {
            workspace: crate::models::Workspace::Personal,
            note_ref: "id".to_owned(),
            refresh: false,
            image_path: oversized.path().to_path_buf(),
            confirm_large_file: true,
        };
        assert!(matches!(
            upload_note_image(&client, &files(), input).await,
            Err(UploadNoteImageError::TooLarge { .. })
        ));

        let warning = tempfile::NamedTempFile::new().expect("temp image should create");
        warning
            .as_file()
            .set_len(IMAGE_WARNING_BYTES + 1)
            .expect("sparse image should resize");
        let warning_input = UploadNoteImageInput {
            workspace: crate::models::Workspace::Personal,
            note_ref: "id".to_owned(),
            refresh: false,
            image_path: warning.path().to_path_buf(),
            confirm_large_file: false,
        };
        assert!(matches!(
            upload_note_image(&client, &files(), warning_input).await,
            Err(UploadNoteImageError::ConfirmationRequired { .. })
        ));
    }

    #[tokio::test]
    async fn an_older_team_workspace_argument_is_accepted_then_refused() {
        let mut image = tempfile::NamedTempFile::new().expect("temp image should create");
        image.write_all(PNG_FIXTURE).expect("image should write");
        let input: UploadNoteImageInput = serde_json::from_value(serde_json::json!({
            "workspace": {"kind": "team", "team_path": "core"},
            "note_ref": "id",
            "image_path": image.path()
        }))
        .expect("the older workspace argument should still parse");
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        assert!(matches!(
            upload_note_image(&client, &files(), input).await,
            Err(UploadNoteImageError::TeamUnsupported)
        ));
    }

    #[tokio::test]
    async fn a_url_naming_a_team_note_is_refused_before_upload() {
        let mut image = tempfile::NamedTempFile::new().expect("temp image should create");
        image.write_all(PNG_FIXTURE).expect("image should write");
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
        ]);
        let input = UploadNoteImageInput {
            workspace: crate::models::Workspace::Personal,
            note_ref: "https://hackmd.io/@core/slug".to_owned(),
            refresh: false,
            image_path: image.path().to_path_buf(),
            confirm_large_file: false,
        };
        assert!(matches!(
            upload_note_image(&fixture.client(), &files(), input).await,
            Err(UploadNoteImageError::TeamUnsupported)
        ));
        fixture.finish();
    }

    #[tokio::test]
    async fn payload_too_large_has_a_resize_hint() {
        let mut image = tempfile::NamedTempFile::new().expect("temp image should create");
        image.write_all(PNG_FIXTURE).expect("image should write");
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "POST",
            "/v1/notes/id/images",
            413,
            r#"{"error":"too large"}"#,
        )]);
        let client = fixture.client();
        let error = upload_note_image(
            &client,
            &files(),
            UploadNoteImageInput {
                workspace: crate::models::Workspace::Personal,
                note_ref: "id".to_owned(),
                refresh: false,
                image_path: image.path().to_path_buf(),
                confirm_large_file: false,
            },
        )
        .await
        .expect_err("413 should fail")
        .to_string();
        assert!(error.contains("resize it below 5 MB"));
        fixture.finish();
    }
}
