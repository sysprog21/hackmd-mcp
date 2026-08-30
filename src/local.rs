//! Everything the server is allowed to touch on this machine: the private sync
//! state, and the policy for the paths a caller may name.
//!
//! The HTTP client deliberately knows nothing about this. Local files are the
//! server's concern, so the tools that read and write them are handed this
//! context alongside the client.

use std::{
    fs,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

use cap_std::{ambient_authority, fs::Dir};
use thiserror::Error;

use crate::sync::state::StateStore;

#[derive(Debug)]
pub(crate) struct LocalFiles {
    state: StateStore,
    root: Option<PathBuf>,
}

pub(crate) struct LocalMetadata {
    is_file: bool,
    is_dir: bool,
}

impl LocalMetadata {
    pub(crate) const fn is_file(&self) -> bool {
        self.is_file
    }

    pub(crate) const fn is_dir(&self) -> bool {
        self.is_dir
    }
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
    #[error("local file operation failed")]
    Io(#[from] io::Error),
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

    pub(crate) fn probe_workspace_root(&self) -> Result<(), LocalAccessError> {
        let Some(root) = self.root.as_ref() else {
            return Ok(());
        };
        let dir = Dir::open_ambient_dir(root, ambient_authority())
            .map_err(|_| LocalAccessError::MissingRoot { root: root.clone() })?;
        if dir.metadata(".")?.is_dir() {
            Ok(())
        } else {
            Err(LocalAccessError::MissingRoot { root: root.clone() })
        }
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

    pub(crate) fn metadata(&self, path: &Path) -> Result<LocalMetadata, LocalAccessError> {
        let Some((dir, relative)) = self.confined(path)? else {
            let metadata = fs::metadata(path)?;
            return Ok(LocalMetadata {
                is_file: metadata.is_file(),
                is_dir: metadata.is_dir(),
            });
        };
        let metadata = dir.metadata(relative)?;
        Ok(LocalMetadata {
            is_file: metadata.is_file(),
            is_dir: metadata.is_dir(),
        })
    }

    pub(crate) fn read(&self, path: &Path) -> Result<Vec<u8>, LocalAccessError> {
        let Some((dir, relative)) = self.confined(path)? else {
            return Ok(fs::read(path)?);
        };
        let mut bytes = Vec::new();
        dir.open(relative)?.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    pub(crate) fn open_read(&self, path: &Path) -> Result<fs::File, LocalAccessError> {
        let Some((dir, relative)) = self.confined(path)? else {
            return Ok(fs::File::open(path)?);
        };
        Ok(dir.open(relative)?.into_std())
    }

    pub(crate) fn read_to_string(&self, path: &Path) -> Result<String, LocalAccessError> {
        String::from_utf8(self.read(path)?).map_err(|error| {
            LocalAccessError::Io(io::Error::new(io::ErrorKind::InvalidData, error))
        })
    }

    pub(crate) fn exists(&self, path: &Path) -> Result<bool, LocalAccessError> {
        match self.metadata(path) {
            Ok(_) => Ok(true),
            Err(LocalAccessError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    /// Atomically replaces a confined file through a directory capability.
    /// Every pathname lookup from the root through the final rename stays
    /// beneath the opened root even if another process swaps symlinks.
    pub(crate) fn write_atomic(
        &self,
        path: &Path,
        contents: &[u8],
        create_parent_dirs: bool,
    ) -> Result<(), LocalAccessError> {
        let Some((dir, relative)) = self.confined(path)? else {
            if create_parent_dirs {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
            }
            return crate::sync::state::write_local_atomic(path, contents)
                .map_err(|error| LocalAccessError::Io(io::Error::other(error)));
        };
        let parent = relative.parent().ok_or_else(|| {
            LocalAccessError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "missing parent",
            ))
        })?;
        if create_parent_dirs {
            dir.create_dir_all(parent)?;
        }
        let file_name = relative.file_name().ok_or_else(|| {
            LocalAccessError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "missing filename",
            ))
        })?;
        let existing_permissions = dir
            .metadata(&relative)
            .ok()
            .map(|metadata| metadata.permissions());
        for _ in 0..16 {
            let temporary = parent.join(format!(
                ".{}.hackmd-mcp-{:016x}.tmp",
                file_name.to_string_lossy(),
                fastrand::u64(..)
            ));
            let mut options = cap_std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            let Ok(mut file) = dir.open_with(&temporary, &options) else {
                continue;
            };
            let result = (|| {
                if let Some(permissions) = existing_permissions.clone() {
                    file.set_permissions(permissions)?;
                }
                file.write_all(contents)?;
                file.sync_all()?;
                dir.rename(&temporary, &dir, &relative)
            })();
            if let Err(error) = result {
                let _ = dir.remove_file(&temporary);
                return Err(LocalAccessError::Io(error));
            }
            return Ok(());
        }
        Err(LocalAccessError::Io(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate an atomic temporary file",
        )))
    }

    fn confined(&self, path: &Path) -> Result<Option<(Dir, PathBuf)>, LocalAccessError> {
        let Some(root) = self.root.as_ref() else {
            return Ok(None);
        };
        if path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(LocalAccessError::Traversal {
                path: path.to_path_buf(),
            });
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| LocalAccessError::OutsideRoot {
                path: path.to_path_buf(),
                root: root.clone(),
            })?;
        let dir = Dir::open_ambient_dir(root, ambient_authority())
            .map_err(|_| LocalAccessError::MissingRoot { root: root.clone() })?;
        Ok(Some((dir, relative.to_path_buf())))
    }
}

/// Canonicalizes as much of `path` as exists and re-attaches the rest, so a
/// destination that has not been created yet is still judged by where it would
/// land, symlinked ancestors included.
fn resolve_existing_prefix(path: &Path) -> PathBuf {
    for ancestor in path.ancestors() {
        if let Ok(canonical) = std::fs::canonicalize(ancestor) {
            return canonical.join(path.strip_prefix(ancestor).unwrap_or(Path::new("")));
        }
    }
    path.to_path_buf()
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

    #[cfg(unix)]
    #[test]
    fn swapping_a_symlink_cannot_escape_capability_reads_or_writes() {
        use std::{
            os::unix::fs::symlink,
            sync::{
                Arc,
                atomic::{AtomicBool, Ordering},
            },
            thread,
        };

        let directory = tempfile::tempdir().expect("temp directory should create");
        let root = directory.path().join("notes");
        let inside = root.join("inside");
        let outside = directory.path().join("outside");
        fs::create_dir_all(&inside).expect("inside directory should create");
        fs::create_dir_all(&outside).expect("outside directory should create");
        fs::write(inside.join("note.md"), "inside").expect("inside fixture should write");
        fs::write(outside.join("note.md"), "outside-secret").expect("outside fixture should write");

        let link = root.join("link");
        symlink(&inside, &link).expect("initial symlink should create");
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker_link = link.clone();
        let worker_inside = inside.clone();
        let worker_outside = outside.clone();
        let worker = thread::spawn(move || {
            let mut outside_next = true;
            while !worker_stop.load(Ordering::Relaxed) {
                let replacement = worker_link.with_extension("replacement");
                let _ = fs::remove_file(&replacement);
                let target = if outside_next {
                    &worker_outside
                } else {
                    &worker_inside
                };
                if symlink(target, &replacement).is_ok() {
                    let _ = fs::rename(&replacement, &worker_link);
                }
                outside_next = !outside_next;
            }
        });

        let files = LocalFiles::new(directory.path().join("state"), Some(root));
        for _ in 0..500 {
            if let Ok(contents) = files.read_to_string(&link.join("note.md")) {
                assert_eq!(contents, "inside");
            }
            let _ = files.write_atomic(&link.join("written.md"), b"confined", false);
        }
        stop.store(true, Ordering::Relaxed);
        worker.join().expect("symlink swapper should stop");

        assert_eq!(
            fs::read_to_string(outside.join("note.md")).expect("outside fixture should remain"),
            "outside-secret"
        );
        assert!(!outside.join("written.md").exists());
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
