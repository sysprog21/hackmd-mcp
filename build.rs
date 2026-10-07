//! Records which source a binary was built from. Every build of one release
//! reports the same package version, so an installed binary older than the
//! checkout it came from is otherwise invisible: its tools just behave as the
//! old source did. The commit, marked `-dirty` when tracked files differ from
//! it, goes into `--version` and the self-check report.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

/// The tracked paths the binary is built from: an edit to one reruns this
/// script and marks the build dirty. Docs and the integration tests under
/// `tests/` do neither; unit tests live in `src` and do both.
const INPUTS: [&str; 4] = ["build.rs", "Cargo.toml", "Cargo.lock", "src"];

/// Runs git against the manifest's checkout. `GIT_DIR`, `GIT_WORK_TREE`, and
/// `GIT_INDEX_FILE` are dropped: inside a git hook they name the hook's
/// repository, work tree, and temporary index, not this checkout.
/// `--no-optional-locks` keeps `status` from refreshing the index, which would
/// race a concurrent commit for `index.lock` and touch a watched path,
/// rerunning this script next build.
fn git(manifest: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(manifest)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Where git keeps `name` for this checkout, worktrees included.
fn git_path(manifest: &Path, name: &str) -> Option<PathBuf> {
    git(manifest, &["rev-parse", "--git-path", name]).map(|path| manifest.join(path))
}

/// Whether the manifest is the top of its own checkout: built from a tarball
/// inside some other repository, that repository's commit would be a lie, and
/// its activity no reason to rerun.
fn owns_checkout(manifest: &Path) -> bool {
    git(manifest, &["rev-parse", "--show-toplevel"])
        .and_then(|top| Path::new(&top).canonicalize().ok())
        .is_some_and(|top| {
            manifest
                .canonicalize()
                .is_ok_and(|manifest| top == manifest)
        })
}

/// The commit, marked dirty when an input differs from it, a new untracked
/// source included (`.gitignore`d files are not). A status git could not read
/// leaves the commit unknown rather than claiming it clean.
fn commit(manifest: &Path) -> Option<String> {
    let hash = git(manifest, &["rev-parse", "--short=12", "HEAD"])?;
    // Explicit, so a `status.showUntrackedFiles=no` setting cannot hide one.
    let mut args = vec!["status", "--porcelain", "--untracked-files=normal", "--"];
    args.extend(INPUTS);
    let status = git(manifest, &args)?;
    Some(if status.is_empty() {
        hash
    } else {
        format!("{hash}-dirty")
    })
}

/// Declaring any path turns off Cargo's own rescan, so everything that can
/// change the commit is listed: what HEAD names and where that branch points
/// (loose or packed). Edits to the inputs are watched in `main`; the index is
/// not, since staging changes neither the commit nor the inputs. Cargo reruns
/// a script on every build while a watched path is missing, and a packed
/// branch has no loose ref (nor an unpacked repository a `packed-refs`), so
/// only paths that exist are named. The branch is watched through its
/// directory instead, which also sees the ref appear or vanish.
fn watch(manifest: &Path) {
    let mut paths: Vec<PathBuf> = ["HEAD", "packed-refs"]
        .into_iter()
        .filter_map(|name| git_path(manifest, name))
        .collect();
    if let Some(branch) = git(manifest, &["symbolic-ref", "-q", "HEAD"])
        && let Some(path) = git_path(manifest, &branch)
        && let Some(parent) = path.parent()
    {
        paths.push(parent.to_path_buf());
    }
    for path in paths.iter().filter(|path| path.exists()) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let manifest = Path::new(&manifest);
    let version = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");

    for path in INPUTS {
        println!("cargo:rerun-if-changed={path}");
    }
    let commit = if owns_checkout(manifest) {
        watch(manifest);
        let commit = commit(manifest);
        // Outside a checkout no commit is expected; inside one, its absence is
        // a git failure, and a silent one would look like a build from a
        // tarball.
        if commit.is_none() {
            println!(
                "cargo:warning=git could not describe this checkout; the build reports no commit"
            );
        }
        commit
    } else {
        None
    };
    let text = match &commit {
        Some(commit) => format!("{version} ({commit})"),
        None => version,
    };
    let commit = commit.unwrap_or_default();
    println!("cargo:rustc-env=HACKMD_MCP_COMMIT={commit}");
    println!("cargo:rustc-env=HACKMD_MCP_VERSION_TEXT={text}");
}
