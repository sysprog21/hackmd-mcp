use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
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
    #[serde(default)]
    pub(crate) workspace: Workspace,
    pub(crate) note_ref: String,
    /// Bypass cached note lists when resolving an `@owner/slug` URL.
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

/// Checks the file's magic bytes.
///
/// The tool hands a local file to a remote CDN that answers with a public link,
/// so the file has to be what the caller says it is. Without this, one confused
/// or coerced tool call publishes a private key as readily as a screenshot.
fn ensure_supported_image(path: &Path) -> Result<(), UploadNoteImageError> {
    let mut header = Vec::with_capacity(12);
    File::open(path)
        .map_err(|_| UploadNoteImageError::InvalidFile)?
        .take(12)
        .read_to_end(&mut header)
        .map_err(|_| UploadNoteImageError::InvalidFile)?;

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
    let metadata =
        std::fs::metadata(&input.image_path).map_err(|_| UploadNoteImageError::InvalidFile)?;
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
    ensure_supported_image(&input.image_path)?;
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
    let response = client
        .upload_note_image(&note.note_id, &input.image_path)
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
    use crate::{client::HackmdClient, config::Config, models::Workspace};

    #[tokio::test]
    async fn uploads_streaming_multipart_and_returns_only_link() {
        let mut image = tempfile::NamedTempFile::new().expect("temp image should create");
        image.write_all(PNG_FIXTURE).expect("image should write");
        let fixture = crate::fixture::SequenceServer::spawn([(
            201,
            r#"{"data":{"link":"https://hackmd.io/_uploads/image.png"}}"#,
        )]);
        let client = fixture.client();
        let output = upload_note_image(
            &client,
            &files(),
            UploadNoteImageInput {
                workspace: Workspace::Personal,
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
        let requests = fixture.finish();
        assert!(requests[0].starts_with("POST /v1/notes/note%2Fid/images HTTP/1.1\r\n"));
        assert!(requests[0].contains("multipart/form-data; boundary="));
        assert!(requests[0].contains("name=\"image\""));
        assert!(requests[0].contains("fixture-image"));
    }

    #[tokio::test]
    async fn refuses_to_upload_a_file_that_is_not_an_image() {
        let mut secret = tempfile::NamedTempFile::new().expect("temp file should create");
        secret
            .write_all(b"-----BEGIN OPENSSH PRIVATE KEY-----\n")
            .expect("file should write");
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        let input = UploadNoteImageInput {
            workspace: Workspace::Personal,
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
            workspace: Workspace::Personal,
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
            workspace: Workspace::Personal,
            note_ref: "id".to_owned(),
            refresh: false,
            image_path: warning.path().to_path_buf(),
            confirm_large_file: false,
        };
        assert!(matches!(
            upload_note_image(&client, &files(), warning_input).await,
            Err(UploadNoteImageError::ConfirmationRequired { .. })
        ));

        let mut team_image = tempfile::NamedTempFile::new().expect("temp image should create");
        team_image
            .write_all(PNG_FIXTURE)
            .expect("image should write");
        let team_input = UploadNoteImageInput {
            workspace: Workspace::Team {
                team_path: "core".to_owned(),
            },
            note_ref: "id".to_owned(),
            refresh: false,
            image_path: team_image.path().to_path_buf(),
            confirm_large_file: false,
        };
        assert!(matches!(
            upload_note_image(&client, &files(), team_input).await,
            Err(UploadNoteImageError::TeamUnsupported)
        ));
    }

    #[tokio::test]
    async fn payload_too_large_has_a_resize_hint() {
        let mut image = tempfile::NamedTempFile::new().expect("temp image should create");
        image.write_all(PNG_FIXTURE).expect("image should write");
        let fixture = crate::fixture::SequenceServer::spawn([(413, r#"{"error":"too large"}"#)]);
        let client = fixture.client();
        let error = upload_note_image(
            &client,
            &files(),
            UploadNoteImageInput {
                workspace: Workspace::Personal,
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
