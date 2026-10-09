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
    /// Refuse to write files agents load as instructions. On without a root,
    /// and on beneath a root the user did not set in their own environment.
    guard_instructions: bool,
    /// Serializes the tools that write sync state; see `sync_lock`.
    sync: tokio::sync::Mutex<()>,
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

/// A file's size and modification time: enough to tell that it was rewritten
/// between two looks, short of a same-size write within the file system's
/// timestamp resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
}

impl Stamp {
    fn of(metadata: &fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        }
    }

    fn of_confined(metadata: &cap_std::fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata
                .modified()
                .ok()
                .map(cap_std::time::SystemTime::into_std),
        }
    }
}

/// What the file at a path must still be for `write_atomic` to replace it, so
/// a save that lands while a caller judges the file is not lost: nothing at
/// all, or the file seen earlier.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Expect {
    Absent,
    Stamp(Stamp),
}

impl Expect {
    /// Whether the file still meets this, given `look(follow)`: its stamp now,
    /// following a symlink or not. Absence is judged without following, so a
    /// dangling symlink is something there, not nothing.
    fn holds(self, look: impl FnOnce(bool) -> io::Result<Option<Stamp>>) -> io::Result<bool> {
        Ok(match self {
            Self::Absent => look(false)?.is_none(),
            Self::Stamp(stamp) => look(true)? == Some(stamp),
        })
    }
}

/// `None` for a missing file, the error otherwise.
fn found<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// What a caller-supplied path currently names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Entry {
    Directory,
    Other,
}

#[derive(Debug, Error)]
pub(crate) enum LocalAccessError {
    #[error("{} must be an absolute path", path.display())]
    Relative { path: PathBuf },
    #[error("{} is not a regular file", path.display())]
    NotRegular { path: PathBuf },
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
    #[error(
        "{} is a file coding agents load as instructions; set HACKMD_MCP_WORKSPACE_ROOT to a tree that may hold it",
        path.display()
    )]
    AgentInstructions { path: PathBuf },
    #[error(
        "{} is inside this server's sync state directory (HACKMD_MCP_STATE_DIR); choose another destination",
        path.display()
    )]
    StateDirectory { path: PathBuf },
    #[error("{}", unconfined_message(path, *from_dotenv))]
    Unconfined { path: PathBuf, from_dotenv: bool },
    #[error(
        "{} is no longer what this call saw there (it changed after it was read, or a dangling symlink stands where nothing was expected), so nothing was written; check the path and redo the step",
        path.display()
    )]
    Changed { path: PathBuf },
    #[error("local file operation failed: {0}")]
    Io(#[from] io::Error),
}

/// Why a publish was refused and how to allow it: which root counts, a
/// directory that would admit this file (only for an absolute path, whose
/// parent names one), and that the server reads the root only at startup.
fn unconfined_message(path: &Path, from_dotenv: bool) -> String {
    let reason = if from_dotenv {
        "HACKMD_MCP_WORKSPACE_ROOT comes only from the working-directory .env, which does not count for publishing"
    } else {
        "HACKMD_MCP_WORKSPACE_ROOT is not set in the server's environment"
    };
    let example = path
        .parent()
        .filter(|_| path.is_absolute())
        .map(|parent| format!(", such as {}", parent.display()))
        .unwrap_or_default();
    format!(
        "{} would be published at a public link, and {reason}; set it in the server's own environment to the narrowest directory that holds the images to share{example}, then restart the server, which reads it only at startup",
        path.display()
    )
}

impl crate::reply::ToolError for LocalAccessError {
    fn kind(&self) -> crate::reply::ErrorKind {
        use crate::reply::ErrorKind;

        match self {
            Self::Traversal { .. }
            | Self::OutsideRoot { .. }
            | Self::MissingRoot { .. }
            | Self::ReplacedRoot { .. }
            | Self::AgentInstructions { .. }
            | Self::StateDirectory { .. }
            | Self::Unconfined { .. } => ErrorKind::LocalAccess,
            Self::Relative { .. } | Self::NotRegular { .. } => ErrorKind::InvalidInput,
            Self::Changed { .. } => ErrorKind::Conflict,
            Self::Io(_) => ErrorKind::LocalIo,
        }
    }
}

/// Runs blocking filesystem or hashing work from an async tool handler. On
/// the multi-threaded runtime the server runs on, the worker first hands its
/// other tasks to the rest of the pool, so an fsync or a 50 MiB hash does not
/// stall them. On a single-threaded runtime, as in tests, it simply runs.
pub(crate) fn offload<T>(work: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(work)
        }
        _ => work(),
    }
}

/// Files that coding agents read as standing instructions, by name. The list
/// is best effort: `CLAUDE.md` can import any Markdown file, so the real
/// control is `HACKMD_MCP_WORKSPACE_ROOT`.
const AGENT_INSTRUCTION_FILES: &[&str] = &[
    "agent.md",
    "agents.md",
    "agents.override.md",
    "claude.md",
    "claude.local.md",
    "conventions.md",
    "crush.md",
    "gemini.md",
    "qwen.md",
    "skill.md",
    "warp.md",
];

/// Directories whose contents coding agents load as configuration, skills,
/// or instructions.
const AGENT_CONFIG_DIRS: &[&str] = &[
    ".agents",
    ".aiassistant",
    ".amazonq",
    ".augment",
    ".claude",
    ".clinerules",
    ".codex",
    ".continue",
    ".cursor",
    ".gemini",
    ".github",
    ".junie",
    ".kilocode",
    ".kiro",
    ".opencode",
    ".roo",
    ".trae",
    ".windsurf",
];

/// A name as a case-insensitive filesystem compares it, so a listed name
/// cannot be spelled past the check. Win32 drops trailing dots and spaces and
/// opens `name:stream` as `name`; APFS and NTFS fold case beyond ASCII, so
/// `agentſ.md` (long s) opens as `AGENTS.md` and a Kelvin sign as `k`.
/// Upper- then lowercasing folds both the way the filesystem does. HFS+ also
/// ignores invisible characters such as U+200C when it looks a name up, so
/// `CLAUDE\u{200c}.md` opens as `CLAUDE.md` there; those are dropped first.
fn fold_name(name: &str) -> String {
    let name = name.split_once(':').map_or(name, |(stem, _)| stem);
    name.chars()
        .filter(|&c| !is_default_ignorable(c))
        .collect::<String>()
        .trim_end_matches(['.', ' '])
        .chars()
        .flat_map(char::to_uppercase)
        .flat_map(char::to_lowercase)
        .collect()
}

/// Unicode's `Default_Ignorable_Code_Point` set: characters that render as
/// nothing, a superset of those HFS+ skips when comparing names.
fn is_default_ignorable(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{034F}'
            | '\u{061C}'
            | '\u{115F}'..='\u{1160}'
            | '\u{17B4}'..='\u{17B5}'
            | '\u{180B}'..='\u{180F}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{3164}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
            | '\u{FFA0}'
            | '\u{FFF0}'..='\u{FFF8}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0000}'..='\u{E0FFF}'
    )
}

/// Whether writing `path` could plant instructions an agent later follows.
fn is_agent_instruction_path(path: &Path) -> bool {
    let listed = |list: &[&str], name: &std::ffi::OsStr| {
        name.to_str()
            .is_some_and(|name| list.contains(&fold_name(name).as_str()))
    };
    path.file_name()
        .is_some_and(|name| listed(AGENT_INSTRUCTION_FILES, name))
        || path
            .components()
            .any(|component| listed(AGENT_CONFIG_DIRS, component.as_os_str()))
}

/// Replaces `path` with `contents` through a temporary file beside it, synced
/// before the rename, so a reader sees the old file or the new one, never a
/// torn mix. `before_persist` runs on the written file just before the rename:
/// to set its permissions, or to refuse.
pub(crate) fn replace_atomic(
    path: &Path,
    contents: &[u8],
    before_persist: impl FnOnce(&fs::File) -> io::Result<()>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing parent"))?;
    // Created private (0600 on unix) and invisible until persisted, so
    // `before_persist` may set permissions after the write; it runs last so
    // a check it makes is as close to the rename as it can be.
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;
    before_persist(temporary.as_file())?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

impl LocalFiles {
    pub(crate) fn new(state_dir: PathBuf, root: Option<PathBuf>) -> Self {
        Self {
            state: StateStore::new(state_dir),
            guard_instructions: root.is_none(),
            root: root.map(WorkspaceRoot::open),
            sync: tokio::sync::Mutex::new(()),
        }
    }

    /// The local files `config` describes, the one way the server and the
    /// self-check build them. A root from the working-directory `.env` is
    /// honored but not trusted: that file may be someone else's, with a root
    /// of `/`, so files agents load as instructions stay refused beneath it.
    pub(crate) fn from_config(config: &crate::config::Config) -> Self {
        let files = Self::new(
            config.state_dir().to_path_buf(),
            config.workspace_root().map(Path::to_path_buf),
        );
        if config.workspace_root().is_none() || config.workspace_root_trusted() {
            return files;
        }
        tracing::warn!(
            "HACKMD_MCP_WORKSPACE_ROOT comes from the working-directory .env, not the environment; files agents load as instructions stay refused beneath it"
        );
        files.guarding_instructions()
    }

    /// Keeps the instruction-file refusal on beneath the root, for a root that
    /// came from a file the user may not have written.
    pub(crate) fn guarding_instructions(mut self) -> Self {
        self.guard_instructions = true;
        self
    }

    pub(crate) fn state(&self) -> &StateStore {
        &self.state
    }

    /// Serializes the tools that write sync state. Pull and push check a file
    /// and its record, then write both; untrack removes a record, leaving the
    /// file alone. Tool calls run concurrently, so two pulls of different
    /// notes into one new file could otherwise each find no record, and leave
    /// the file holding one note's body while its record names the other, so
    /// the next push overwrites the wrong note; and a push in flight could
    /// write back a record untrack just removed. One lock for every file keeps
    /// that reasoning simple; sync is not a hot path.
    pub(crate) async fn sync_lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.sync.lock().await
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
        // Relative to what would be the server's own working directory, which
        // the caller neither chose nor can see.
        if !path.is_absolute() {
            return Err(LocalAccessError::Relative {
                path: path.to_path_buf(),
            });
        }
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

    /// `allow`, for a file about to be published at a public link. That needs
    /// a tree the user chose: with no root, one confused tool call could
    /// publish any screenshot or scanned document on the machine.
    pub(crate) fn allow_publish(&self, path: &Path) -> Result<(), LocalAccessError> {
        // A root from a working-directory `.env` may be someone else's, with a
        // root of `/`: only one from the server's own environment counts, which
        // is exactly when the instruction guard is off.
        if self.root.is_none() || self.guard_instructions {
            return Err(LocalAccessError::Unconfined {
                path: path.to_path_buf(),
                from_dotenv: self.root.is_some(),
            });
        }
        self.allow(path)
    }

    /// `allow`, for a path about to be written. Without a root there is no
    /// tree the user chose, and a root from a working-directory `.env` may not
    /// be the user's choice either, so in both cases the files agents load as
    /// instructions are refused too: a note must not be able to talk an agent
    /// into pulling itself over `CLAUDE.md` or into a skills directory.
    pub(crate) fn allow_write(&self, path: &Path) -> Result<(), LocalAccessError> {
        self.allow(path)?;

        // `..` is refused for every write, root or not: beneath a directory
        // that does not exist yet it cannot be resolved, so the checks below
        // could not see where the path really lands.
        if path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(LocalAccessError::Traversal {
                path: path.to_path_buf(),
            });
        }
        let resolved = resolve_existing_prefix(path);

        // Judged on the path as spelled and as resolved, so a symlinked
        // directory such as `/tmp/x -> /repo/.claude` cannot hide one.
        if self.guard_instructions
            && (is_agent_instruction_path(path) || is_agent_instruction_path(&resolved))
        {
            return Err(LocalAccessError::AgentInstructions {
                path: path.to_path_buf(),
            });
        }

        // The sync state is this server's own, and is trusted on load: a note
        // written over a sidecar or baseline would forge it. A relative state
        // directory is made absolute first, or no absolute path would ever fall
        // beneath it.
        let state_root = std::path::absolute(self.state.root())
            .unwrap_or_else(|_| self.state.root().to_path_buf());

        // Judged both fully resolved and with only the parent resolved: a
        // symlink as the final component resolves elsewhere, but the write
        // replaces the link itself, where it stands.
        let state_root = resolve_existing_prefix(&state_root);
        let entry = match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => resolve_existing_prefix(parent).join(name),
            _ => resolved.clone(),
        };
        if resolved.starts_with(&state_root) || entry.starts_with(&state_root) {
            return Err(LocalAccessError::StateDirectory {
                path: path.to_path_buf(),
            });
        }
        Ok(())
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

    /// The stamp of the file at `path` now, from its metadata alone, so even
    /// a file that cannot be read has one; `None` when nothing is there.
    pub(crate) fn stamp(&self, path: &Path) -> Result<Option<Stamp>, LocalAccessError> {
        crate::local::offload(|| {
            Ok(match self.confined(path)? {
                Some((dir, relative)) => found(dir.metadata(relative))?
                    .as_ref()
                    .map(Stamp::of_confined),
                None => found(fs::metadata(path))?.as_ref().map(Stamp::of),
            })
        })
    }

    /// Reads at most `limit` bytes, so a caller that only compares or bounds a
    /// file never loads more of it than it can use; a result exactly `limit`
    /// long may be the start of a larger file. The stamp comes from the handle
    /// the bytes came through, for `write_atomic` to compare against later.
    pub(crate) fn read_stamped(
        &self,
        path: &Path,
        limit: usize,
    ) -> Result<(Vec<u8>, Stamp), LocalAccessError> {
        crate::local::offload(|| {
            let file = self.open_read(path)?;
            let stamp = Stamp::of(&file.metadata()?);
            let mut bytes = Vec::new();
            file.take(u64::try_from(limit).unwrap_or(u64::MAX))
                .read_to_end(&mut bytes)?;
            Ok((bytes, stamp))
        })
    }

    /// Opens a regular file for reading and refuses anything else. The open
    /// itself is non-blocking: opening a FIFO would otherwise wait for a
    /// writer, hanging the tool call and pinning a runtime worker, and a check
    /// made before opening could be raced by swapping one in. The handle it
    /// returns is what gets checked. For a regular file the flag changes
    /// nothing, so reads behave as usual.
    pub(crate) fn open_read(&self, path: &Path) -> Result<fs::File, LocalAccessError> {
        let opened = match self.confined(path)? {
            None => {
                let mut options = fs::OpenOptions::new();
                options.read(true);
                #[cfg(unix)]
                std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NONBLOCK);
                options.open(path)
            }
            Some((dir, relative)) => {
                let mut options = cap_std::fs::OpenOptions::new();
                options.read(true);
                #[cfg(unix)]
                cap_std::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NONBLOCK);
                dir.open_with(relative, &options)
                    .map(cap_std::fs::File::into_std)
            }
        };

        // Windows refuses to open a directory at all, with "access denied",
        // where Unix opens it and the check below refuses it. Either way the
        // caller hears the same thing. Looking only after a failed open
        // classifies the error; it decides nothing an attacker could race.
        let file = opened.map_err(|error| {
            if error.kind() == io::ErrorKind::PermissionDenied && path.is_dir() {
                LocalAccessError::NotRegular {
                    path: path.to_path_buf(),
                }
            } else {
                LocalAccessError::Io(error)
            }
        })?;
        if !file.metadata()?.is_file() {
            return Err(LocalAccessError::NotRegular {
                path: path.to_path_buf(),
            });
        }
        Ok(file)
    }

    /// Removes a file, through the root's capability when one is configured.
    pub(crate) fn remove_file(&self, path: &Path) -> Result<(), LocalAccessError> {
        offload(|| {
            match self.confined(path)? {
                Some((dir, relative)) => dir.remove_file(relative)?,
                None => fs::remove_file(path)?,
            }
            Ok(())
        })
    }

    /// Atomically replaces a confined file through a directory capability.
    /// Every pathname lookup from the root through the final rename stays
    /// beneath the opened root even if another process swaps symlinks.
    pub(crate) fn write_atomic(
        &self,
        path: &Path,
        contents: &[u8],
        create_parent_dirs: bool,
        expect: Expect,
    ) -> Result<(), LocalAccessError> {
        let changed = || LocalAccessError::Changed {
            path: path.to_path_buf(),
        };
        crate::local::offload(|| {
            // The same refusal `allow_write` gives up front, repeated where the
            // write happens so no caller can skip it.
            self.allow_write(path)?;
            let Some((dir, relative)) = self.confined(path)? else {
                if create_parent_dirs && let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                // The user's own file keeps whatever permissions it had.
                let existing = fs::metadata(path).ok().map(|meta| meta.permissions());
                let mut held = true;
                let written = replace_atomic(path, contents, |file| {
                    held = expect.holds(|follow| {
                        let metadata = if follow {
                            fs::metadata(path)
                        } else {
                            fs::symlink_metadata(path)
                        };
                        Ok(found(metadata)?.as_ref().map(Stamp::of))
                    })?;
                    if !held {
                        return Err(io::Error::other("changed"));
                    }
                    existing.map_or(Ok(()), |permissions| file.set_permissions(permissions))
                });
                return match written {
                    Err(_) if !held => Err(changed()),
                    written => Ok(written?),
                };
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

            // A random 64-bit name, opened create-new: a collision is not a
            // case worth retrying for, so any failure is reported as itself.
            let temporary = parent.join(format!(
                ".{}.hackmd-mcp-{:016x}.tmp",
                file_name.to_string_lossy(),
                fastrand::u64(..)
            ));
            let mut options = cap_std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            let mut file = dir.open_with(&temporary, &options)?;
            let result = (|| {
                file.write_all(contents)?;
                file.sync_all()?;
                let look = |follow| {
                    let metadata = if follow {
                        dir.metadata(&relative)
                    } else {
                        dir.symlink_metadata(&relative)
                    };
                    Ok(found(metadata)?.as_ref().map(Stamp::of_confined))
                };
                if !expect.holds(look)? {
                    return Ok(false);
                }
                // Only now, so a refused temporary is never left read-only
                // where it cannot be removed.
                if let Some(permissions) = existing_permissions {
                    file.set_permissions(permissions)?;
                }
                dir.rename(&temporary, dir, &relative).map(|()| true)
            })();
            match result {
                Ok(true) => Ok(()),
                Ok(false) => {
                    let _ = dir.remove_file(&temporary);
                    Err(changed())
                }
                Err(error) => {
                    let _ = dir.remove_file(&temporary);
                    Err(LocalAccessError::Io(error))
                }
            }
        })
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
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use super::{Expect, LocalAccessError, LocalFiles};

    /// `path` made absolute on this platform. A leading `/` is not enough on
    /// Windows, where a path without a drive is relative to the current one.
    fn abs(path: &str) -> PathBuf {
        let root = if cfg!(windows) { r"C:\" } else { "/" };
        Path::new(root).join(path)
    }

    /// A write that expects the file read earlier, or none, refuses once an
    /// editor has saved there meanwhile, and leaves that save in place.
    #[test]
    fn a_write_refuses_a_file_changed_since_it_was_read() {
        for confined in [false, true] {
            let directory = tempfile::tempdir().expect("temp directory should create");
            let root = directory
                .path()
                .canonicalize()
                .expect("root should resolve");
            let path = root.join("note.md");
            let files = LocalFiles::new(root.join("state"), confined.then(|| root.clone()));

            fs::write(&path, "first").expect("file should write");
            let (_, stamp) = files.read_stamped(&path, 64).expect("file should read");
            fs::write(&path, "an editor's save").expect("file should rewrite");
            for expect in [Expect::Stamp(stamp), Expect::Absent] {
                assert!(matches!(
                    files.write_atomic(&path, b"pull", false, expect),
                    Err(LocalAccessError::Changed { .. })
                ));
            }
            assert_eq!(
                fs::read_to_string(&path).expect("file should read"),
                "an editor's save"
            );

            let (_, stamp) = files.read_stamped(&path, 64).expect("file should read");
            files
                .write_atomic(&path, b"pull", false, Expect::Stamp(stamp))
                .expect("an unchanged file should be replaced");
            assert_eq!(fs::read_to_string(&path).expect("file should read"), "pull");
            let leftovers = fs::read_dir(&root)
                .expect("root should list")
                .filter(|entry| {
                    entry
                        .as_ref()
                        .is_ok_and(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
                })
                .count();
            assert_eq!(leftovers, 0, "a refused write leaves no temporary file");
        }
    }

    /// A dangling symlink is something there: a write that expects nothing
    /// refuses rather than replace it.
    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_is_not_absent() {
        for confined in [false, true] {
            let directory = tempfile::tempdir().expect("temp directory should create");
            let root = directory
                .path()
                .canonicalize()
                .expect("root should resolve");
            let path = root.join("note.md");
            std::os::unix::fs::symlink(root.join("missing.md"), &path)
                .expect("symlink should create");
            let files = LocalFiles::new(root.join("state"), confined.then(|| root.clone()));
            assert!(matches!(
                files.write_atomic(&path, b"pull", false, Expect::Absent),
                Err(LocalAccessError::Changed { .. })
            ));
            assert!(fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink()));
        }
    }

    #[test]
    fn without_a_root_every_absolute_path_is_allowed() {
        let files = LocalFiles::new(abs("tmp/state"), None);
        assert!(files.allow(&abs("etc/hosts")).is_ok());
        assert!(files.allow(&abs("tmp/../etc/hosts")).is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn offload_runs_on_the_server_runtime_and_on_a_plain_thread() {
        assert_eq!(super::offload(|| 1 + 1), 2);
        assert_eq!(
            tokio::spawn(async { super::offload(|| "worker") })
                .await
                .expect("task should finish"),
            "worker"
        );
        assert_eq!(
            tokio::spawn(async { super::offload(|| super::offload(|| "nested")) })
                .await
                .expect("nested offload should finish"),
            "nested"
        );
        assert_eq!(super::offload(|| "no runtime needed"), "no runtime needed");
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_is_refused_instead_of_blocking_the_read() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let fifo = directory.path().join("pipe.md");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo should run");
        assert!(made.success());
        for files in [
            LocalFiles::new(directory.path().join("state"), None),
            LocalFiles::new(
                directory.path().join("state"),
                Some(directory.path().to_path_buf()),
            ),
        ] {
            let error = files
                .open_read(&fifo)
                .expect_err("a FIFO is not a regular file");
            assert_eq!(
                error.to_string(),
                format!("{} is not a regular file", fifo.display())
            );
        }
    }

    #[test]
    fn a_relative_path_is_refused_as_invalid_input() {
        use crate::reply::{ErrorKind, ToolError};

        let files = LocalFiles::new(abs("tmp/state"), None);
        let error = files
            .allow("notes/note.md".as_ref())
            .expect_err("a relative path must be refused");
        assert_eq!(error.to_string(), "notes/note.md must be an absolute path");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn without_a_root_agent_instruction_files_are_not_written() {
        let files = LocalFiles::new(abs("tmp/state"), None);
        for refused in [
            "repo/CLAUDE.md",
            "repo/agents.md",
            "home/u/.claude/skills/x/SKILL.md",
            "repo/.github/copilot-instructions.md",
            "repo/.cursor/rules/note.md",
            "repo/.clinerules/note.md",
            "repo/CRUSH.md",
            // Spellings a case-insensitive or Win32 filesystem opens as a
            // listed name.
            "repo/agent\u{17f}.md",
            "repo/\u{17f}kill.md",
            "repo/.\u{212a}iro/note.md",
            "repo/.claude./agents/note.md",
            "repo/.claude /agents/note.md",
            "repo/.claude::$INDEX_ALLOCATION/agents/note.md",
            // Names HFS+ matches while skipping an invisible character.
            "repo/CLAUDE\u{200c}.md",
            "repo/.c\u{200d}laude/commands/x.md",
            "repo/\u{feff}AGENTS.md",
            "repo/AGENTS.override.md",
            "repo/WARP.md",
            "repo/.kilocode/rules/note.md",
        ]
        .map(abs)
        {
            let error = files
                .allow_write(&refused)
                .expect_err("agent instruction files must be refused");
            assert_eq!(
                error.to_string(),
                format!(
                    "{} is a file coding agents load as instructions; set HACKMD_MCP_WORKSPACE_ROOT to a tree that may hold it",
                    refused.display()
                )
            );
            // Reading one, say to check its sync state, is not the risk.
            assert!(files.allow(&refused).is_ok());
        }
        assert!(files.allow_write(&abs("repo/docs/notes.md")).is_ok());
        assert!(files.allow_write(&abs("repo/CLAUDE-notes.md")).is_ok());
    }

    /// A sidecar or baseline is trusted on load, so nothing may be written
    /// over one, root or not.
    #[test]
    fn the_state_directory_is_never_a_destination() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let state = directory.path().join("state");
        let files = LocalFiles::new(state.clone(), None);
        for refused in [state.join("tracked/x.baseline.md"), state.join("notes.md")] {
            assert!(matches!(
                files.allow_write(&refused),
                Err(LocalAccessError::StateDirectory { .. })
            ));
        }
        assert!(
            files
                .allow_write(&directory.path().join("notes.md"))
                .is_ok()
        );

        // Through a directory that does not exist yet, `..` cannot be resolved,
        // so it is refused for writes even without a root.
        let around = directory
            .path()
            .join("missing/../state/tracked/x.baseline.md");
        assert!(matches!(
            files.allow_write(&around),
            Err(LocalAccessError::Traversal { .. })
        ));

        // A relative state directory still covers its absolute spelling.
        let relative = LocalFiles::new(PathBuf::from("relative-state"), None);
        let absolute = std::env::current_dir()
            .expect("working directory should resolve")
            .join("relative-state/tracked/x.baseline.md");
        assert!(matches!(
            relative.allow_write(&absolute),
            Err(LocalAccessError::StateDirectory { .. })
        ));
    }

    /// Publishing needs a root from the server's own environment; one from a
    /// working-directory `.env` could be `/`.
    #[test]
    fn only_a_trusted_root_may_publish() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let image = directory.path().join("image.png");
        let state = directory.path().join("state");
        let from_env = LocalFiles::new(state.clone(), Some(directory.path().to_path_buf()));
        assert!(from_env.allow_publish(&image).is_ok());
        let from_dotenv =
            LocalFiles::new(state, Some(directory.path().to_path_buf())).guarding_instructions();
        assert!(matches!(
            from_dotenv.allow_publish(&image),
            Err(LocalAccessError::Unconfined { .. })
        ));
    }

    /// A symlink inside the state directory resolves elsewhere, but a write
    /// replaces the link where it stands, inside the state directory.
    #[cfg(unix)]
    #[test]
    fn a_symlink_in_the_state_directory_is_still_inside_it() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let state = directory.path().join("state");
        fs::create_dir(&state).expect("state directory should create");
        let link = state.join("forged.md");
        std::os::unix::fs::symlink(directory.path().join("outside.md"), &link)
            .expect("symlink should create");
        let files = LocalFiles::new(state, None);
        assert!(matches!(
            files.allow_write(&link),
            Err(LocalAccessError::StateDirectory { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_cannot_hide_an_agent_config_dir() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let config = directory.path().join(".claude");
        fs::create_dir(&config).expect("config directory should create");
        let link = directory.path().join("innocent");
        std::os::unix::fs::symlink(&config, &link).expect("symlink should create");
        let files = LocalFiles::new(directory.path().join("state"), None);
        assert!(matches!(
            files.allow_write(&link.join("rules/note.md")),
            Err(LocalAccessError::AgentInstructions { .. })
        ));
    }

    #[test]
    fn a_root_from_dotenv_confines_and_still_refuses_instruction_files() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let files = LocalFiles::new(
            directory.path().join("state"),
            Some(directory.path().to_path_buf()),
        )
        .guarding_instructions();
        assert!(matches!(
            files.allow_write(&directory.path().join("CLAUDE.md")),
            Err(LocalAccessError::AgentInstructions { .. })
        ));
        assert!(
            files
                .allow_write(&directory.path().join("notes.md"))
                .is_ok()
        );
        assert!(matches!(
            files.allow_write(&abs("elsewhere/notes.md")),
            Err(LocalAccessError::OutsideRoot { .. })
        ));
    }

    #[test]
    fn a_root_the_user_chose_may_hold_agent_instruction_files() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let files = LocalFiles::new(
            directory.path().join("state"),
            Some(directory.path().to_path_buf()),
        );
        assert!(
            files
                .allow_write(&directory.path().join("CLAUDE.md"))
                .is_ok()
        );
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

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_tree_does_not_escape_it() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let root = directory.path().join("notes");
        fs::create_dir(&root).expect("root should create");
        let outside = directory.path().join("outside.md");
        fs::write(&outside, "secret").expect("outside file should write");
        std::os::unix::fs::symlink(&outside, root.join("link.md")).expect("symlink should create");

        let files = LocalFiles::new(directory.path().join("state"), Some(root.clone()));
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
            if let Ok(contents) = files
                .read_stamped(&link.join("note.md"), 64)
                .map(|(bytes, _)| bytes)
            {
                assert_eq!(contents, b"inside");
            }
            let written = link.join("written.md");
            let current = files.stamp(&written).ok().flatten();
            let expect = current.map_or(Expect::Absent, Expect::Stamp);
            let _ = files.write_atomic(&written, b"confined", false, expect);
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
                .write_atomic(&path, b"body", false, Expect::Absent)
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
            files.write_atomic(&root.join("note.md"), b"body", false, Expect::Absent),
            Err(LocalAccessError::ReplacedRoot { .. })
        ));
    }

    #[test]
    fn a_missing_root_is_reported_rather_than_ignored() {
        let files = LocalFiles::new(abs("tmp/state"), Some(abs("nonexistent/root")));
        assert!(matches!(
            files.allow(&abs("nonexistent/root/note.md")),
            Err(LocalAccessError::MissingRoot { .. })
        ));
    }
}
