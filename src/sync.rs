//! Local-first sync: the tracked state store and the tools that move a note
//! between `HackMD` and a local Markdown file.

pub(crate) mod check;
pub(crate) mod pull;
pub(crate) mod push;
pub(crate) mod state;
pub(crate) mod tracking;

use std::{io::Read, path::Path};

use thiserror::Error;

use crate::local::{LocalAccessError, LocalFiles};

/// Note body sizes the sync tools accept. Above the warning size a caller must
/// pass `confirm_large_file`; above the maximum the body is refused outright,
/// because both ends of a sync have to hold the whole body in memory.
pub(crate) const BODY_WARNING_BYTES: usize = 5 * 1024 * 1024;
pub(crate) const BODY_MAX_BYTES: usize = 50 * 1024 * 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum BodySizeError {
    #[error("{side} is {size_bytes} bytes; retry with confirm_large_file: true")]
    ConfirmationRequired {
        side: &'static str,
        size_bytes: usize,
    },
    #[error(
        "{side} is {size_bytes} bytes; bodies above {} MiB are refused",
        BODY_MAX_BYTES / 1024 / 1024
    )]
    TooLarge {
        side: &'static str,
        size_bytes: usize,
    },
}

impl crate::reply::ToolError for BodySizeError {
    fn kind(&self) -> crate::reply::ErrorKind {
        use crate::reply::ErrorKind;

        match self {
            Self::ConfirmationRequired { .. } => ErrorKind::ConfirmationRequired,
            Self::TooLarge { .. } => ErrorKind::TooLarge,
        }
    }
}

/// Applies the body limits to one side of a sync, named in the error as
/// `side` ("remote body", "local file").
pub(crate) fn check_body_size(
    side: &'static str,
    size_bytes: usize,
    confirmed: bool,
) -> Result<(), BodySizeError> {
    if size_bytes > BODY_MAX_BYTES {
        return Err(BodySizeError::TooLarge { side, size_bytes });
    }
    if size_bytes > BODY_WARNING_BYTES && !confirmed {
        return Err(BodySizeError::ConfirmationRequired { side, size_bytes });
    }
    Ok(())
}

#[derive(Debug, Error)]
pub(crate) enum LocalBodyError {
    #[error("local_path must be a readable regular file")]
    NotAFile,
    #[error("local_path is not UTF-8 text")]
    NotUtf8,
    #[error(transparent)]
    Access(#[from] LocalAccessError),
    #[error(transparent)]
    Size(#[from] BodySizeError),
}

impl crate::reply::ToolError for LocalBodyError {
    fn kind(&self) -> crate::reply::ErrorKind {
        use crate::reply::ErrorKind;

        match self {
            Self::NotAFile | Self::NotUtf8 => ErrorKind::InvalidInput,
            Self::Access(error) => error.kind(),
            Self::Size(error) => error.kind(),
        }
    }
}

/// Reads a tracked Markdown file under the body limits. The size is judged on
/// the opened file before reading, and the read stops one byte past the
/// maximum, so an oversized file is refused without ever being loaded whole,
/// even if it grows in between.
pub(crate) fn read_local_body(
    files: &LocalFiles,
    path: &Path,
    confirmed: bool,
) -> Result<String, LocalBodyError> {
    crate::local::offload(|| {
        let file = files
            .open_read(path)
            // Only a missing file is "not a file" to the caller. Anything else,
            // a permission error or a policy refusal, is reported as itself so
            // it points at what actually needs fixing.
            .map_err(|error| match error {
                LocalAccessError::NotRegular { .. } => LocalBodyError::NotAFile,
                LocalAccessError::Io(io) if io.kind() == std::io::ErrorKind::NotFound => {
                    LocalBodyError::NotAFile
                }
                other => LocalBodyError::Access(other),
            })?;
        let metadata = file.metadata().map_err(LocalAccessError::Io)?;
        let size = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        check_body_size("local file", size, confirmed)?;
        let mut bytes = Vec::with_capacity(size);
        file.take(u64::try_from(BODY_MAX_BYTES + 1).unwrap_or(u64::MAX))
            .read_to_end(&mut bytes)
            .map_err(LocalAccessError::Io)?;
        check_body_size("local file", bytes.len(), confirmed)?;
        String::from_utf8(bytes).map_err(|_| LocalBodyError::NotUtf8)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangeState {
    InSync,
    LocalOnly,
    RemoteOnly,
    Conflict,
}

pub(crate) fn classify_changes(
    baseline: &[u8; 32],
    local: &[u8; 32],
    remote: &[u8; 32],
) -> ChangeState {
    match (local != baseline, remote != baseline) {
        (false, false) => ChangeState::InSync,
        (true, false) => ChangeState::LocalOnly,
        (false, true) => ChangeState::RemoteOnly,
        (true, true) => ChangeState::Conflict,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BODY_MAX_BYTES, BODY_WARNING_BYTES, BodySizeError, ChangeState, check_body_size,
        classify_changes,
    };

    #[test]
    fn body_limits_have_exact_boundaries() {
        assert!(check_body_size("body", BODY_WARNING_BYTES, false).is_ok());
        assert!(matches!(
            check_body_size("body", BODY_WARNING_BYTES + 1, false),
            Err(BodySizeError::ConfirmationRequired { .. })
        ));
        assert!(check_body_size("body", BODY_WARNING_BYTES + 1, true).is_ok());
        assert!(check_body_size("body", BODY_MAX_BYTES, true).is_ok());
        assert!(matches!(
            check_body_size("body", BODY_MAX_BYTES + 1, true),
            Err(BodySizeError::TooLarge { .. })
        ));
    }

    #[test]
    fn classifies_every_three_way_hash_state() {
        let baseline = [0_u8; 32];
        let local = [1_u8; 32];
        let remote = [2_u8; 32];
        for (local_hash, remote_hash, expected) in [
            (&baseline, &baseline, ChangeState::InSync),
            (&local, &baseline, ChangeState::LocalOnly),
            (&baseline, &remote, ChangeState::RemoteOnly),
            (&local, &remote, ChangeState::Conflict),
        ] {
            assert_eq!(
                classify_changes(&baseline, local_hash, remote_hash),
                expected
            );
        }
    }
}
