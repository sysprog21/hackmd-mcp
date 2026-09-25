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
    root: Option<WorkspaceRoot>,
}

/// `HACKMD_MCP_WORKSPACE_ROOT`, resolved and opened once. Both the policy check
/// and every later operation go through this one directory, so replacing the
/// root (or a symlink on the way to it) after startup changes neither.
#[derive(Debug)]
struct WorkspaceRoot {
    /// As configured, which is how callers usually spell their paths.
    configured: PathBuf,
    /// The canonical path and an open handle, or `None` when the root did not
    /// exist at startup.
    pinned: Option<PinnedRoot>,
}

#[derive(Debug)]
struct PinnedRoot {
    canonical: PathBuf,
    dir: Dir,
    /// Device and inode of `canonical` when it was opened, where the platform
    /// has them.
    identity: Option<(u64, u64)>,
}

impl WorkspaceRoot {
    fn open(configured: PathBuf) -> Self {
        let pinned = fs::canonicalize(&configured).ok().and_then(|canonical| {
            let dir = Dir::open_ambient_dir(&canonical, ambient_authority()).ok()?;
            let identity = directory_identity(&canonical);
            Some(PinnedRoot {
                canonical,
                dir,
                identity,
            })
        });
        Self { configured, pinned }
    }

    /// The pinned root, provided the directory at its canonical path is still
    /// the one that was opened. Path checks run against the live tree while
    /// operations run through the handle, so a root moved or replaced after
    /// startup would otherwise have them disagree about which file is meant.
    fn pinned(&self) -> Result<(&Path, &Dir), LocalAccessError> {
        let Some(root) = self.pinned.as_ref() else {
            return Err(LocalAccessError::MissingRoot {
                root: self.configured.clone(),
            });
        };
        if directory_identity(&root.canonical) != root.identity {
            return Err(LocalAccessError::ReplacedRoot {
                root: root.canonical.clone(),
            });
        }
        Ok((root.canonical.as_path(), &root.dir))
    }
}

#[cfg(unix)]
fn directory_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path)
        .ok()
        .map(|metadata| (metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn directory_identity(_path: &Path) -> Option<(u64, u64)> {
    None
}

/// What a caller-supplied path currently names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Entry {
    Directory,
    Other,
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
    #[error(
        "HACKMD_MCP_WORKSPACE_ROOT ({}) did not exist when the server started",
        root.display()
    )]
    MissingRoot { root: PathBuf },
    #[error(
        "HACKMD_MCP_WORKSPACE_ROOT ({}) was moved or replaced after the server started; restart it",
        root.display()
    )]
    ReplacedRoot { root: PathBuf },
    #[error("local file operation failed: {0}")]
    Io(#[from] io::Error),
}

impl LocalFiles {
    pub(crate) fn new(state_dir: PathBuf, root: Option<PathBuf>) -> Self {
        Self {
            state: StateStore::new(state_dir),
            root: root.map(WorkspaceRoot::open),
        }
    }

    pub(crate) fn state(&self) -> &StateStore {
        &self.state
    }

    pub(crate) fn probe_workspace_root(&self) -> Result<(), LocalAccessError> {
        let Some(root) = self.root.as_ref() else {
            return Ok(());
        };
        let (_, dir) = root.pinned()?;
        dir.metadata(".")?;
        Ok(())
    }

    /// Decides whether a caller-supplied path may be read or written.
    ///
    /// Without `HACKMD_MCP_WORKSPACE_ROOT` every absolute path is allowed,
    /// which is the historical behavior. With it set, the tools can only reach
    /// inside that tree: an agent acting on a note that tells it to write
    /// `~/.zshrc` gets an error instead of a shell profile.
    pub(crate) fn allow(&self, path: &Path) -> Result<(), LocalAccessError> {
        let Some((canonical, _)) = self.root_for(path)? else {
            return Ok(());
        };
        if resolve_existing_prefix(path).starts_with(canonical) {
            Ok(())
        } else {
            Err(LocalAccessError::OutsideRoot {
                path: path.to_path_buf(),
                root: canonical.to_path_buf(),
            })
        }
    }

    /// The pinned root a caller-supplied path is judged against, or `None`
    /// when no root is configured. `..` is rejected outright rather than
    /// resolved, because the prefix checks only hold for a path that cannot
    /// climb back out.
    fn root_for(&self, path: &Path) -> Result<Option<(&Path, &Dir)>, LocalAccessError> {
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
        root.pinned().map(Some)
    }

    /// What `path` names, or `None` when nothing is there: one lookup where
    /// callers would otherwise ask "does it exist" and then "what is it".
    pub(crate) fn entry(&self, path: &Path) -> Result<Option<Entry>, LocalAccessError> {
        let is_dir = match self.confined(path)? {
            Some((dir, relative)) => dir.metadata(relative).map(|metadata| metadata.is_dir()),
            None => fs::metadata(path).map(|metadata| metadata.is_dir()),
        };
        match is_dir {
            Ok(true) => Ok(Some(Entry::Directory)),
            Ok(false) => Ok(Some(Entry::Other)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Reads at most `limit` bytes, so a caller that only compares or bounds a
    /// file never loads more of it than it can use. A result exactly `limit`
    /// long may be the start of a larger file.
    pub(crate) fn read_capped(
        &self,
        path: &Path,
        limit: usize,
    ) -> Result<Vec<u8>, LocalAccessError> {
        let mut bytes = Vec::new();
        self.open_read(path)?
            .take(u64::try_from(limit).unwrap_or(u64::MAX))
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    pub(crate) fn open_read(&self, path: &Path) -> Result<fs::File, LocalAccessError> {
        let Some((dir, relative)) = self.confined(path)? else {
            return Ok(fs::File::open(path)?);
        };
        Ok(dir.open(relative)?.into_std())
    }

    /// Removes a file, through the root's capability when one is configured.
    pub(crate) fn remove_file(&self, path: &Path) -> Result<(), LocalAccessError> {
        match self.confined(path)? {
            Some((dir, relative)) => dir.remove_file(relative)?,
            None => fs::remove_file(path)?,
        }
        Ok(())
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
            if create_parent_dirs && let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
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

        // A random 64-bit name, opened create-new: a collision is not a case
        // worth retrying for, so any failure is reported as itself.
        let temporary = parent.join(format!(
            ".{}.hackmd-mcp-{:016x}.tmp",
            file_name.to_string_lossy(),
            fastrand::u64(..)
        ));
        let mut options = cap_std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = dir.open_with(&temporary, &options)?;
        let result = (|| {
            if let Some(permissions) = existing_permissions {
                file.set_permissions(permissions)?;
            }
            file.write_all(contents)?;
            file.sync_all()?;
            dir.rename(&temporary, dir, &relative)
        })();
        if let Err(error) = result {
            let _ = dir.remove_file(&temporary);
            return Err(LocalAccessError::Io(error));
        }
        Ok(())
    }

    /// The pinned root and `path` relative to it.
    fn confined(&self, path: &Path) -> Result<Option<(&Dir, PathBuf)>, LocalAccessError> {
        let Some((canonical, dir)) = self.root_for(path)? else {
            return Ok(None);
        };

        // Judged by where the parent really is, the same way `allow` judges it,
        // so every spelling `allow` accepts works here too. The final component
        // stays unresolved: a symlink there is the capability's to follow or
        // replace, not ours.
        if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
            && let Ok(relative) = resolve_existing_prefix(parent)
                .join(name)
                .strip_prefix(canonical)
        {
            let relative = if relative.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                relative.to_path_buf()
            };
            return Ok(Some((dir, relative)));
        }

        // The root itself is a valid target too, for instance as a parent
        // checked before a note is written into it. Spelled through a symlink,
        // only resolving the whole path recognizes it.
        if resolve_existing_prefix(path) == canonical {
            return Ok(Some((dir, PathBuf::from("."))));
        }
        Err(LocalAccessError::OutsideRoot {
            path: path.to_path_buf(),
            root: canonical.to_path_buf(),
        })
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
            if let Ok(contents) = files.read_capped(&link.join("note.md"), 64) {
                assert_eq!(contents, b"inside");
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

    #[cfg(unix)]
    #[test]
    fn a_symlinked_root_accepts_both_spellings_of_a_path() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let real = directory.path().join("real");
        fs::create_dir(&real).expect("real root should create");
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("root symlink should create");
        let files = LocalFiles::new(directory.path().join("state"), Some(link.clone()));

        for root in [&link, &real] {
            assert_eq!(
                files
                    .entry(root)
                    .expect("either spelling of the root itself resolves"),
                Some(super::Entry::Directory)
            );
        }
        for path in [link.join("a.md"), real.join("b.md")] {
            files
                .allow(&path)
                .expect("either spelling is inside the root");
            files
                .write_atomic(&path, b"body", false)
                .expect("either spelling writes through the pinned root");
        }
        assert_eq!(
            fs::read_to_string(real.join("a.md")).expect("write should land"),
            "body"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_root_replaced_after_startup_is_refused() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let root = directory.path().join("notes");
        fs::create_dir(&root).expect("root should create");
        let files = LocalFiles::new(directory.path().join("state"), Some(root.clone()));
        assert!(files.allow(&root.join("note.md")).is_ok());

        fs::rename(&root, directory.path().join("moved")).expect("root should move");
        fs::create_dir(&root).expect("replacement root should create");
        assert!(matches!(
            files.allow(&root.join("note.md")),
            Err(LocalAccessError::ReplacedRoot { .. })
        ));
        assert!(matches!(
            files.write_atomic(&root.join("note.md"), b"body", false),
            Err(LocalAccessError::ReplacedRoot { .. })
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
