//! rusk-sdkmgr: the piece that used to be missing — before this crate,
//! `rusk build` *required* `ANDROID_HOME` to already point at a
//! hand-installed SDK. This crate makes the "everything just gets
//! pulled automatically" promise actually true for the Android SDK side,
//! not just for the NDK: it reads Google's own package repository
//! manifest (the same XML `sdkmanager` reads) and downloads exactly the
//! components a build needs — `platform-tools`, one `build-tools;<ver>`,
//! one `platforms;android-<api>` — into `~/.rusk/android-sdk`, with no
//! separate installer binary and no manual `sdkmanager --licenses` step.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use quick_xml::events::Event;
use quick_xml::reader::Reader;
use rusk_ui::DownloadProgress;
use sha1::{Digest, Sha1};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SdkMgrError {
    #[error("network error fetching {url}: {source}")]
    Network {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("package \"{0}\" is not listed in Google's repository manifest for this host OS")]
    PackageNotFound(String),
    #[error("checksum mismatch downloading \"{package}\": expected {expected}, got {actual}")]
    ChecksumMismatch {
        package: String,
        expected: String,
        actual: String,
    },
    #[error("could not determine a home/cache directory for this user")]
    NoCacheDir,
}

const REPOSITORY_MANIFEST_URL: &str = "https://dl.google.com/android/repository/repository2-3.xml";
const ARCHIVE_BASE_URL: &str = "https://dl.google.com/android/repository/";

/// One `<archive>` entry parsed out of the repository manifest for a
/// single package path.
struct ArchiveInfo {
    host_os: Option<String>,
    url: String,
    sha1: String,
}

struct PackageInfo {
    archives: Vec<ArchiveInfo>,
}

fn host_os_tag() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macosx"
    } else {
        "linux"
    }
}

fn sdk_root() -> Result<PathBuf, SdkMgrError> {
    let home = dirs::home_dir().ok_or(SdkMgrError::NoCacheDir)?;
    Ok(home.join(".rusk").join("android-sdk"))
}

fn fetch_repository_xml(client: &reqwest::blocking::Client) -> Result<String, SdkMgrError> {
    let resp = client
        .get(REPOSITORY_MANIFEST_URL)
        .send()
        .map_err(|source| SdkMgrError::Network {
            url: REPOSITORY_MANIFEST_URL.to_string(),
            source,
        })?;
    resp.text().map_err(|source| SdkMgrError::Network {
        url: REPOSITORY_MANIFEST_URL.to_string(),
        source,
    })
}

/// Streams the (fairly large) repository manifest looking only for
/// `<remotePackage path="...">` blocks whose `path` is in `wanted`,
/// collecting each one's `<archive>` entries. This avoids building a full
/// DOM for a manifest that lists hundreds of unrelated packages.
fn parse_packages(xml: &str, wanted: &[&str]) -> std::collections::HashMap<String, PackageInfo> {
    let mut reader = Reader::from_str(xml);
    reader.trim_text(true);

    let mut out: std::collections::HashMap<String, PackageInfo> = std::collections::HashMap::new();

    let mut current_path: Option<String> = None;
    let mut capturing = false;
    let mut in_archive = false;
    let mut archives: Vec<ArchiveInfo> = Vec::new();
    let mut cur_host_os: Option<String> = None;
    let mut cur_url = String::new();
    let mut cur_sha1 = String::new();
    let mut cur_checksum_type = String::new();
    let mut tag_stack: Vec<String> = Vec::new();

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if tag == "remotePackage" {
                    let path_attr = e
                        .attributes()
                        .flatten()
                        .find(|a| a.key.as_ref() == b"path")
                        .map(|a| String::from_utf8_lossy(&a.value).into_owned());
                    if let Some(p) = &path_attr {
                        if wanted.contains(&p.as_str()) {
                            capturing = true;
                            current_path = path_attr;
                            archives.clear();
                        } else {
                            capturing = false;
                            current_path = None;
                        }
                    }
                } else if capturing && tag == "archive" {
                    in_archive = true;
                    cur_host_os = None;
                    cur_url.clear();
                    cur_sha1.clear();
                } else if capturing && in_archive && tag == "checksum" {
                    cur_checksum_type = e
                        .attributes()
                        .flatten()
                        .find(|a| a.key.as_ref() == b"type")
                        .map(|a| String::from_utf8_lossy(&a.value).into_owned())
                        .unwrap_or_default();
                }
                tag_stack.push(tag);
            }
            Ok(Event::End(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                tag_stack.pop();
                if tag == "remotePackage" {
                    if let Some(path) = current_path.take() {
                        out.insert(
                            path,
                            PackageInfo {
                                archives: std::mem::take(&mut archives),
                            },
                        );
                    }
                    capturing = false;
                } else if tag == "archive" && capturing {
                    archives.push(ArchiveInfo {
                        host_os: cur_host_os.clone(),
                        url: cur_url.clone(),
                        sha1: cur_sha1.clone(),
                    });
                    in_archive = false;
                }
            }
            Ok(Event::Text(t)) => {
                if capturing && in_archive {
                    if let Some(parent) = tag_stack.last().map(|s| s.as_str()) {
                        let text = t.unescape().unwrap_or_default().into_owned();
                        match parent {
                            "host-os" => cur_host_os = Some(text),
                            "url" => cur_url = text,
                            "checksum" if cur_checksum_type.eq_ignore_ascii_case("sha1") => {
                                cur_sha1 = text
                            }
                            _ => {}
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

fn pick_archive<'a>(pkg: &'a PackageInfo, package_name: &str) -> Result<&'a ArchiveInfo, SdkMgrError> {
    let host = host_os_tag();
    pkg.archives
        .iter()
        .find(|a| a.host_os.as_deref() == Some(host))
        .or_else(|| pkg.archives.iter().find(|a| a.host_os.is_none()))
        .ok_or_else(|| SdkMgrError::PackageNotFound(package_name.to_string()))
}

fn download_and_verify(
    client: &reqwest::blocking::Client,
    archive: &ArchiveInfo,
    label: &str,
    dest_zip: &Path,
) -> Result<(), SdkMgrError> {
    let url = format!("{ARCHIVE_BASE_URL}{}", archive.url);
    let mut resp = client.get(&url).send().map_err(|source| SdkMgrError::Network {
        url: url.clone(),
        source,
    })?;
    let total = resp.content_length().unwrap_or(0);
    let mut out = File::create(dest_zip).map_err(|source| SdkMgrError::Io {
        path: dest_zip.to_path_buf(),
        source,
    })?;
    let mut progress = DownloadProgress::new(label.to_string(), total);
    let mut hasher = Sha1::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = resp.read(&mut buf).map_err(|source| SdkMgrError::Io {
            path: dest_zip.to_path_buf(),
            source,
        })?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).map_err(|source| SdkMgrError::Io {
            path: dest_zip.to_path_buf(),
            source,
        })?;
        hasher.update(&buf[..n]);
        progress.add(n as u64);
        progress.tick_frame();
    }
    progress.finish();

    if !archive.sha1.is_empty() {
        let actual = format!("{:x}", hasher.finalize());
        if !actual.eq_ignore_ascii_case(&archive.sha1) {
            return Err(SdkMgrError::ChecksumMismatch {
                package: label.to_string(),
                expected: archive.sha1.clone(),
                actual,
            });
        }
    }
    Ok(())
}

/// Unpacks a package zip, stripping its single top-level directory (every
/// SDK component zip has exactly one, e.g. `build-tools/` or
/// `android-13/`) and placing the contents directly at `dest`.
fn unpack_stripping_root(zip_path: &Path, dest: &Path) -> Result<(), SdkMgrError> {
    let file = File::open(zip_path).map_err(|source| SdkMgrError::Io {
        path: zip_path.to_path_buf(),
        source,
    })?;
    let mut zip = zip::ZipArchive::new(file)?;
    let mut progress = rusk_ui::ExtractProgress::new("Unpacking", zip.len());
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let name = entry.name().to_string();
        progress.advance(&name);
        let relative = match name.split_once('/') {
            Some((_, rest)) if !rest.is_empty() => rest,
            _ => continue,
        };
        let out_path = dest.join(relative);
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path).map_err(|source| SdkMgrError::Io {
                path: out_path.clone(),
                source,
            })?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| SdkMgrError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let mut out_file = File::create(&out_path).map_err(|source| SdkMgrError::Io {
            path: out_path.clone(),
            source,
        })?;
        std::io::copy(&mut entry, &mut out_file).map_err(|source| SdkMgrError::Io {
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

pub struct ManagedSdk {
    pub root: PathBuf,
    pub build_tools: PathBuf,
    pub platform_jar: PathBuf,
    pub platform_tools_dir: PathBuf,
}

/// Ensures `platform-tools`, `build-tools;<build_tools_version>` and
/// `platforms;android-<api>` are present under `~/.rusk/android-sdk`,
/// downloading whichever of the three is missing. Each component is
/// marked complete with a `.rusk-complete` sentinel file so a second call
/// is a set of cheap existence checks, not re-downloads.
pub fn ensure_components(build_tools_version: &str, api: u32) -> Result<ManagedSdk, SdkMgrError> {
    let root = sdk_root()?;
    std::fs::create_dir_all(&root).map_err(|source| SdkMgrError::Io {
        path: root.clone(),
        source,
    })?;

    let build_tools_path = format!("build-tools;{build_tools_version}");
    let platform_path = format!("platforms;android-{api}");
    let platform_tools_path = "platform-tools";

    let build_tools_dir = root.join("build-tools").join(build_tools_version);
    let platform_dir = root.join("platforms").join(format!("android-{api}"));
    let platform_tools_dir = root.join("platform-tools");

    let need_build_tools = !is_component_complete(&build_tools_dir);
    let need_platform = !is_component_complete(&platform_dir);
    let need_platform_tools = !is_component_complete(&platform_tools_dir);

    if need_build_tools || need_platform || need_platform_tools {
        let client = http_client()?;
        let step = rusk_ui::Step::start("Fetching Android SDK package manifest");
        let mut wanted = Vec::new();
        if need_build_tools {
            wanted.push(build_tools_path.as_str());
        }
        if need_platform {
            wanted.push(platform_path.as_str());
        }
        if need_platform_tools {
            wanted.push(platform_tools_path);
        }
        let xml = match fetch_repository_xml(&client) {
            Ok(x) => x,
            Err(e) => {
                step.fail(&e.to_string());
                return Err(e);
            }
        };
        let packages = parse_packages(&xml, &wanted);
        step.ok();

        if need_platform_tools {
            install_component(&client, &packages, platform_tools_path, "platform-tools", &platform_tools_dir)?;
        }
        if need_build_tools {
            install_component(
                &client,
                &packages,
                &build_tools_path,
                &format!("build-tools {build_tools_version}"),
                &build_tools_dir,
            )?;
        }
        if need_platform {
            install_component(
                &client,
                &packages,
                &platform_path,
                &format!("platform android-{api}"),
                &platform_dir,
            )?;
        }
    }

    Ok(ManagedSdk {
        root: root.clone(),
        build_tools: build_tools_dir,
        platform_jar: platform_dir.join("android.jar"),
        platform_tools_dir,
    })
}

fn http_client() -> Result<reqwest::blocking::Client, SdkMgrError> {
    reqwest::blocking::Client::builder()
        .user_agent("rusk-sdk/0.2")
        .build()
        .map_err(|source| SdkMgrError::Network {
            url: REPOSITORY_MANIFEST_URL.to_string(),
            source,
        })
}

fn is_component_complete(dir: &Path) -> bool {
    dir.join(".rusk-complete").is_file()
}

/// Everything beyond compilers and build-tools that "interacting with
/// Android" actually needs: a command-line SDK manager tree (so any
/// `sdkmanager`-shaped workflow a project's own tooling expects also has
/// somewhere real to live), the emulator binary, and one system image —
/// enough to run `rusk emulator create` / `rusk emulator start` without
/// ever opening Android Studio.
pub struct EmulatorComponents {
    pub cmdline_tools_dir: PathBuf,
    pub emulator_dir: PathBuf,
    pub emulator_bin: PathBuf,
    pub system_image_dir: PathBuf,
    pub avdmanager: PathBuf,
}

/// The system-image package path Google publishes for a given API level
/// running on the host's own CPU architecture (so the emulator can use
/// hardware acceleration instead of falling back to slow full-system
/// emulation). `google_apis` images are used over the bare AOSP ones
/// since almost every real app depends on Play Services being present.
fn system_image_path(api: u32) -> String {
    let abi = if cfg!(target_arch = "aarch64") {
        "arm64-v8a"
    } else {
        "x86_64"
    };
    format!("system-images;android-{api};google_apis;{abi}")
}

/// Ensures `cmdline-tools;latest`, `emulator`, and a `google_apis` system
/// image for `api` are present under `~/.rusk/android-sdk`, so `rusk
/// emulator` can create and boot an AVD with zero manual SDK-manager
/// steps — mirroring what `ensure_components` already does for the
/// compiler-facing build-tools/platform-tools.
pub fn ensure_emulator_components(api: u32) -> Result<EmulatorComponents, SdkMgrError> {
    let root = sdk_root()?;
    std::fs::create_dir_all(&root).map_err(|source| SdkMgrError::Io {
        path: root.clone(),
        source,
    })?;

    let cmdline_tools_path = "cmdline-tools;latest";
    let emulator_path = "emulator";
    let sysimg_path = system_image_path(api);

    let cmdline_tools_dir = root.join("cmdline-tools").join("latest");
    let emulator_dir = root.join("emulator");
    let system_image_dir = root
        .join("system-images")
        .join(format!("android-{api}"))
        .join("google_apis")
        .join(if cfg!(target_arch = "aarch64") { "arm64-v8a" } else { "x86_64" });

    let need_cmdline_tools = !is_component_complete(&cmdline_tools_dir);
    let need_emulator = !is_component_complete(&emulator_dir);
    let need_sysimg = !is_component_complete(&system_image_dir);

    if need_cmdline_tools || need_emulator || need_sysimg {
        let client = http_client()?;
        let step = rusk_ui::Step::start("Fetching Android SDK package manifest (emulator components)");
        let mut wanted = Vec::new();
        if need_cmdline_tools {
            wanted.push(cmdline_tools_path);
        }
        if need_emulator {
            wanted.push(emulator_path);
        }
        if need_sysimg {
            wanted.push(sysimg_path.as_str());
        }
        let xml = match fetch_repository_xml(&client) {
            Ok(x) => x,
            Err(e) => {
                step.fail(&e.to_string());
                return Err(e);
            }
        };
        let packages = parse_packages(&xml, &wanted);
        step.ok();

        if need_cmdline_tools {
            install_component(&client, &packages, cmdline_tools_path, "cmdline-tools", &cmdline_tools_dir)?;
        }
        if need_emulator {
            install_component(&client, &packages, emulator_path, "emulator", &emulator_dir)?;
        }
        if need_sysimg {
            install_component(
                &client,
                &packages,
                &sysimg_path,
                &format!("system-image android-{api} (google_apis)"),
                &system_image_dir,
            )?;
        }
    }

    let exe = if cfg!(windows) { ".exe" } else { "" };
    let bat = if cfg!(windows) { ".bat" } else { "" };
    Ok(EmulatorComponents {
        cmdline_tools_dir: cmdline_tools_dir.clone(),
        emulator_bin: emulator_dir.join(format!("emulator{exe}")),
        emulator_dir,
        system_image_dir,
        avdmanager: cmdline_tools_dir.join("bin").join(format!("avdmanager{bat}")),
    })
}

fn install_component(
    client: &reqwest::blocking::Client,
    packages: &std::collections::HashMap<String, PackageInfo>,
    package_path: &str,
    label: &str,
    dest_dir: &Path,
) -> Result<(), SdkMgrError> {
    let pkg = packages
        .get(package_path)
        .ok_or_else(|| SdkMgrError::PackageNotFound(package_path.to_string()))?;
    let archive = pick_archive(pkg, package_path)?;

    std::fs::create_dir_all(dest_dir).map_err(|source| SdkMgrError::Io {
        path: dest_dir.to_path_buf(),
        source,
    })?;
    let zip_path = dest_dir.join("download.zip");
    download_and_verify(client, archive, label, &zip_path)?;

    let step = rusk_ui::Step::start_nested(format!("Unpacking {label}"));
    if let Err(e) = unpack_stripping_root(&zip_path, dest_dir) {
        step.fail(&e.to_string());
        return Err(e);
    }
    step.ok();

    let _ = std::fs::remove_file(&zip_path);
    let marker = dest_dir.join(".rusk-complete");
    File::create(&marker)
        .and_then(|mut f| f.write_all(b"ok"))
        .map_err(|source| SdkMgrError::Io {
            path: marker.clone(),
            source,
        })?;
    Ok(())
}

/// Build-tools executable paths inside a [`ManagedSdk`], mirroring
/// `rusk_ndk::BuildTools` so `rusk-apk` doesn't need to care whether the
/// SDK came from `ANDROID_HOME` or from this crate's managed install.
pub struct BuildTools {
    pub aapt2: PathBuf,
    pub d8: PathBuf,
    pub zipalign: PathBuf,
    pub apksigner: PathBuf,
}

impl ManagedSdk {
    pub fn build_tools(&self) -> BuildTools {
        let exe = if cfg!(windows) { ".exe" } else { "" };
        let bat = if cfg!(windows) { ".bat" } else { "" };
        BuildTools {
            aapt2: self.build_tools.join(format!("aapt2{exe}")),
            d8: self.build_tools.join(format!("d8{bat}")),
            zipalign: self.build_tools.join(format!("zipalign{exe}")),
            apksigner: self.build_tools.join(format!("apksigner{bat}")),
        }
    }
}

/// Lists the AVDs already created under `~/.rusk/android-sdk/avd` by
/// reading the `.ini` files `avdmanager` writes there — no need to shell
/// out just to enumerate what already exists.
pub fn list_avds() -> Result<Vec<String>, SdkMgrError> {
    let root = sdk_root()?;
    let avd_home = root.join("avd");
    if !avd_home.is_dir() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&avd_home).map_err(|source| SdkMgrError::Io {
        path: avd_home.clone(),
        source,
    })? {
        let entry = entry.map_err(|source| SdkMgrError::Io {
            path: avd_home.clone(),
            source,
        })?;
        if let Some(name) = entry.file_name().to_str() {
            if let Some(stripped) = name.strip_suffix(".ini") {
                names.push(stripped.to_string());
            }
        }
    }
    names.sort();
    Ok(names)
}

/// Creates a new AVD named `name` from the fetched system image, using
/// `avdmanager` non-interactively (`echo no |` equivalent — declines the
/// "create a custom hardware profile?" prompt so this never blocks
/// waiting for stdin the caller isn't providing).
pub fn create_avd(components: &EmulatorComponents, name: &str, api: u32) -> Result<(), SdkMgrError> {
    let avd_home = sdk_root()?.join("avd");
    std::fs::create_dir_all(&avd_home).map_err(|source| SdkMgrError::Io {
        path: avd_home.clone(),
        source,
    })?;

    let package = system_image_path(api);
    let mut child = std::process::Command::new(&components.avdmanager)
        .arg("create")
        .arg("avd")
        .arg("--name")
        .arg(name)
        .arg("--package")
        .arg(&package)
        .arg("--force")
        .env("ANDROID_AVD_HOME", &avd_home)
        .env("ANDROID_SDK_ROOT", sdk_root()?)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|source| SdkMgrError::Io {
            path: components.avdmanager.clone(),
            source,
        })?;
    // avdmanager asks "Do you wish to create a custom hardware profile
    // [no]" on stdin; answering "no\n" accepts the system image's
    // defaults instead of hanging the process waiting for a TTY.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"no\n");
    }
    let status = child.wait().map_err(|source| SdkMgrError::Io {
        path: components.avdmanager.clone(),
        source,
    })?;
    if !status.success() {
        return Err(SdkMgrError::PackageNotFound(format!(
            "avdmanager exited with status {:?} creating AVD \"{name}\"",
            status.code()
        )));
    }
    Ok(())
}

/// Launches `emulator -avd <name>` detached (non-blocking) so `rusk
/// emulator start` returns as soon as the process is spawned rather than
/// waiting for the emulator window to close.
pub fn launch_avd(components: &EmulatorComponents, name: &str) -> Result<std::process::Child, SdkMgrError> {
    std::process::Command::new(&components.emulator_bin)
        .arg("-avd")
        .arg(name)
        .arg("-netdelay")
        .arg("none")
        .arg("-netspeed")
        .arg("full")
        .env("ANDROID_AVD_HOME", sdk_root()?.join("avd"))
        .env("ANDROID_SDK_ROOT", sdk_root()?)
        .spawn()
        .map_err(|source| SdkMgrError::Io {
            path: components.emulator_bin.clone(),
            source,
        })
}