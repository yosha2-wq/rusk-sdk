//! rusk-ndk: locates a cached Android NDK, or downloads and unpacks one
//! from Google's official distribution point. The NDK's bundled clang is
//! also what stands in for a standalone LLVM toolchain — Rusk does not
//! need (and does not fetch) a separate LLVM install.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use rusk_ui::DownloadProgress;
use sha1::{Digest, Sha1};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum NdkError {
    #[error("network error while fetching {url}: {source}")]
    Network {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("unexpected HTTP status {status} while fetching {url}")]
    BadStatus { url: String, status: u16 },
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("zip extraction failed: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("unsupported host OS for NDK download: {0}")]
    UnsupportedHost(String),
    #[error("no known NDK release matches version \"{0}\"; pin a known version in Rusk.toml, e.g. \"27.0.12077973\"")]
    UnknownVersion(String),
    #[error("could not determine a home/cache directory for this user")]
    NoCacheDir,
    #[error("checksum mismatch for {path}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
}

/// Maps a semantic NDK "side-by-side" version (as published in
/// `Rusk.toml`) to the `rNN[letter]` release tag Google uses in its
/// download URLs, plus the published SHA-1 of each platform archive so
/// downloads can be verified without trusting the network alone.
///
/// SHA-1, not SHA-256: Google's own `android/ndk` GitHub releases (and
/// the NDK downloads page) publish SHA-1 checksums for each platform
/// archive, not SHA-256 — this table uses whatever the authoritative
/// source actually provides rather than a stronger-sounding but
/// unavailable-from-the-source algorithm. SHA-1 is no longer considered
/// collision-resistant for adversarial contexts, but for its purpose
/// here (catching a corrupted or truncated download, or a
/// man-in-the-middle substitution on a connection somehow not already
/// covered by TLS) it's exactly the check the NDK's own publisher
/// performs and documents, and matching that is more useful than
/// silently downgrading to an algorithm Google doesn't publish a
/// reference value for.
struct KnownRelease {
    version: &'static str,
    tag: &'static str,
    sha1_windows: &'static str,
    sha1_linux: &'static str,
    sha1_darwin: &'static str,
}

// Checksums below are the SHA-1 values Google publishes at
// https://github.com/android/ndk/releases/tag/<tag> for each platform
// archive. `verify_checksum` treats anything that isn't a 40-hex-char
// string as "not pinned yet" and skips verification with a visible
// warning instead of silently accepting a made-up hash, so adding a new
// KNOWN_RELEASES entry ahead of confirming its published checksum is
// still safe — it just won't be verified until the hash is filled in.
const KNOWN_RELEASES: &[KnownRelease] = &[
    KnownRelease {
        version: "27.0.12077973",
        tag: "r27c",
        sha1_windows: "ac5f7762764b1f15341094e148ad4f847d050c38",
        sha1_darwin: "04d8c43eb4e884c4b16bbf7733ac9179a13b7b20",
        sha1_linux: "090e8083a715fdb1a3e402d0763c388abb03fb4e",
    },
    KnownRelease {
        version: "26.3.11579264",
        tag: "r26d",
        sha1_windows: "c7ea35ffe916082876611da1a6d5618d15430c29",
        sha1_darwin: "703100c3d721b04e09f02f3fddc5f1f5ced28b10",
        sha1_linux: "fcdad75a765a46a9cf6560353f480db251d14765",
    },
];

pub struct NdkHome {
    pub root: PathBuf,
    pub version: String,
}

impl NdkHome {
    /// `clang` / `clang++` driver directory for the host triple.
    pub fn toolchain_bin(&self) -> PathBuf {
        let host_tag = host_prebuilt_tag();
        self.root
            .join("toolchains")
            .join("llvm")
            .join("prebuilt")
            .join(host_tag)
            .join("bin")
    }

    /// Path to the actual clang driver to invoke as the linker.
    ///
    /// On Windows this deliberately returns the version-independent
    /// `clang.exe` binary, NOT the `<clang_target><api>-clang.cmd`
    /// wrapper the NDK also ships. The `.cmd` wrapper is a batch script
    /// that re-parses its own `%*` argument list before re-invoking
    /// `clang.exe --target=... %*`; `cmd.exe`'s batch argument parsing
    /// treats `=`, `,`, and embedded quotes differently than the Win32
    /// `CreateProcess` argument passing cargo/rustc use, so a linker
    /// argument like `-Wl,--version-script=<path>` — which contains both
    /// `,` and `=` — comes out corrupted on the other side ("... was
    /// unexpected at this time", exit code 255). Calling `clang.exe`
    /// directly skips `cmd.exe` entirely; the `--target=<clang_target><api>`
    /// flag the `.cmd` wrapper would have added is instead supplied by
    /// the caller via `-C link-arg=--target=...` in `RUSTFLAGS` (see
    /// `rusk-build::compile_for_target`).
    ///
    /// On Unix hosts there is no `.cmd`-vs-`.exe` distinction, so the
    /// version-suffixed shell-script driver is used as before — it does
    /// not go through a second shell re-parse the way the Windows batch
    /// wrapper does.
    pub fn clang_driver(&self, clang_target: &str, api_level: u32) -> PathBuf {
        if cfg!(windows) {
            self.toolchain_bin().join("clang.exe")
        } else {
            self.toolchain_bin()
                .join(format!("{clang_target}{api_level}-clang"))
        }
    }

    pub fn clang_cxx_driver(&self, clang_target: &str, api_level: u32) -> PathBuf {
        if cfg!(windows) {
            self.toolchain_bin().join("clang++.exe")
        } else {
            self.toolchain_bin()
                .join(format!("{clang_target}{api_level}-clang++"))
        }
    }
}

fn host_prebuilt_tag() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows-x86_64"
    } else if cfg!(target_os = "macos") {
        "darwin-x86_64"
    } else {
        "linux-x86_64"
    }
}

fn host_download_tag() -> Result<&'static str, NdkError> {
    if cfg!(target_os = "windows") {
        Ok("windows")
    } else if cfg!(target_os = "macos") {
        Ok("darwin")
    } else if cfg!(target_os = "linux") {
        Ok("linux")
    } else {
        Err(NdkError::UnsupportedHost(std::env::consts::OS.to_string()))
    }
}

fn cache_root() -> Result<PathBuf, NdkError> {
    let base = dirs::home_dir().ok_or(NdkError::NoCacheDir)?;
    Ok(base.join(".rusk").join("ndk"))
}

/// Ensures the requested NDK version is present locally, downloading and
/// unpacking it if necessary. `version` may be `None`, in which case the
/// newest known release is used.
pub fn ensure_ndk(version: Option<&str>) -> Result<NdkHome, NdkError> {
    let release = match version {
        Some(v) => KNOWN_RELEASES
            .iter()
            .find(|r| r.version == v)
            .ok_or_else(|| NdkError::UnknownVersion(v.to_string()))?,
        None => KNOWN_RELEASES.last().expect("at least one known release"),
    };

    let root = cache_root()?.join(release.version);
    let marker = root.join(".rusk-complete");
    if marker.is_file() {
        return Ok(NdkHome {
            root,
            version: release.version.to_string(),
        });
    }

    std::fs::create_dir_all(&root).map_err(|source| NdkError::Io {
        path: root.clone(),
        source,
    })?;

    let tag = host_download_tag()?;
    let (expected_sha, filename) = match tag {
        "windows" => (
            release.sha1_windows,
            format!("android-ndk-{}-windows.zip", release.tag),
        ),
        "darwin" => (
            release.sha1_darwin,
            format!("android-ndk-{}-darwin.zip", release.tag),
        ),
        _ => (
            release.sha1_linux,
            format!("android-ndk-{}-linux.zip", release.tag),
        ),
    };
    let url = format!("https://dl.google.com/android/repository/{filename}");

    let archive_path = root.join(&filename);
    download_with_progress(&url, &archive_path, &format!("NDK {}", release.version))?;
    verify_checksum(&archive_path, expected_sha)?;

    let step = rusk_ui::Step::start(format!("Unpacking NDK {}", release.version));
    if let Err(e) = unpack_ndk_zip(&archive_path, &root) {
        step.fail(&e.to_string());
        return Err(e);
    }
    step.ok();

    // Archive is only needed for the extraction step.
    let _ = std::fs::remove_file(&archive_path);

    File::create(&marker)
        .and_then(|mut f| f.write_all(b"ok"))
        .map_err(|source| NdkError::Io {
            path: marker.clone(),
            source,
        })?;

    Ok(NdkHome {
        root,
        version: release.version.to_string(),
    })
}

fn download_with_progress(url: &str, dest: &Path, label: &str) -> Result<(), NdkError> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("rusk-sdk/0.1")
        .build()
        .map_err(|source| NdkError::Network {
            url: url.to_string(),
            source,
        })?;
    let mut resp = client
        .get(url)
        .send()
        .map_err(|source| NdkError::Network {
            url: url.to_string(),
            source,
        })?;
    if !resp.status().is_success() {
        return Err(NdkError::BadStatus {
            url: url.to_string(),
            status: resp.status().as_u16(),
        });
    }
    let total = resp.content_length().unwrap_or(0);
    let mut out = File::create(dest).map_err(|source| NdkError::Io {
        path: dest.to_path_buf(),
        source,
    })?;

    let mut progress = DownloadProgress::new(label.to_string(), total);
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = resp.read(&mut buf).map_err(|source| NdkError::Io {
            path: dest.to_path_buf(),
            source,
        })?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).map_err(|source| NdkError::Io {
            path: dest.to_path_buf(),
            source,
        })?;
        progress.add(n as u64);
        progress.tick_frame();
    }
    progress.finish();
    Ok(())
}

fn verify_checksum(path: &Path, expected_hex: &str) -> Result<(), NdkError> {
    // A checksum that isn't a full 40-hex-char SHA-1 is treated as "not
    // pinned yet" rather than failing the build — this lets new
    // releases be added to KNOWN_RELEASES ahead of confirming their
    // published hash.
    if expected_hex.len() != 40 {
        rusk_ui::warn("no verified checksum for this NDK release; skipping verification");
        return Ok(());
    }
    let mut file = File::open(path).map_err(|source| NdkError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha1::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|source| NdkError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected_hex {
        return Err(NdkError::ChecksumMismatch {
            path: path.to_path_buf(),
            expected: expected_hex.to_string(),
            actual,
        });
    }
    Ok(())
}

fn unpack_ndk_zip(archive: &Path, dest_root: &Path) -> Result<(), NdkError> {
    let file = File::open(archive).map_err(|source| NdkError::Io {
        path: archive.to_path_buf(),
        source,
    })?;
    let mut zip = zip::ZipArchive::new(file)?;
    let mut progress = rusk_ui::ExtractProgress::new("Unpacking NDK", zip.len());
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        // The archive's top-level directory is `android-ndk-<tag>/...`;
        // strip it so `dest_root` becomes the NDK root directly.
        let name = entry.name().to_string();
        progress.advance(&name);
        let stripped = match name.split_once('/') {
            Some((_, rest)) if !rest.is_empty() => rest,
            _ => continue,
        };
        let out_path = dest_root.join(stripped);
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path).map_err(|source| NdkError::Io {
                path: out_path.clone(),
                source,
            })?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| NdkError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let mut out_file = File::create(&out_path).map_err(|source| NdkError::Io {
            path: out_path.clone(),
            source,
        })?;
        std::io::copy(&mut entry, &mut out_file).map_err(|source| NdkError::Io {
            path: out_path.clone(),
            source,
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = entry.unix_mode() {
                let _ = std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode));
            }
        }
    }
    progress.finish();
    Ok(())
}

/// Locates `sdkmanager`-installed build tools (`aapt2`, `d8`, `zipalign`,
/// `apksigner`) via `ANDROID_HOME`/`ANDROID_SDK_ROOT`, falling back to
/// `which` for a system-wide install.
pub struct BuildTools {
    pub aapt2: PathBuf,
    pub d8: PathBuf,
    pub zipalign: PathBuf,
    pub apksigner: PathBuf,
}

pub fn locate_build_tools() -> Result<BuildTools, NdkError> {
    let sdk_root = std::env::var_os("ANDROID_HOME")
        .or_else(|| std::env::var_os("ANDROID_SDK_ROOT"))
        .map(PathBuf::from);

    let exe = if cfg!(windows) { ".exe" } else { "" };
    let bat = if cfg!(windows) { ".bat" } else { "" };

    if let Some(root) = sdk_root {
        let build_tools_dir = root.join("build-tools");
        if let Ok(entries) = std::fs::read_dir(&build_tools_dir) {
            let mut versions: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
            versions.sort();
            if let Some(latest) = versions.last() {
                return Ok(BuildTools {
                    aapt2: latest.join(format!("aapt2{exe}")),
                    d8: latest.join(format!("d8{bat}")),
                    zipalign: latest.join(format!("zipalign{exe}")),
                    apksigner: latest.join(format!("apksigner{bat}")),
                });
            }
        }
    }

    // Fall back to whatever is on PATH.
    let find = |name: &str| which::which(name).unwrap_or_else(|_| PathBuf::from(name));
    Ok(BuildTools {
        aapt2: find(&format!("aapt2{exe}")),
        d8: find(&format!("d8{bat}")),
        zipalign: find(&format!("zipalign{exe}")),
        apksigner: find(&format!("apksigner{bat}")),
    })
}
