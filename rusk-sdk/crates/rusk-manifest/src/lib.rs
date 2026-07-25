//! rusk-manifest: parses and validates `Rusk.toml`, the project descriptor
//! consumed by every other crate in the Rusk toolchain.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("Rusk.toml not found in {0} (or any parent directory)")]
    NotFound(PathBuf),
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse Rusk.toml: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid `package.id` \"{0}\": must be a reverse-DNS Java package identifier, e.g. com.example.app")]
    InvalidAppId(String),
    #[error("`package.entry` points to \"{0}\", but that file does not exist")]
    MissingEntry(PathBuf),
    #[error("unknown ABI \"{0}\"; expected one of: arm64-v8a, armeabi-v7a, x86_64, x86")]
    UnknownAbi(String),
    #[error("`package.sdk_min` ({sdk_min}) cannot be greater than `package.sdk_target` ({sdk_target})")]
    SdkRange { sdk_min: u32, sdk_target: u32 },
    #[error("no [[flavor]] named \"{0}\" in Rusk.toml")]
    UnknownFlavor(String),
}

/// Root of the parsed `Rusk.toml` document.
#[derive(Debug, Clone, Deserialize)]
pub struct RuskManifest {
    pub package: PackageSection,
    #[serde(default)]
    pub app: AppSection,
    #[serde(default)]
    pub abi: AbiSection,
    #[serde(default)]
    pub permissions: PermissionsSection,
    #[serde(default)]
    pub dependencies: DependenciesSection,
    #[serde(default)]
    pub ndk: NdkSection,
    #[serde(default)]
    pub sdk: SdkToolsSection,
    #[serde(default)]
    pub signing: Option<SigningSection>,
    /// Named build variants — each overlays a partial set of overrides on
    /// top of the base manifest (see [`RuskManifest::apply_flavor`]).
    #[serde(default)]
    pub flavor: Vec<FlavorSection>,
    #[serde(default)]
    pub bundle: Option<BundleSection>,
    #[serde(default)]
    pub proguard: Option<ProguardSection>,
    #[serde(default)]
    pub lint: LintSection,
}

/// `[bundle]` — configuration for `rusk bundle` (Android App Bundle
/// generation via bundletool). Entirely optional; a project that never
/// intends to publish to Play never needs this section.
#[derive(Debug, Clone, Deserialize)]
pub struct BundleSection {
    #[serde(default)]
    pub bundletool_version: Option<String>,
}

/// `[proguard]` — configuration for R8/ProGuard minification of the
/// generated Java glue. Off by default: a Rusk app's Java surface is
/// small enough that shrinking it rarely matters, and turning it on
/// unconditionally would surprise anyone hitting a stripped-symbol JNI
/// crash without having asked for minification.
#[derive(Debug, Clone, Deserialize)]
pub struct ProguardSection {
    #[serde(default)]
    pub enabled: bool,
    /// Path (relative to the project root) to hand-written rules to
    /// append after Rusk's own generated keep rules.
    #[serde(default)]
    pub extra_rules: Option<PathBuf>,
}

/// `[lint]` — which `rusk lint` checks to skip. Every check is on by
/// default; this section exists purely to suppress specific checks a
/// project has a deliberate reason to violate (e.g. a broad permission
/// list for a system-level utility app).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LintSection {
    #[serde(default)]
    pub ignore: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PackageSection {
    /// Human-readable project name.
    pub name: String,
    /// Reverse-DNS Android application id, e.g. `com.example.app`.
    pub id: String,
    #[serde(default = "default_version")]
    pub version: String,
    /// Path to the crate entry point, relative to the project root.
    #[serde(default = "default_entry")]
    pub entry: PathBuf,
    #[serde(default = "default_sdk_min")]
    pub sdk_min: u32,
    #[serde(default = "default_sdk_target")]
    pub sdk_target: u32,
    #[serde(default = "default_sdk_target")]
    pub sdk_compile: u32,
}

fn default_version() -> String {
    "0.1.0".to_string()
}
fn default_entry() -> PathBuf {
    PathBuf::from("src/main.rs")
}
fn default_sdk_min() -> u32 {
    24
}
fn default_sdk_target() -> u32 {
    34
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppSection {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default = "default_orientation")]
    pub orientation: Orientation,
    #[serde(default)]
    pub fullscreen: bool,
    #[serde(default)]
    pub debuggable: bool,
    /// Optional path to a PNG icon (any size, will be used for all
    /// densities — Rusk does not synthesize icons for you).
    #[serde(default)]
    pub icon: Option<PathBuf>,
}

impl Default for AppSection {
    fn default() -> Self {
        Self {
            label: None,
            orientation: default_orientation(),
            fullscreen: false,
            debuggable: false,
            icon: None,
        }
    }
}

fn default_orientation() -> Orientation {
    Orientation::Unspecified
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Orientation {
    Unspecified,
    Portrait,
    Landscape,
    Sensor,
}

impl Orientation {
    pub fn as_android_value(&self) -> &'static str {
        match self {
            Orientation::Unspecified => "unspecified",
            Orientation::Portrait => "portrait",
            Orientation::Landscape => "landscape",
            Orientation::Sensor => "sensor",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AbiSection {
    #[serde(default = "default_targets")]
    pub targets: Vec<String>,
}

impl Default for AbiSection {
    fn default() -> Self {
        Self {
            targets: default_targets(),
        }
    }
}

fn default_targets() -> Vec<String> {
    vec!["arm64-v8a".to_string()]
}

/// Maps an Android ABI name to the Rust target triple and NDK-side clang
/// target prefix used when invoking the cross compiler.
#[derive(Debug, Clone, Copy)]
pub struct AbiTarget {
    pub abi: &'static str,
    pub rust_triple: &'static str,
    pub clang_target: &'static str,
    pub jni_libs_dir: &'static str,
}

pub const KNOWN_ABIS: &[AbiTarget] = &[
    AbiTarget {
        abi: "arm64-v8a",
        rust_triple: "aarch64-linux-android",
        clang_target: "aarch64-linux-android",
        jni_libs_dir: "arm64-v8a",
    },
    AbiTarget {
        abi: "armeabi-v7a",
        rust_triple: "armv7-linux-androideabi",
        clang_target: "armv7a-linux-androideabi",
        jni_libs_dir: "armeabi-v7a",
    },
    AbiTarget {
        abi: "x86_64",
        rust_triple: "x86_64-linux-android",
        clang_target: "x86_64-linux-android",
        jni_libs_dir: "x86_64",
    },
    AbiTarget {
        abi: "x86",
        rust_triple: "i686-linux-android",
        clang_target: "i686-linux-android",
        jni_libs_dir: "x86",
    },
];

pub fn resolve_abi(name: &str) -> Option<&'static AbiTarget> {
    KNOWN_ABIS.iter().find(|a| a.abi == name)
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PermissionsSection {
    /// Short permission names, e.g. "INTERNET", "CAMERA". Rusk expands
    /// these to `android.permission.<NAME>` when generating the manifest.
    #[serde(default)]
    pub list: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DependenciesSection {
    /// Java/Kotlin dependencies pulled from Maven, keyed as
    /// `"group:artifact" = "version"`.
    #[serde(default)]
    pub java: BTreeMap<String, String>,
    /// Extra Maven repositories, searched in order before Google/Maven
    /// Central.
    #[serde(default)]
    pub repositories: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NdkSection {
    /// Pinned NDK version. If omitted, Rusk installs the latest version
    /// it knows about at build time.
    #[serde(default)]
    pub version: Option<String>,
}

impl Default for NdkSection {
    fn default() -> Self {
        Self { version: None }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SdkToolsSection {
    /// Android SDK build-tools revision to use for `aapt2`/`d8`/`zipalign`.
    /// When `ANDROID_HOME` isn't set, Rusk provisions exactly this
    /// revision (plus the matching `platforms;android-<sdk_compile>` and
    /// `platform-tools`) under `~/.rusk/android-sdk` automatically.
    #[serde(default = "default_build_tools_version")]
    pub build_tools_version: String,
}

impl Default for SdkToolsSection {
    fn default() -> Self {
        Self {
            build_tools_version: default_build_tools_version(),
        }
    }
}

fn default_build_tools_version() -> String {
    "34.0.0".to_string()
}

/// A named build variant. `rusk build --flavor <name>` clones the base
/// manifest and applies whichever of these fields are set, so e.g. a
/// "pro" flavor can add a suffixed application id, extra Cargo features,
/// and a narrower ABI list without duplicating the whole `Rusk.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct FlavorSection {
    pub name: String,
    #[serde(default)]
    pub id_suffix: Option<String>,
    #[serde(default)]
    pub label_suffix: Option<String>,
    #[serde(default)]
    pub cargo_features: Vec<String>,
    #[serde(default)]
    pub abi_targets: Option<Vec<String>>,
    #[serde(default)]
    pub java: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SigningSection {
    pub keystore: PathBuf,
    pub alias: String,
    /// Env var name to read the store password from, so secrets never
    /// live in Rusk.toml itself. Defaults to `RUSK_KEYSTORE_PASSWORD`.
    #[serde(default = "default_password_env")]
    pub password_env: String,
}

fn default_password_env() -> String {
    "RUSK_KEYSTORE_PASSWORD".to_string()
}

impl RuskManifest {
    /// Loads and validates `Rusk.toml`, walking up from `start_dir` if it
    /// isn't found in the given directory (mirrors Cargo's own lookup).
    pub fn discover(start_dir: &Path) -> Result<(Self, PathBuf), ManifestError> {
        let mut dir = start_dir.to_path_buf();
        loop {
            let candidate = dir.join("Rusk.toml");
            if candidate.is_file() {
                let manifest = Self::load(&candidate)?;
                return Ok((manifest, dir));
            }
            if !dir.pop() {
                return Err(ManifestError::NotFound(start_dir.to_path_buf()));
            }
        }
    }

    pub fn load(path: &Path) -> Result<Self, ManifestError> {
        let text = std::fs::read_to_string(path).map_err(|source| ManifestError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let manifest: RuskManifest = toml::from_str(&text)?;
        manifest.validate(path.parent().unwrap_or(Path::new(".")))?;
        Ok(manifest)
    }

    pub fn validate(&self, project_root: &Path) -> Result<(), ManifestError> {
        if !is_valid_app_id(&self.package.id) {
            return Err(ManifestError::InvalidAppId(self.package.id.clone()));
        }
        let entry_path = project_root.join(&self.package.entry);
        if !entry_path.is_file() {
            return Err(ManifestError::MissingEntry(entry_path));
        }
        for abi in &self.abi.targets {
            if resolve_abi(abi).is_none() {
                return Err(ManifestError::UnknownAbi(abi.clone()));
            }
        }
        if self.package.sdk_min > self.package.sdk_target {
            return Err(ManifestError::SdkRange {
                sdk_min: self.package.sdk_min,
                sdk_target: self.package.sdk_target,
            });
        }
        Ok(())
    }

    pub fn app_label(&self) -> String {
        self.app
            .label
            .clone()
            .unwrap_or_else(|| self.package.name.clone())
    }

    pub fn resolved_targets(&self) -> Vec<&'static AbiTarget> {
        self.abi
            .targets
            .iter()
            .filter_map(|a| resolve_abi(a))
            .collect()
    }

    pub fn java_dependencies(&self) -> impl Iterator<Item = (&str, &str)> {
        self.dependencies
            .java
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Produces the effective manifest for a named `[[flavor]]`, along
    /// with the extra `--features` cargo should be invoked with. Passing
    /// `None` returns the base manifest unchanged with no extra features.
    pub fn apply_flavor(&self, name: Option<&str>) -> Result<(RuskManifest, Vec<String>), ManifestError> {
        let Some(name) = name else {
            return Ok((self.clone(), Vec::new()));
        };
        let flavor = self
            .flavor
            .iter()
            .find(|f| f.name == name)
            .ok_or_else(|| ManifestError::UnknownFlavor(name.to_string()))?;

        let mut effective = self.clone();
        if let Some(suffix) = &flavor.id_suffix {
            effective.package.id = format!("{}.{}", effective.package.id, suffix);
        }
        if let Some(suffix) = &flavor.label_suffix {
            let base = effective.app_label();
            effective.app.label = Some(format!("{base} {suffix}"));
        }
        if let Some(targets) = &flavor.abi_targets {
            effective.abi.targets = targets.clone();
        }
        for (k, v) in &flavor.java {
            effective.dependencies.java.insert(k.clone(), v.clone());
        }
        Ok((effective, flavor.cargo_features.clone()))
    }
}

fn is_valid_app_id(id: &str) -> bool {
    let parts: Vec<&str> = id.split('.').collect();
    if parts.len() < 2 {
        return false;
    }
    parts.iter().all(|p| {
        !p.is_empty()
            && p.chars().next().map(|c| c.is_ascii_alphabetic()).unwrap_or(false)
            && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Renders a fresh `Rusk.toml` for `rusk new`. Values are filled in from
/// the project name — nothing here is a static template file, it is
/// composed at generation time so future schema fields stay in sync with
/// [`RuskManifest`] automatically.
pub fn render_new_manifest(project_name: &str, app_id: &str) -> String {
    format!(
        r#"[package]
name = "{project_name}"
id = "{app_id}"
version = "0.1.0"
entry = "src/main.rs"
sdk_min = 24
sdk_target = 34
sdk_compile = 34

[app]
label = "{project_name}"
orientation = "unspecified"
fullscreen = false
debuggable = true

[abi]
targets = ["arm64-v8a", "x86_64"]

[permissions]
list = ["INTERNET"]

[dependencies.java]
# "androidx.core:core-ktx" = "1.13.1"

[ndk]
# version = "27.0.12077973"
"#
    )
}
