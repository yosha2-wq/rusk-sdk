//! rusk-incremental: decides whether a per-ABI compile step in
//! `rusk-build` can be skipped because nothing that would affect its
//! output has changed since the last successful build.
//!
//! # The problem this solves
//!
//! `cargo build` already has its own incremental compilation within a
//! single target — but `rusk-build` re-runs the *entire* pipeline
//! (`cargo build` invocation, resource linking, dexing, signing) on
//! every `rusk build`, even when the only thing that happened between
//! two runs was rebuilding the same unchanged source a second time.
//! `cargo` itself will still skip recompiling unchanged crates, but
//! `rusk-build`'s own downstream steps (copying the `.so`, re-running
//! `aapt2 link`, re-signing) have no equivalent skip logic, so a
//! developer re-running `rusk build` to, say, regenerate an APK after
//! nothing changed pays the same cost as the first build.
//!
//! # How it decides "nothing changed"
//!
//! A [`BuildFingerprint`] is a content hash (SHA-256) computed over:
//! - every `.rs` file under the project's `src/` directory (recursively,
//!   sorted by path so the hash is order-independent of filesystem
//!   iteration order),
//! - the project's `Cargo.toml` and `Rusk.toml`,
//! - the resolved toolchain versions involved (NDK version, Android
//!   build-tools version, target triple, debug/release profile) — a
//!   fingerprint that matched on source alone but used a different NDK
//!   version would be a correctness bug, not a cache hit.
//!
//! This is deliberately *not* a timestamp-based check (mtimes are
//! notoriously unreliable across `git checkout`, CI cache restores, and
//! clock skew) and deliberately *not* a full `cargo build --dry-run`
//! dependency-graph analysis (which would need to duplicate a
//! significant part of Cargo's own build-plan logic to get right). A
//! content hash over the actual inputs is slower to compute than a
//! timestamp check but cannot be wrong in the way timestamps can, and is
//! simple enough to audit by reading this file top to bottom.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum IncrementalError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse incremental build record at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("failed to serialize incremental build record: {0}")]
    Serialize(#[from] toml::ser::Error),
}

/// Everything about the toolchain/target that affects a compile's
/// output but isn't part of the source tree itself — two builds with
/// byte-identical source but a different NDK version must never be
/// treated as a cache hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolchainContext {
    pub ndk_version: String,
    pub build_tools_version: String,
    pub rust_triple: String,
    pub release: bool,
    /// Cargo features enabled for this build, sorted — order must be
    /// normalized before hashing since `["a", "b"]` and `["b", "a"]`
    /// select the same build but would hash differently as raw strings.
    pub features: Vec<String>,
}

impl ToolchainContext {
    fn normalized(&self) -> Self {
        let mut features = self.features.clone();
        features.sort();
        features.dedup();
        Self {
            ndk_version: self.ndk_version.clone(),
            build_tools_version: self.build_tools_version.clone(),
            rust_triple: self.rust_triple.clone(),
            release: self.release,
            features,
        }
    }
}

/// A computed fingerprint for one (source tree, toolchain context) pair,
/// plus the metadata needed to report *why* a fingerprint changed —
/// "your NDK version changed" is a much more actionable message than
/// "cache miss" with no further explanation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildFingerprint {
    /// Hex-encoded SHA-256 over the concatenation of every hashed
    /// input, in the order documented on [`compute`].
    pub hash: String,
    pub toolchain: ToolchainContext,
    /// Number of source files that were hashed to produce this
    /// fingerprint — purely diagnostic (shown in `rusk cache stats`-style
    /// output), not used in the comparison itself.
    pub file_count: usize,
    pub computed_at: u64,
}

/// Computes a [`BuildFingerprint`] for `project_root`'s `src/` tree,
/// `Cargo.toml`, and `Rusk.toml`, combined with `toolchain`.
///
/// Hashing is over file *contents*, not paths+mtimes, and files are
/// visited in sorted path order so the result is deterministic
/// regardless of the OS's directory-listing order (which is unspecified
/// on most filesystems and does vary between Linux/macOS/Windows in
/// practice).
pub fn compute(project_root: &Path, toolchain: &ToolchainContext) -> Result<BuildFingerprint, IncrementalError> {
    let mut hasher = Sha256::new();
    let mut file_count = 0usize;

    let src_dir = project_root.join("src");
    let mut source_files: Vec<PathBuf> = if src_dir.is_dir() {
        walkdir::WalkDir::new(&src_dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter(|e| e.path().extension().map(|ext| ext == "rs").unwrap_or(false))
            .map(|e| e.path().to_path_buf())
            .collect()
    } else {
        Vec::new()
    };
    source_files.sort();

    for path in &source_files {
        hash_file_into(&mut hasher, path)?;
        file_count += 1;
    }

    for extra in ["Cargo.toml", "Rusk.toml", "build.rs"] {
        let path = project_root.join(extra);
        if path.is_file() {
            hash_file_into(&mut hasher, &path)?;
            file_count += 1;
        }
    }

    let normalized_toolchain = toolchain.normalized();
    // A stable, explicit serialization of the toolchain context is fed
    // into the same hash rather than hashed separately, so a toolchain
    // change and a source change can never accidentally cancel out to
    // the same combined hash (which a naive XOR-style combination could
    // risk; feeding everything through one hasher sequentially avoids
    // that class of bug entirely).
    hasher.update(normalized_toolchain.ndk_version.as_bytes());
    hasher.update(normalized_toolchain.build_tools_version.as_bytes());
    hasher.update(normalized_toolchain.rust_triple.as_bytes());
    hasher.update([normalized_toolchain.release as u8]);
    for feature in &normalized_toolchain.features {
        hasher.update(feature.as_bytes());
    }

    let hash = format!("{:x}", hasher.finalize());
    Ok(BuildFingerprint {
        hash,
        toolchain: normalized_toolchain,
        file_count,
        computed_at: unix_now(),
    })
}

fn hash_file_into(hasher: &mut Sha256, path: &Path) -> Result<(), IncrementalError> {
    let bytes = std::fs::read(path).map_err(|source| IncrementalError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    // The path itself is folded into the hash (not just the file's
    // bytes) so that renaming a file, or two files swapping identical
    // content, is still detected as a change — content-only hashing
    // would treat "renamed src/a.rs to src/b.rs" as a no-op, which is
    // wrong when the rename is itself a meaningful source change (e.g.
    // module reorganization that Cargo needs to see).
    hasher.update(path.to_string_lossy().as_bytes());
    hasher.update(&bytes);
    Ok(())
}

/// One entry in the on-disk incremental-build record: a fingerprint plus
/// where the artifact it corresponds to can be found, so a cache hit can
/// actually reuse the artifact, not just skip work with nothing to show
/// for it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedArtifact {
    pub fingerprint: BuildFingerprint,
    pub artifact_path: PathBuf,
    /// SHA-256 of the artifact file itself at the time it was cached —
    /// checked before reuse so a cache entry surviving an external
    /// modification to the artifact (a developer manually editing the
    /// `.so`, an antivirus quarantine-and-restore cycle, disk
    /// corruption) is detected and treated as a miss rather than
    /// silently reused.
    pub artifact_sha256: String,
}

/// The full on-disk record for a project — one [`CachedArtifact`] per
/// ABI target, keyed by Rust target triple, persisted as
/// `target/rusk-incremental.toml` inside the project so it travels with
/// (and is cleaned by) the project's own `target/` directory rather than
/// living in a separate global cache location.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IncrementalRecord {
    #[serde(default)]
    pub artifacts: BTreeMap<String, CachedArtifact>,
}

impl IncrementalRecord {
    pub fn load(path: &Path) -> Result<Self, IncrementalError> {
        if !path.is_file() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path).map_err(|source| IncrementalError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&text).map_err(|source| IncrementalError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn write(&self, path: &Path) -> Result<(), IncrementalError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| IncrementalError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let text = toml::to_string_pretty(self)?;
        let tmp_path = path.with_extension("toml.tmp-write");
        std::fs::write(&tmp_path, &text).map_err(|source| IncrementalError::Io {
            path: tmp_path.clone(),
            source,
        })?;
        std::fs::rename(&tmp_path, path).map_err(|source| IncrementalError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(())
    }

    /// Checks whether `triple`'s currently cached artifact is still
    /// valid for `fingerprint` — both the fingerprint hash matching
    /// *and* the artifact file's own content hash matching what was
    /// recorded when it was cached. Returns the artifact path on a hit.
    pub fn check(&self, triple: &str, fingerprint: &BuildFingerprint) -> CacheDecision {
        let Some(cached) = self.artifacts.get(triple) else {
            return CacheDecision::Miss(MissReason::NoPriorBuild);
        };
        if cached.fingerprint.hash != fingerprint.hash {
            let reason = if cached.fingerprint.toolchain != fingerprint.toolchain {
                MissReason::ToolchainChanged
            } else {
                MissReason::SourceChanged
            };
            return CacheDecision::Miss(reason);
        }
        if !cached.artifact_path.is_file() {
            return CacheDecision::Miss(MissReason::ArtifactMissing);
        }
        match hash_file_hex(&cached.artifact_path) {
            Ok(actual) if actual == cached.artifact_sha256 => {
                CacheDecision::Hit(cached.artifact_path.clone())
            }
            _ => CacheDecision::Miss(MissReason::ArtifactModifiedExternally),
        }
    }

    /// Records a freshly built artifact for `triple`, replacing any
    /// prior entry. Call this immediately after a successful compile so
    /// the next `rusk build` can potentially skip it.
    pub fn record(
        &mut self,
        triple: &str,
        fingerprint: BuildFingerprint,
        artifact_path: PathBuf,
    ) -> Result<(), IncrementalError> {
        let artifact_sha256 = hash_file_hex(&artifact_path)?;
        self.artifacts.insert(
            triple.to_string(),
            CachedArtifact {
                fingerprint,
                artifact_path,
                artifact_sha256,
            },
        );
        Ok(())
    }

    /// Drops every cached entry — used by `rusk clean` to make sure a
    /// clean build is genuinely clean, not accidentally short-circuited
    /// by a stale incremental record.
    pub fn clear(&mut self) {
        self.artifacts.clear();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheDecision {
    /// The cached artifact at this path is still valid and can be
    /// reused as-is without recompiling.
    Hit(PathBuf),
    Miss(MissReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissReason {
    NoPriorBuild,
    SourceChanged,
    ToolchainChanged,
    /// The record pointed at an artifact that no longer exists on disk
    /// (e.g. `target/` was partially cleaned by something other than
    /// `rusk clean`).
    ArtifactMissing,
    /// The artifact file's content no longer matches what was recorded
    /// when it was cached — treated as a miss rather than trusted,
    /// since something outside `rusk-build`'s control touched it.
    ArtifactModifiedExternally,
}

impl MissReason {
    pub fn describe(&self) -> &'static str {
        match self {
            MissReason::NoPriorBuild => "no cached build found for this target",
            MissReason::SourceChanged => "source files changed since the last build",
            MissReason::ToolchainChanged => "NDK/build-tools version or build profile changed",
            MissReason::ArtifactMissing => "the previously cached artifact no longer exists on disk",
            MissReason::ArtifactModifiedExternally => "the cached artifact's contents changed outside of rusk build",
        }
    }
}

fn hash_file_hex(path: &Path) -> Result<String, IncrementalError> {
    let bytes = std::fs::read(path).map_err(|source| IncrementalError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

/// The standard on-disk location for a project's incremental build
/// record — inside `target/` so `cargo clean`/a CI cache-wipe of
/// `target/` naturally invalidates it too, and so it never needs its own
/// separate `.gitignore` entry (the whole `target/` directory is already
/// ignored by every generated Rusk project).
pub fn record_path(project_root: &Path) -> PathBuf {
    project_root.join("target").join("rusk-incremental.toml")
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Human-readable summary of a project's incremental cache state, used
/// by `rusk cache stats` — separate from `rusk-cache`'s own
/// download/build-step log, since this is specifically about compile
/// skip/hit state rather than download history.
pub fn format_summary(record: &IncrementalRecord) -> String {
    if record.artifacts.is_empty() {
        return "no cached builds yet".to_string();
    }
    let mut lines = vec![format!("{} cached target(s):", record.artifacts.len())];
    for (triple, cached) in &record.artifacts {
        lines.push(format!(
            "  {triple}: {} ({})",
            cached.artifact_path.display(),
            if cached.fingerprint.toolchain.release { "release" } else { "debug" }
        ));
    }
    lines.join("\n")
}
