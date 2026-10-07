use std::{
    borrow::Cow,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
};

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    client::{HackmdClient, HackmdError, ImageUrl, RemoteImageError},
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
    /// Absolute path to a local image file. Give this or `image_url`.
    #[serde(default)]
    pub(crate) image_path: Option<PathBuf>,
    /// Public `https` URL of an image to re-host on `HackMD`, such as an imgur
    /// link in a note being migrated. Give this or `image_path`.
    #[serde(default)]
    pub(crate) image_url: Option<String>,
    /// Required for files larger than 5 MiB.
    #[serde(default)]
    pub(crate) confirm_large_file: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct UploadNoteImageOutput {
    pub(crate) link: String,
    /// [`crate::client::HackmdClient::anonymous_access`] of `link` right after
    /// the upload.
    pub(crate) publicly_readable: Option<bool>,
}

#[derive(Debug, Error)]
pub(crate) enum UploadNoteImageError {
    #[error(transparent)]
    Access(#[from] LocalAccessError),
    #[error("give exactly one of image_path or image_url")]
    Source,
    #[error("image_path is not a readable regular file")]
    InvalidFile,
    #[error("the image is not a PNG, JPEG, GIF, or WebP file")]
    UnsupportedFormat,
    #[error(
        "image is {size}; files above {} MiB are refused",
        IMAGE_MAX_BYTES / 1024 / 1024
    )]
    TooLarge { size: ImageSize },
    #[error(
        "image is {size}; retry with confirm_large_file: true or resize below {} MiB",
        IMAGE_WARNING_BYTES / 1024 / 1024
    )]
    ConfirmationRequired { size: ImageSize },
    #[error(transparent)]
    Remote(#[from] RemoteImageError),
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
            Self::Remote(error) => error.kind(),
            Self::Source | Self::InvalidFile | Self::UnsupportedFormat => ErrorKind::InvalidInput,
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

/// An image's size: known exactly, or known only to pass the limit a read
/// stopped at, when the host declared none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageSize {
    Exact(u64),
    Over(u64),
}

impl std::fmt::Display for ImageSize {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exact(bytes) => write!(formatter, "{bytes} bytes"),
            Self::Over(bytes) => write!(formatter, "over {bytes} bytes"),
        }
    }
}

/// The most an image may be before [`check_size`] refuses it.
const fn size_limit(confirm_large_file: bool) -> u64 {
    if confirm_large_file {
        IMAGE_MAX_BYTES
    } else {
        IMAGE_WARNING_BYTES
    }
}

/// The one size rule, for a local file's length, a declared length, and a
/// read that stopped at [`size_limit`].
fn check_size(size: ImageSize, confirm_large_file: bool) -> Result<(), UploadNoteImageError> {
    let least = match size {
        ImageSize::Exact(bytes) => bytes,
        ImageSize::Over(bytes) => bytes.saturating_add(1),
    };
    if least > IMAGE_MAX_BYTES {
        return Err(UploadNoteImageError::TooLarge { size });
    }
    if least > size_limit(confirm_large_file) {
        return Err(UploadNoteImageError::ConfirmationRequired { size });
    }
    Ok(())
}

/// Opens the image, checks its size and leading bytes, and rewinds it for
/// the upload.
fn open_image(
    files: &LocalFiles,
    path: &std::path::Path,
    confirm_large_file: bool,
) -> Result<(std::fs::File, u64, &'static str), UploadNoteImageError> {
    // A missing or special file is simply not an image to upload. Anything
    // else, a permission error or a path the confinement refuses, is reported
    // as itself, so it points at what needs fixing.
    let mut image = files.open_read(path).map_err(|error| match error {
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
    check_size(ImageSize::Exact(size_bytes), confirm_large_file)?;

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

/// Where the image comes from, decided once from the input.
enum ImageSource<'a> {
    Local {
        image: std::fs::File,
        size_bytes: u64,
        mime: &'static str,
        name: &'a str,
    },
    Remote(ImageUrl),
}

pub(crate) async fn upload_note_image(
    client: &HackmdClient,
    files: &LocalFiles,
    input: UploadNoteImageInput,
) -> Result<Result<UploadNoteImageOutput, NoteResolution>, UploadNoteImageError> {
    // A local file, and whatever of a URL needs no I/O, is checked before the
    // note is resolved, so a refused input costs no request. A URL is fetched
    // after, so a bad note_ref costs no download.
    let source = match (&input.image_path, &input.image_url) {
        (Some(path), None) => {
            files.allow_publish(path)?;
            let (image, size_bytes, mime) =
                crate::local::offload(|| open_image(files, path, input.confirm_large_file))?;
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("image");
            ImageSource::Local {
                image,
                size_bytes,
                mime,
                name,
            }
        }
        (None, Some(link)) => ImageSource::Remote(client.check_image_url(link)?),
        _ => return Err(UploadNoteImageError::Source),
    };
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

    let (name, mime, body, size_bytes): (Cow<str>, _, reqwest::Body, _) = match source {
        ImageSource::Local {
            image,
            size_bytes,
            mime,
            name,
        } => (
            Cow::Borrowed(name),
            mime,
            tokio::fs::File::from_std(image).into(),
            size_bytes,
        ),
        ImageSource::Remote(link) => {
            let remote = client.open_image(&link).await?;

            // A declared size is checked before the body is read, and with none
            // the read stops at the limit, so an image that needs
            // confirm_large_file is never downloaded whole only to be refused.
            if let Some(declared) = remote.declared_len() {
                check_size(ImageSize::Exact(declared), input.confirm_large_file)?;
            }
            let limit = size_limit(input.confirm_large_file);
            let bytes = match remote.read(limit).await {
                Ok(bytes) => bytes,
                // check_size refuses anything past the limit it set.
                Err(RemoteImageError::TooLarge) => {
                    return Err(check_size(ImageSize::Over(limit), input.confirm_large_file)
                        .err()
                        .unwrap_or(UploadNoteImageError::Remote(RemoteImageError::TooLarge)));
                }
                Err(error) => return Err(error.into()),
            };
            let mime = image_mime(bytes.get(..12).unwrap_or(&bytes))?;
            let size_bytes = bytes.len() as u64;
            (
                Cow::Owned(remote_file_name(link.url(), mime)),
                mime,
                bytes.into(),
                size_bytes,
            )
        }
    };

    // Both personal and team notes upload through the plain /notes/{id}/images
    // route; image visibility then follows the note's own read permission.
    let response = client
        .upload_note_image(&note.note_id, &name, mime, body, size_bytes)
        .await?;

    // An image on a note others cannot read is refused to them too, and nothing
    // in the reply says so: an agent handed only the link would publish an
    // article whose images fail for every reader.
    let publicly_readable = client.anonymous_access(&response.data.link).await;
    Ok(Ok(UploadNoteImageOutput {
        link: response.data.link,
        publicly_readable,
    }))
}

/// The stem of `url`'s last path segment, with the extension of the type its
/// bytes showed, so a `.png` link serving a JPEG is not uploaded as `.png`.
/// Inner dots become dashes, and the stem is cut to 100 bytes. A stem that is
/// not plain ASCII, such as a percent-escaped one, is `image`.
fn remote_file_name(url: &url::Url, mime: &str) -> String {
    const STEM_MAX_BYTES: usize = 100;

    let extension = match mime {
        "image/jpeg" => "jpg",
        other => other.trim_start_matches("image/"),
    };
    let mut stem = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .map(|segment| {
            segment
                .rsplit_once('.')
                .map_or(segment, |(stem, _)| stem)
                .replace('.', "-")
        })
        .filter(|stem| {
            !stem.is_empty()
                && stem
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        })
        .unwrap_or_else(|| "image".to_owned());
    stem.truncate(STEM_MAX_BYTES);
    format!("{stem}.{extension}")
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use serde_json::json;

    use super::{
        IMAGE_MAX_BYTES, IMAGE_WARNING_BYTES, ImageSize, RemoteImageError, UploadNoteImageError,
        UploadNoteImageInput, upload_note_image,
    };

    /// Local access confined to the temporary directory the test images are
    /// written in: an upload needs a root.
    fn files() -> crate::local::LocalFiles {
        crate::local::LocalFiles::new(
            std::env::temp_dir().join("hackmd-mcp-test"),
            Some(std::env::temp_dir()),
        )
    }

    /// An upload of the local file at `path` to `note_ref`.
    fn path_input(
        note_ref: &str,
        path: &std::path::Path,
        confirm_large_file: bool,
    ) -> UploadNoteImageInput {
        UploadNoteImageInput {
            workspace: crate::models::Workspace::Personal,
            note_ref: note_ref.to_owned(),
            refresh: false,
            image_path: Some(path.to_path_buf()),
            image_url: None,
            confirm_large_file,
        }
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
            path_input("note/id", image.path(), false),
        )
        .await
        .expect("upload should succeed")
        .expect("direct note should resolve");
        assert_eq!(output.link, "https://hackmd.io/_uploads/image.png");
        // The link is not on the fixture's origin, so it is never fetched.
        assert_eq!(output.publicly_readable, None);
        fixture.finish();
    }

    /// Uploads to a fixture whose reply names a link on the fixture itself,
    /// then answers the signed-out HEAD for that link with `head`.
    async fn upload_then_check(head: Scenario) -> Option<bool> {
        let mut image = tempfile::NamedTempFile::new().expect("temp image should create");
        image.write_all(PNG_FIXTURE).expect("image should write");
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new(
                "POST",
                "/v1/notes/id/images",
                201,
                r#"{"data":{"link":"{origin}/_uploads/x.png"}}"#,
            ),
            head,
        ]);
        let output = upload_note_image(
            &fixture.client(),
            &files(),
            path_input("id", image.path(), false),
        )
        .await
        .expect("upload should succeed")
        .expect("a bare id resolves directly");
        let requests = fixture.finish();
        assert!(
            !requests[1].to_ascii_lowercase().contains("authorization:"),
            "the signed-out check must not carry the token"
        );
        output.publicly_readable
    }

    fn head(status: u16) -> Scenario {
        Scenario::new("HEAD", "/_uploads/x.png", status, "")
    }

    #[tokio::test]
    async fn an_image_on_a_private_note_is_reported_unreadable() {
        assert_eq!(upload_then_check(head(403)).await, Some(false));
    }

    #[tokio::test]
    async fn a_redirect_off_the_site_to_storage_is_readable() {
        let storage = head(302).response_header(
            "location",
            "https://storage.example/x.png?AWSAccessKeyId=K&Expires=1&Signature=S",
        );
        assert_eq!(upload_then_check(storage).await, Some(true));
        let v4 = head(302).response_header(
            "location",
            "https://storage.example/x.png?X-Amz-Credential=C&X-Amz-Expires=60&X-Amz-Signature=S",
        );
        assert_eq!(upload_then_check(v4).await, Some(true));
    }

    #[tokio::test]
    async fn a_served_image_is_readable() {
        let served = head(200).response_header("content-type", "Image/PNG");
        assert_eq!(upload_then_check(served).await, Some(true));
    }

    /// None of these shows the image reached a signed-out reader: a login
    /// page on the site or at an identity provider (signed or not), an unsigned
    /// or plain-http storage redirect, a redirect with nowhere to go, a cache
    /// revalidation, or a page that is not an image.
    #[tokio::test]
    async fn anything_short_of_an_image_or_storage_is_unknown() {
        for scenario in [
            head(302).response_header("location", "{origin}/login"),
            head(302).response_header("location", "/login"),
            head(302).response_header("location", "https://sso.example/auth?next=x"),
            head(302).response_header("location", "//other.example/x.png"),
            head(302).response_header("location", "https://storage.example/x.png"),
            head(302).response_header("location", "http://storage.example/x.png?Signature=S"),
            head(302).response_header("location", "https://md.example/login?Signature=S"),
            head(302).response_header(
                "location",
                "https://storage.example/x.png?AWSAccessKeyId=K&Expires=1&Signature=",
            ),
            head(302).response_header("location", "https://sso.example/auth?Expires=1&Signature=S"),
            head(302),
            head(304),
            head(200).response_header("content-type", "text/html"),
            head(404),
        ] {
            assert_eq!(upload_then_check(scenario).await, None);
        }
    }

    #[tokio::test]
    async fn refuses_to_upload_a_file_that_is_not_an_image() {
        let mut secret = tempfile::NamedTempFile::new().expect("temp file should create");
        secret
            .write_all(b"-----BEGIN OPENSSH PRIVATE KEY-----\n")
            .expect("file should write");
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        let input = path_input("note-id", secret.path(), false);

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
        let input = path_input("note-id", image.path(), false);
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
        let input = path_input("id", oversized.path(), true);
        assert!(matches!(
            upload_note_image(&client, &files(), input).await,
            Err(UploadNoteImageError::TooLarge { .. })
        ));

        let warning = tempfile::NamedTempFile::new().expect("temp image should create");
        warning
            .as_file()
            .set_len(IMAGE_WARNING_BYTES + 1)
            .expect("sparse image should resize");
        let warning_input = path_input("id", warning.path(), false);
        assert!(matches!(
            upload_note_image(&client, &files(), warning_input).await,
            Err(UploadNoteImageError::ConfirmationRequired { .. })
        ));
    }

    /// Exactly one source: both or neither is refused before any request.
    #[tokio::test]
    async fn needs_exactly_one_of_path_or_url() {
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        for args in [
            json!({ "note_ref": "id" }),
            json!({ "note_ref": "id", "image_path": "/tmp/x.png", "image_url": "https://example.com/x.png" }),
        ] {
            let input = serde_json::from_value(args).expect("input should deserialize");
            assert!(matches!(
                upload_note_image(&client, &files(), input).await,
                Err(UploadNoteImageError::Source)
            ));
        }
    }

    /// A URL that is not public https is refused before it is fetched; a bare
    /// note id resolves without a request, so none is made at all.
    #[tokio::test]
    async fn refuses_to_fetch_a_url_that_is_not_public_https() {
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        for (link, expected) in [
            ("https://127.0.0.1/x.png", "private"),
            ("https://localhost/x.png", "private"),
        ] {
            let input = serde_json::from_value(json!({ "note_ref": "id", "image_url": link }))
                .expect("input should deserialize");
            let error = upload_note_image(&client, &files(), input)
                .await
                .expect_err("the URL should be refused");
            assert!(
                matches!(error, UploadNoteImageError::Remote(_)),
                "{error:?}"
            );
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    /// An `image_url` upload against the loopback fixture: the image is
    /// served from the fixture's origin, then posted to the API on it.
    async fn upload_url(
        client: &HackmdClient,
        link: String,
        confirm_large_file: bool,
    ) -> Result<super::UploadNoteImageOutput, UploadNoteImageError> {
        let input = UploadNoteImageInput {
            workspace: crate::models::Workspace::Personal,
            note_ref: "id".to_owned(),
            refresh: false,
            image_path: None,
            image_url: Some(link),
            confirm_large_file,
        };
        upload_note_image(client, &crate::fixture::scratch_files(), input)
            .await
            .map(|output| output.expect("a bare id resolves directly"))
    }

    /// The fetched bytes go up as the multipart part, named after the URL
    /// with the type they showed, and no workspace root is needed.
    #[tokio::test]
    async fn a_url_image_is_fetched_then_uploaded() {
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new("GET", "/pic.png", 200, "GIF89a-fixture")
                .response_header("content-type", "image/gif"),
            Scenario::new(
                "POST",
                "/v1/notes/id/images",
                201,
                r#"{"data":{"link":"https://hackmd.io/_uploads/pic.gif"}}"#,
            )
            .expect_body("the fetched bytes, named and typed", |body| {
                body.contains("filename=\"pic.gif\"")
                    && body.contains("Content-Type: image/gif")
                    && body.contains("GIF89a-fixture")
            }),
        ]);
        let link = format!("{}/pic.png", fixture.origin());
        let output = upload_url(&fixture.client(), link, false)
            .await
            .expect("the upload succeeds");
        assert_eq!(output.link, "https://hackmd.io/_uploads/pic.gif");
        fixture.finish();
    }

    /// A declared size over the threshold is refused from the headers alone:
    /// this fixture declares a body it never sends.
    #[tokio::test]
    async fn a_declared_large_image_needs_confirmation_before_the_download() {
        let declared = IMAGE_WARNING_BYTES + 1;
        let server = crate::fixture::spawn_raw_body(Some(declared), Vec::new());
        let client = server.client(std::time::Duration::from_secs(5));
        let error = upload_url(&client, format!("{}/x.gif", server.origin), false)
            .await
            .expect_err("a large image needs confirmation");
        assert!(
            matches!(
                error,
                UploadNoteImageError::ConfirmationRequired { size } if size == ImageSize::Exact(declared)
            ),
            "{error:?}"
        );
    }

    /// With no declared size, an unconfirmed read stops at the threshold
    /// rather than downloading up to the hard limit.
    #[tokio::test]
    async fn an_undeclared_large_image_needs_confirmation_at_the_threshold() {
        let server = crate::fixture::spawn_raw_body(
            None,
            vec![
                (std::time::Duration::ZERO, b"GIF89a".to_vec()),
                (
                    std::time::Duration::ZERO,
                    vec![b'x'; usize::try_from(IMAGE_WARNING_BYTES).expect("fits")],
                ),
            ],
        );
        let client = server.client(std::time::Duration::from_secs(5));
        let error = upload_url(&client, format!("{}/x.gif", server.origin), false)
            .await
            .expect_err("a large image needs confirmation");
        assert!(
            matches!(
                error,
                UploadNoteImageError::ConfirmationRequired { size } if size == ImageSize::Over(IMAGE_WARNING_BYTES)
            ),
            "{error:?}"
        );
    }

    /// A page that is not an image is refused before anything is uploaded.
    #[tokio::test]
    async fn a_url_that_serves_no_image_is_not_uploaded() {
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/x.png",
            200,
            "<html>not found</html>",
        )
        .response_header("content-type", "text/html")]);
        let link = format!("{}/x.png", fixture.origin());
        let error = upload_url(&fixture.client(), link, false)
            .await
            .expect_err("HTML is not an image");
        assert!(
            matches!(error, UploadNoteImageError::UnsupportedFormat),
            "{error:?}"
        );
        fixture.finish();
    }

    /// A URL that could never be fetched is refused before the note is
    /// resolved. Resolving this `@owner/slug` reference would need the
    /// network, and this client has no token, so any other error would mean
    /// resolution ran first.
    #[tokio::test]
    async fn a_url_that_cannot_be_fetched_is_refused_before_resolution() {
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        let input = serde_json::from_value(json!({
            "note_ref": "https://hackmd.io/@owner/slug",
            "image_url": "http://i.imgur.com/x.png"
        }))
        .expect("input should deserialize");
        let error = upload_note_image(&client, &files(), input)
            .await
            .expect_err("the URL is refused");
        assert!(
            matches!(
                error,
                UploadNoteImageError::Remote(RemoteImageError::NotHttps)
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_remote_image_is_named_by_its_last_path_segment_and_type() {
        let name = |link: &str, mime| {
            super::remote_file_name(&url::Url::parse(link).expect("url parses"), mime)
        };
        assert_eq!(
            name("https://i.imgur.com/KC1dCXq.jpg?x=1", "image/jpeg"),
            "KC1dCXq.jpg"
        );
        assert_eq!(name("https://example.com/x.png", "image/jpeg"), "x.jpg");
        assert_eq!(
            name("https://example.com/download", "image/webp"),
            "download.webp"
        );
        assert_eq!(name("https://example.com/a.b.png", "image/png"), "a-b.png");
        assert_eq!(
            name("https://example.com/%E5%9C%96.png", "image/png"),
            "image.png"
        );
        assert_eq!(name("https://example.com/", "image/gif"), "image.gif");
        let long = format!("https://example.com/{}.png", "a".repeat(300));
        assert_eq!(name(&long, "image/png").len(), 104);
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
        let input = path_input("https://hackmd.io/@core/slug", image.path(), false);
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
        let error = upload_note_image(&client, &files(), path_input("id", image.path(), false))
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
            path_input("id", image.path(), false),
        )
        .await
        .expect_err("an unreadable reply should fail");
        assert_eq!(error.kind(), ErrorKind::Upstream, "{error}");
        fixture.finish();
    }
}
