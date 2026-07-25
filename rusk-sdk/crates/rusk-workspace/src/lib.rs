//! rusk-workspace: multi-project support for repositories that contain
//! more than one Rusk app — e.g. a phone app and a wear/TV variant
//! sharing a Rust workspace, or several example apps next to a shared
//! library crate. A `Rusk.workspace.toml` at the repo root lists member
//! project directories (each with its own `Rusk.toml`); `rusk build
//! --all` / `rusk lint --all` then operate over every member in one
//! invocation instead of the developer `cd`-ing into each one by hand.
//!
//! This is deliberately independent of Cargo's own `[workspace]`
//! concept: a Rusk workspace groups *Android app projects* (each with
//! its own `Rusk.toml`, own `AndroidManifest.xml`, own APK output), which
//! may or may not also happen to share a single Cargo workspace
//! underneath. Requiring them to be the same thing would rule out the
//! common case of one shared Rust library crate backing several
//! independently-versioned Android apps.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("Rusk.workspace.toml not found in {0} (or any parent directory)")]
    NotFound(PathBuf),
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse Rusk.workspace.toml: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("member \"{0}\" listed in Rusk.workspace.toml has no Rusk.toml at that path")]
    MemberMissingManifest(String),
    #[error("member path \"{0}\" appears more than once (after glob expansion)")]
    DuplicateMember(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceFile {
    /// Member project directories, relative to the workspace root. A
    /// trailing `/*` expands to every immediate subdirectory containing
    /// a `Rusk.toml` — e.g. `"examples/*"` picks up every example app
    /// without listing each one by name.
    pub members: Vec<String>,
    /// Directories to skip during `/*` glob expansion (build output,
    /// vendored dependencies, etc.) — has no effect on explicitly
    /// (non-glob) listed members.
    #[serde(default)]
    pub exclude: Vec<String>,
}

pub struct WorkspaceMember {
    pub name: String,
    pub dir: PathBuf,
    pub manifest_path: PathBuf,
}

pub struct Workspace {
    pub root: PathBuf,
    pub members: Vec<WorkspaceMember>,
}

impl Workspace {
    /// Walks up from `start_dir` looking for `Rusk.workspace.toml`,
    /// mirroring `RuskManifest::discover`'s own upward search so both
    /// can be invoked from any subdirectory of the project tree.
    pub fn discover(start_dir: &Path) -> Result<Self, WorkspaceError> {
        let mut dir = start_dir.to_path_buf();
        loop {
            let candidate = dir.join("Rusk.workspace.toml");
            if candidate.is_file() {
                return Self::load(&candidate);
            }
            if !dir.pop() {
                return Err(WorkspaceError::NotFound(start_dir.to_path_buf()));
            }
        }
    }

    pub fn load(path: &Path) -> Result<Self, WorkspaceError> {
        let text = std::fs::read_to_string(path).map_err(|source| WorkspaceError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let file: WorkspaceFile = toml::from_str(&text)?;
        let root = path.parent().unwrap_or(Path::new(".")).to_path_buf();

        let mut members = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for pattern in &file.members {
            if let Some(prefix) = pattern.strip_suffix("/*") {
                let base = root.join(prefix);
                if !base.is_dir() {
                    continue;
                }
                let mut entries: Vec<PathBuf> = std::fs::read_dir(&base)
                    .map_err(|source| WorkspaceError::Io {
                        path: base.clone(),
                        source,
                    })?
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect();
                entries.sort();

                for dir in entries {
                    let rel = dir.strip_prefix(&root).unwrap_or(&dir).to_string_lossy().replace('\\', "/");
                    if file.exclude.iter().any(|ex| ex == &rel) {
                        continue;
                    }
                    let manifest_path = dir.join("Rusk.toml");
                    if !manifest_path.is_file() {
                        continue; // glob expansion silently skips non-Rusk dirs
                    }
                    push_member(&mut members, &mut seen, &root, dir, manifest_path)?;
                }
            } else {
                let dir = root.join(pattern);
                let manifest_path = dir.join("Rusk.toml");
                if !manifest_path.is_file() {
                    return Err(WorkspaceError::MemberMissingManifest(pattern.clone()));
                }
                push_member(&mut members, &mut seen, &root, dir, manifest_path)?;
            }
        }

        Ok(Self { root, members })
    }

    /// True if `dir` (or any of its ancestors up to the workspace root)
    /// is a listed member — used by `rusk build`/`rusk lint` to decide
    /// whether the current directory is inside a workspace at all before
    /// trying `--all`.
    pub fn contains(&self, dir: &Path) -> bool {
        self.members.iter().any(|m| dir.starts_with(&m.dir))
    }
}

fn push_member(
    members: &mut Vec<WorkspaceMember>,
    seen: &mut std::collections::HashSet<PathBuf>,
    root: &Path,
    dir: PathBuf,
    manifest_path: PathBuf,
) -> Result<(), WorkspaceError> {
    let canonical = dir.canonicalize().unwrap_or_else(|_| dir.clone());
    if !seen.insert(canonical) {
        return Err(WorkspaceError::DuplicateMember(
            dir.strip_prefix(root).unwrap_or(&dir).to_string_lossy().to_string(),
        ));
    }
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| dir.to_string_lossy().to_string());
    members.push(WorkspaceMember { name, dir, manifest_path });
    Ok(())
}

/// Renders a starter `Rusk.workspace.toml`, analogous to
/// `rusk_manifest::render_new_manifest` for the per-project file.
pub fn render_new_workspace_file() -> String {
    r#"# Lists every Rusk project directory in this repository. `rusk build
# --all` / `rusk lint --all` operate on every listed member in one
# invocation. A trailing "/*" expands to every immediate subdirectory
# that contains a Rusk.toml.
members = [
    "apps/*",
]
exclude = []
"#
    .to_string()
}
