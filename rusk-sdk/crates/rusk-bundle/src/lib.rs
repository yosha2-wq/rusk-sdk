//! rusk-bundle: produces an Android App Bundle (`.aab`) instead of (or
//! alongside) a plain APK.
//!
//! Google Play has required `.aab` uploads instead of universal `.apk`
//! files since August 2021 — Play itself splits the bundle into
//! per-device APKs at install time, which is how apps ship
//! architecture-specific `.so` files without bloating every install with
//! every ABI. `rusk-apk` already produces a perfectly good universal APK
//! for sideloading/`adb install`/`rusk run`, but a project that intends
//! to publish needs a bundle too. Rather than reimplement the protobuf
//! bundle format, this crate drives Google's own `bundletool.jar`
//! (auto-downloaded from its GitHub releases, since — unlike aapt2/d8/
//! zipalign/apksigner — it isn't part of the SDK build-tools package
//! `rusk-sdkmgr` provisions).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use rusk_manifest::RuskManifest;
use rusk_ui::DownloadProgress;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BundleError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("network error fetching bundletool: {0}")]
    Network(#[from] reqwest::Error),
    #[error("unexpected HTTP status {0} fetching bundletool")]
    BadStatus(u16),
    #[error("could not determine a home directory to cache bundletool in")]
    NoCacheDir,
    #[error("`java -jar bundletool.jar` exited with status {0}")]
    ToolFailed(i32),
    #[error("bundletool build-apks needs a keystore; pass one explicitly or configure [signing] in Rusk.toml")]
    NoKeystore,
    #[error("invalid bundle module: {0}")]
    InvalidModule(String),
}

/// The bundletool release Rusk pins by default. Bundletool has a stable
/// enough CLI surface that a slightly older release is rarely a problem,
/// but this can be overridden per-project via `[bundle] bundletool_version`.
pub const DEFAULT_BUNDLETOOL_VERSION: &str = "1.17.1";

fn cache_dir() -> Result<PathBuf, BundleError> {
    let home = dirs::home_dir().ok_or(BundleError::NoCacheDir)?;
    Ok(home.join(".rusk").join("bundletool"))
}

/// Ensures `bundletool-all-<version>.jar` is downloaded and cached under
/// `~/.rusk/bundletool/`, fetching it from bundletool's GitHub releases
/// if this is the first time this version has been requested.
pub fn ensure_bundletool(version: &str) -> Result<PathBuf, BundleError> {
    let dir = cache_dir()?;
    std::fs::create_dir_all(&dir).map_err(|source| BundleError::Io {
        path: dir.clone(),
        source,
    })?;
    let jar_path = dir.join(format!("bundletool-all-{version}.jar"));
    if jar_path.is_file() {
        return Ok(jar_path);
    }

    let url = format!(
        "https://github.com/google/bundletool/releases/download/{version}/bundletool-all-{version}.jar"
    );
    let client = reqwest::blocking::Client::builder()
        .user_agent("rusk-sdk")
        .build()?;
    let mut resp = client.get(&url).send()?;
    if !resp.status().is_success() {
        return Err(BundleError::BadStatus(resp.status().as_u16()));
    }
    let total = resp.content_length().unwrap_or(0);

    let tmp_path = dir.join(format!(".bundletool-{version}.tmp"));
    let mut file = std::fs::File::create(&tmp_path).map_err(|source| BundleError::Io {
        path: tmp_path.clone(),
        source,
    })?;

    let mut progress = DownloadProgress::new(format!("bundletool {version}"), total);
    let mut buf = [0u8; 64 * 1024];
    use std::io::Read;
    loop {
        let n = resp.read(&mut buf).map_err(|source| BundleError::Io {
            path: tmp_path.clone(),
            source,
        })?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|source| BundleError::Io {
            path: tmp_path.clone(),
            source,
        })?;
        progress.add(n as u64);
        progress.tick_frame();
    }
    progress.finish();
    drop(file);

    std::fs::rename(&tmp_path, &jar_path).map_err(|source| BundleError::Io {
        path: jar_path.clone(),
        source,
    })?;
    Ok(jar_path)
}

/// Everything `bundletool build-bundle` needs — mirrors the shape of the
/// staging directory `rusk-apk` already produces (compiled resources,
/// per-ABI native libs, optional dex) so `rusk-build` can feed the same
/// intermediates into both the APK and bundle paths without duplicating
/// resource compilation.
pub struct BundleInputs<'a> {
    pub bundletool_jar: &'a Path,
    /// Directory laid out as a bundletool "base module": `manifest/`,
    /// `dex/`, `lib/<abi>/`, `res/`, `assets/`, `resources.pb`.
    pub base_module_dir: &'a Path,
    pub out_aab: &'a Path,
}

/// Runs `bundletool build-bundle --modules=<base.zip> --output=<aab>`.
/// bundletool expects each module as a zip, not a bare directory, so
/// this zips `base_module_dir` into a temp file first.
///
/// This is the single-module path — equivalent to calling
/// [`build_bundle_multi_module`] with an empty `feature_modules` list.
/// Kept as a separate, simpler entry point since a project with no
/// dynamic feature modules (the common case) shouldn't need to reason
/// about the multi-module API at all.
pub fn build_bundle(inputs: &BundleInputs) -> Result<(), BundleError> {
    build_bundle_multi_module(inputs, &[])
}

/// One dynamic feature module: a named, independently-packaged piece of
/// an app that Play can deliver on-demand or conditionally, instead of
/// bundling it into every install. Laid out the same way a base module
/// is (`manifest/`, `dex/`, `lib/<abi>/`, `res/`, `assets/`), but with
/// its own `AndroidManifest.xml` declaring `<dist:module>` metadata
/// (delivery type, module name, fusing behavior) — `rusk-bundle` does
/// not generate that manifest content itself; it packages whatever
/// `module_dir` already contains, the same way it does for the base
/// module.
pub struct BundleModule<'a> {
    /// Module name as it will appear in the bundle and in
    /// `bundletool dump manifest --module <name>` — must match the
    /// `split` attribute the module's own `AndroidManifest.xml` declares
    /// internally, since bundletool cross-checks the two and rejects a
    /// mismatch rather than silently renaming one to match the other.
    pub name: &'a str,
    pub module_dir: &'a Path,
}

/// Builds a bundle from a base module plus any number of additional
/// feature modules, each zipped independently and passed to
/// `bundletool build-bundle` as a comma-separated `--modules` list —
/// the real multi-module assembly path noted as missing in earlier
/// versions of this crate (it previously only supported a single base
/// module, packaged as if the whole app were one module).
///
/// Each module is validated to have a non-empty name and an existing
/// directory before any zipping happens, so a typo in a module list
/// fails fast with a clear error rather than partway through packaging
/// three of five modules.
pub fn build_bundle_multi_module(
    inputs: &BundleInputs,
    feature_modules: &[BundleModule],
) -> Result<(), BundleError> {
    for module in feature_modules {
        if module.name.trim().is_empty() {
            return Err(BundleError::InvalidModule(
                "feature module has an empty name".to_string(),
            ));
        }
        if !module.module_dir.is_dir() {
            return Err(BundleError::InvalidModule(format!(
                "feature module \"{}\" points at {}, which is not a directory",
                module.name,
                module.module_dir.display()
            )));
        }
    }

    let step = rusk_ui::Step::start(format!(
        "Packaging {} module{}",
        1 + feature_modules.len(),
        if feature_modules.is_empty() { "" } else { "s" }
    ));
    let base_zip = inputs.out_aab.with_extension("basemodule.zip.tmp");
    if let Err(e) = zip_directory(inputs.base_module_dir, &base_zip) {
        step.fail(&e.to_string());
        return Err(e);
    }

    let mut module_zips: Vec<PathBuf> = vec![base_zip.clone()];
    let mut cleanup_result = Ok(());
    for module in feature_modules {
        let module_zip = inputs
            .out_aab
            .with_extension(format!("module-{}.zip.tmp", sanitize_module_name(module.name)));
        if let Err(e) = zip_directory(module.module_dir, &module_zip) {
            cleanup_result = Err(e);
            break;
        }
        module_zips.push(module_zip);
    }
    if let Err(e) = cleanup_result {
        // Best-effort cleanup of whatever zips were already produced
        // before the failure — leaving partial .tmp files around after
        // an error is confusing clutter, not useful debugging evidence,
        // since the error itself already identifies which module failed.
        for zip in &module_zips {
            let _ = std::fs::remove_file(zip);
        }
        step.fail(&e.to_string());
        return Err(e);
    }
    step.ok();

    let step = rusk_ui::Step::start(format!(
        "Building app bundle ({} module{}, bundletool build-bundle)",
        module_zips.len(),
        if module_zips.len() == 1 { "" } else { "s" }
    ));
    let modules_arg = module_zips
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(",");
    let status = Command::new("java")
        .arg("-jar")
        .arg(inputs.bundletool_jar)
        .arg("build-bundle")
        .arg(format!("--modules={modules_arg}"))
        .arg(format!("--output={}", inputs.out_aab.display()))
        .arg("--overwrite")
        .status()
        .map_err(|source| BundleError::Io {
            path: PathBuf::from("java"),
            source,
        });

    for zip in &module_zips {
        let _ = std::fs::remove_file(zip);
    }

    let status = status?;
    if !status.success() {
        step.fail("bundletool exited with an error");
        return Err(BundleError::ToolFailed(status.code().unwrap_or(-1)));
    }
    step.ok();
    Ok(())
}

/// Bundle module names can contain characters (e.g. nothing unusual in
/// practice, but this is defensive) that aren't safe to use directly in
/// a filename — this produces a filesystem-safe stand-in used only for
/// naming this module's temporary zip, not for anything bundletool
/// itself sees (the module's real name is still passed through
/// unmodified inside the zip's own manifest).
fn sanitize_module_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Signing material for `bundletool build-apks`, kept separate from
/// `rusk-keystore::GenerateOptions` since bundletool only needs the
/// already-existing keystore's location/credentials, not the ability to
/// create one.
pub struct BundleSigning<'a> {
    pub keystore_path: &'a Path,
    pub alias: &'a str,
    pub store_password: &'a str,
    pub key_password: &'a str,
}

/// Runs `bundletool build-apks` to produce a local `.apks` archive from
/// an `.aab` — the fastest way to sanity-check what Play's device-
/// targeted APK splitting will actually produce for a given device,
/// without uploading anything to Play Console first.
pub fn build_apks_for_testing(
    bundletool_jar: &Path,
    aab_path: &Path,
    signing: &BundleSigning,
    out_apks: &Path,
    connected_device_only: bool,
) -> Result<(), BundleError> {
    let step = rusk_ui::Step::start("Generating device APK set (bundletool build-apks)");
    let mut cmd = Command::new("java");
    cmd.arg("-jar").arg(bundletool_jar);
    cmd.arg("build-apks");
    cmd.arg(format!("--bundle={}", aab_path.display()));
    cmd.arg(format!("--output={}", out_apks.display()));
    cmd.arg("--overwrite");
    cmd.arg(format!("--ks={}", signing.keystore_path.display()));
    cmd.arg(format!("--ks-key-alias={}", signing.alias));
    cmd.arg(format!("--ks-pass=pass:{}", signing.store_password));
    cmd.arg(format!("--key-pass=pass:{}", signing.key_password));
    if connected_device_only {
        cmd.arg("--connected-device");
    }
    let status = cmd.status().map_err(|source| BundleError::Io {
        path: PathBuf::from("java"),
        source,
    })?;
    if !status.success() {
        step.fail("bundletool exited with an error");
        return Err(BundleError::ToolFailed(status.code().unwrap_or(-1)));
    }
    step.ok();
    Ok(())
}

/// Installs the device-matched APK set from an `.apks` archive onto a
/// connected device/emulator via `bundletool install-apks` — the bundle
/// equivalent of `adb install`, since a raw `.apks` archive isn't
/// directly installable with plain `adb`.
pub fn install_apks(bundletool_jar: &Path, apks_path: &Path, device_serial: Option<&str>) -> Result<(), BundleError> {
    let step = rusk_ui::Step::start("Installing APK set (bundletool install-apks)");
    let mut cmd = Command::new("java");
    cmd.arg("-jar").arg(bundletool_jar);
    cmd.arg("install-apks");
    cmd.arg(format!("--apks={}", apks_path.display()));
    if let Some(serial) = device_serial {
        cmd.arg(format!("--device-id={serial}"));
    }
    let status = cmd.status().map_err(|source| BundleError::Io {
        path: PathBuf::from("java"),
        source,
    })?;
    if !status.success() {
        step.fail("bundletool exited with an error");
        return Err(BundleError::ToolFailed(status.code().unwrap_or(-1)));
    }
    step.ok();
    Ok(())
}

/// Reads `[bundle] bundletool_version` from the manifest, falling back
/// to [`DEFAULT_BUNDLETOOL_VERSION`].
pub fn bundletool_version_for(manifest: &RuskManifest) -> String {
    manifest
        .bundle
        .as_ref()
        .and_then(|b| b.bundletool_version.clone())
        .unwrap_or_else(|| DEFAULT_BUNDLETOOL_VERSION.to_string())
}

fn zip_directory(src_dir: &Path, out_zip: &Path) -> Result<(), BundleError> {
    if let Some(parent) = out_zip.parent() {
        std::fs::create_dir_all(parent).map_err(|source| BundleError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let file = std::fs::File::create(out_zip).map_err(|source| BundleError::Io {
        path: out_zip.to_path_buf(),
        source,
    })?;
    let mut writer = zip::ZipWriter::new(file);
    let options = zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    for entry in walkdir::WalkDir::new(src_dir).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        let rel = path.strip_prefix(src_dir).unwrap_or(path);
        if rel.as_os_str().is_empty() {
            continue;
        }
        let name = rel.to_string_lossy().replace('\\', "/");
        if path.is_dir() {
            writer
                .add_directory(format!("{name}/"), options)
                .map_err(|e| BundleError::Io {
                    path: path.to_path_buf(),
                    source: std::io::Error::new(std::io::ErrorKind::Other, e.to_string()),
                })?;
        } else {
            writer.start_file(name, options).map_err(|e| BundleError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::new(std::io::ErrorKind::Other, e.to_string()),
            })?;
            let mut f = std::fs::File::open(path).map_err(|source| BundleError::Io {
                path: path.to_path_buf(),
                source,
            })?;
            std::io::copy(&mut f, &mut writer).map_err(|source| BundleError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        }
    }
    writer.finish().map_err(|e| BundleError::Io {
        path: out_zip.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::Other, e.to_string()),
    })?;
    Ok(())
}
