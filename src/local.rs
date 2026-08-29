//! Everything the server is allowed to touch on this machine: the private sync
//! state, and the policy for the paths a caller may name.
//!
//! The HTTP client deliberately knows nothing about this. Local files are the
//! server's concern, so the tools that read and write them are handed this
//! context alongside the client.

use std::path::{Component, Path, PathBuf};

use thiserror::Error;

use crate::sync::state::StateStore;

#[derive(Debug)]
pub(crate) struct LocalFiles {
    state: StateStore,
    root: Option<PathBuf>,
}

#[derive(Debug, Error)]
pub(crate) enum LocalAccessError {
    #[error("{} contains a .. component; name the destination directly", path.display())]
    Traversal { path: PathBuf },
    #[error(
        "{} is outside HACKMD_MCP_WORKSPACE_ROOT ({})",
        path.display(),
        root.display()
    )]
    OutsideRoot { path: PathBuf, root: PathBuf },
    #[error("HACKMD_MCP_WORKSPACE_ROOT ({}) does not exist", root.display())]
    MissingRoot { root: PathBuf },
}

impl LocalFiles {
    pub(crate) fn new(state_dir: PathBuf, root: Option<PathBuf>) -> Self {
        Self {
            state: StateStore::new(state_dir),
            root,
        }
    }

    pub(crate) fn state(&self) -> &StateStore {
        &self.state
    }

    /// Decides whether a caller-supplied path may be read or written.
    ///
    /// Without `HACKMD_MCP_WORKSPACE_ROOT` every absolute path is allowed,
    /// which is the historical behavior. With it set, the tools can only reach
    /// inside that tree: an agent acting on a note that tells it to write
    /// `~/.zshrc` gets an error instead of a shell profile.
    pub(crate) fn allow(&self, path: &Path) -> Result<(), LocalAccessError> {
        let Some(root) = self.root.as_ref() else {
            return Ok(());
        };

        // `..` is rejected outright rather than resolved, because the prefix
        // check below only holds for a path that cannot climb back out.
        if path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(LocalAccessError::Traversal {
                path: path.to_path_buf(),
            });
        }
        let root = std::fs::canonicalize(root)
            .map_err(|_| LocalAccessError::MissingRoot { root: root.clone() })?;
        if resolve_existing_prefix(path).starts_with(&root) {
            Ok(())
        } else {
            Err(LocalAccessError::OutsideRoot {
                path: path.to_path_buf(),
                root,
            })
        }
    }
}

/// Canonicalizes as much of `path` as exists and re-attaches the rest, so a
/// destination that has not been created yet is still judged by where it would
/// land, symlinked ancestors included.
fn resolve_existing_prefix(path: &Path) -> PathBuf {
    let mut existing = path;
    loop {
        if let Ok(canonical) = std::fs::canonicalize(existing) {
            let remainder = path.strip_prefix(existing).unwrap_or(Path::new(""));
            return canonical.join(remainder);
        }
        match existing.parent() {
            Some(parent) => existing = parent,
            None => return path.to_path_buf(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{LocalAccessError, LocalFiles};

    #[test]
    fn without_a_root_every_absolute_path_is_allowed() {
        let files = LocalFiles::new("/tmp/state".into(), None);
        assert!(files.allow("/etc/hosts".as_ref()).is_ok());
        assert!(files.allow("/tmp/../etc/hosts".as_ref()).is_ok());
    }

    #[test]
    fn a_root_confines_reads_and_writes_to_its_tree() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let root = directory.path().join("notes");
        fs::create_dir(&root).expect("root should create");
        let files = LocalFiles::new(directory.path().join("state"), Some(root.clone()));

        assert!(files.allow(&root.join("note.md")).is_ok());
        // A destination that does not exist yet is judged by its parent.
        assert!(files.allow(&root.join("nested/note.md")).is_ok());
        assert!(matches!(
            files.allow(&directory.path().join("outside.md")),
            Err(LocalAccessError::OutsideRoot { .. })
        ));
        assert!(matches!(
            files.allow(&root.join("../outside.md")),
            Err(LocalAccessError::Traversal { .. })
        ));
    }

    #[test]
    fn a_symlink_out_of_the_tree_does_not_escape_it() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let root = directory.path().join("notes");
        fs::create_dir(&root).expect("root should create");
        let outside = directory.path().join("outside.md");
        fs::write(&outside, "secret").expect("outside file should write");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("link.md")).expect("symlink should create");

        let files = LocalFiles::new(directory.path().join("state"), Some(root.clone()));
        #[cfg(unix)]
        assert!(matches!(
            files.allow(&root.join("link.md")),
            Err(LocalAccessError::OutsideRoot { .. })
        ));
    }

    #[test]
    fn a_missing_root_is_reported_rather_than_ignored() {
        let files = LocalFiles::new("/tmp/state".into(), Some("/nonexistent/root".into()));
        assert!(matches!(
            files.allow("/nonexistent/root/note.md".as_ref()),
            Err(LocalAccessError::MissingRoot { .. })
        ));
    }
}
