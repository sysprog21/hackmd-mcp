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
    /// outright. Not advertised: the upload route is the same for every note,
    /// so this only cross-checks the owner of an `@owner/slug` URL.
    #[serde(default, rename = "team_path", alias = "workspace")]
    #[schemars(skip)]
    pub(crate) workspace: Workspace,
    /// Internal note ID, `hackmd.io/<id>`, or `hackmd.io/@owner/slug` URL.
    /// Personal and team notes both accept uploads.
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
            Self::InvalidFile | Self::UnsupportedFormat => ErrorKind::InvalidInput,
            Self::TooLarge { .. } => ErrorKind::TooLarge,
            Self::ConfirmationRequired { .. } => ErrorKind::ConfirmationRequired,
            Self::Reference(error) => error.kind(),
            Self::Api(error) => error.kind(),
        }
    }
}

/// Checks the file's magic bytes and names the type they show.
///
/// The tool hands a local file to a remote CDN that answers with a public link,
/// so the file has to be what the caller says it is. Without this, one confused
/// or coerced tool call publishes a private key as readily as a screenshot. The
/// type goes out on the upload's part: a part with none is read as text.
fn image_mime(header: &[u8]) -> Result<&'static str, UploadNoteImageError> {
    if header.starts_with(b"\x89PNG\r\n\x1a\n") {
        Ok("image/png")
    } else if header.starts_with(b"\xff\xd8\xff") {
        Ok("image/jpeg")
    } else if header.starts_with(b"GIF87a") || header.starts_with(b"GIF89a") {
        Ok("image/gif")
    } else if header.len() == 12 && header.starts_with(b"RIFF") && &header[8..12] == b"WEBP" {
        Ok("image/webp")
    } else {
        Err(UploadNoteImageError::UnsupportedFormat)
    }
}

/// Opens the image, checks its size and leading bytes, and rewinds it for
/// the upload.
fn open_image(
    files: &LocalFiles,
    input: &UploadNoteImageInput,
) -> Result<(std::fs::File, u64, &'static str), UploadNoteImageError> {
    // A missing or special file is simply not an image to upload. Anything
    // else, a permission error or a path the confinement refuses, is reported
    // as itself, so it points at what needs fixing.
    let mut image = files
        .open_read(&input.image_path)
        .map_err(|error| match error {
            LocalAccessError::NotRegular { .. } => UploadNoteImageError::InvalidFile,
            LocalAccessError::Io(io) if io.kind() == std::io::ErrorKind::NotFound => {
                UploadNoteImageError::InvalidFile
            }
            other => UploadNoteImageError::Access(other),
        })?;
    let size_bytes = image
        .metadata()
        .map_err(|_| UploadNoteImageError::InvalidFile)?
        .len();
    if size_bytes > IMAGE_MAX_BYTES {
        return Err(UploadNoteImageError::TooLarge { size_bytes });
    }
    if size_bytes > IMAGE_WARNING_BYTES && !input.confirm_large_file {
        return Err(UploadNoteImageError::ConfirmationRequired { size_bytes });
    }

    // `take` then `read_to_end`, not one `read`: a single read may return fewer
    // bytes than asked, and a WebP needs all twelve.
    let mut header = Vec::with_capacity(12);
    (&mut image)
        .take(12)
        .read_to_end(&mut header)
        .map_err(|_| UploadNoteImageError::InvalidFile)?;
    let mime = image_mime(&header)?;
    image
        .seek(SeekFrom::Start(0))
        .map_err(|_| UploadNoteImageError::InvalidFile)?;
    Ok((image, size_bytes, mime))
}

pub(crate) async fn upload_note_image(
    client: &HackmdClient,
    files: &LocalFiles,
    input: UploadNoteImageInput,
) -> Result<Result<UploadNoteImageOutput, NoteResolution>, UploadNoteImageError> {
    files.allow_publish(&input.image_path)?;
    let (image, size_bytes, mime) = crate::local::offload(|| open_image(files, &input))?;
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

    // Both personal and team notes upload through the plain /notes/{id}/images
    // route; image visibility then follows the note's own read permission.
    let file_name = input
        .image_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("image");
    let response = client
        .upload_note_image(
            &note.note_id,
            file_name,
            mime,
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

    /// Local access confined to the temporary directory the test images are
    /// written in: an upload needs a root.
    fn files() -> crate::local::LocalFiles {
        crate::local::LocalFiles::new(
            std::env::temp_dir().join("hackmd-mcp-test"),
            Some(std::env::temp_dir()),
        )
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
            body.contains("name=\"image\"")
                && body.contains("Content-Type: image/png")
                && body.contains("fixture-image")
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

    /// With no root, nothing marks which images the user meant to share.
    #[tokio::test]
    async fn refuses_to_upload_without_a_workspace_root() {
        let mut image = tempfile::NamedTempFile::new().expect("temp image should create");
        image.write_all(PNG_FIXTURE).expect("image should write");
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        let input = UploadNoteImageInput {
            workspace: crate::models::Workspace::Personal,
            note_ref: "note-id".to_owned(),
            refresh: false,
            image_path: image.path().to_path_buf(),
            confirm_large_file: false,
        };
        let error = upload_note_image(&client, &crate::fixture::scratch_files(), input)
            .await
            .expect_err("an upload without a root should be refused");
        assert!(
            matches!(
                error,
                UploadNoteImageError::Access(crate::local::LocalAccessError::Unconfined { .. })
            ),
            "{error:?}"
        );

        // The refusal names a root that would admit this very file, and says
        // the server has to restart to read it.
        let message = error.to_string();
        let parent = image.path().parent().expect("temp file has a parent");
        assert!(message.contains(&parent.display().to_string()), "{message}");
        assert!(message.contains("restart"), "{message}");
    }

    #[tokio::test]
    async fn rejects_relative_and_oversize_inputs_before_upload() {
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        let relative = serde_json::from_value(json!({
            "note_ref": "id",
            "image_path": "image.png"
        }))
        .expect("input should deserialize");
        assert!(matches!(
            upload_note_image(&client, &files(), relative).await,
            Err(UploadNoteImageError::Access(
                crate::local::LocalAccessError::Relative { .. }
            ))
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
    async fn a_team_note_by_id_uploads_through_the_plain_route() {
        let mut image = tempfile::NamedTempFile::new().expect("temp image should create");
        image.write_all(PNG_FIXTURE).expect("image should write");

        // A bare id resolves without a lookup, so the team_path is carried but
        // not needed: the note uploads via the same route as a personal note.
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "POST",
            "/v1/notes/team-note/images",
            200,
            r#"{"data":{"link":"https://hackmd.io/_uploads/team.png"}}"#,
        )]);
        let input: UploadNoteImageInput = serde_json::from_value(serde_json::json!({
            "team_path": "core",
            "note_ref": "team-note",
            "image_path": image.path()
        }))
        .expect("the team_path argument should parse");
        let output = upload_note_image(&fixture.client(), &files(), input)
            .await
            .expect("a team note should upload")
            .expect("a bare id resolves directly");
        assert_eq!(output.link, "https://hackmd.io/_uploads/team.png");
        fixture.finish();
    }

    #[tokio::test]
    async fn a_url_naming_a_team_note_resolves_then_uploads() {
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
            Scenario::new(
                "POST",
                "/v1/notes/team-id/images",
                200,
                r#"{"data":{"link":"https://hackmd.io/_uploads/url.png"}}"#,
            ),
        ]);
        let input = UploadNoteImageInput {
            workspace: crate::models::Workspace::Personal,
            note_ref: "https://hackmd.io/@core/slug".to_owned(),
            refresh: false,
            image_path: image.path().to_path_buf(),
            confirm_large_file: false,
        };
        let output = upload_note_image(&fixture.client(), &files(), input)
            .await
            .expect("a team note named by URL should upload")
            .expect("the slug should resolve");
        assert_eq!(output.link, "https://hackmd.io/_uploads/url.png");
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
        assert!(error.contains("resize it smaller and retry"));
        fixture.finish();
    }

    #[tokio::test]
    async fn an_unreadable_upload_reply_stays_upstream() {
        use crate::reply::{ErrorKind, ToolError as _};

        let mut image = tempfile::NamedTempFile::new().expect("temp image should create");
        image.write_all(PNG_FIXTURE).expect("image should write");
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "POST",
            "/v1/notes/id/images",
            200,
            "not json",
        )]);
        let error = upload_note_image(
            &fixture.client(),
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
        .expect_err("an unreadable reply should fail");
        assert_eq!(error.kind(), ErrorKind::Upstream, "{error}");
        fixture.finish();
    }
}
