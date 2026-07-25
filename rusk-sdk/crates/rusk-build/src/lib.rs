//! rusk-build: the actual "translator" the SDK is built around — it turns
//! `rusk build` into the concrete sequence of `cargo`, NDK-`clang`,
//! `javac`, `d8`, `aapt2`, `zipalign` and `apksigner` invocations needed
//! to produce an APK, driven entirely by `Rusk.toml`.

use std::path::{Path, PathBuf};
use std::process::Command;

use rusk_manifest::{resolve_abi, AbiTarget, RuskManifest};
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BuildError {
    #[error(transparent)]
    Ndk(#[from] rusk_ndk::NdkError),
    #[error(transparent)]
    SdkMgr(#[from] rusk_sdkmgr::SdkMgrError),
    #[error(transparent)]
    JavaDeps(#[from] rusk_javadeps::JavaDepsError),
    #[error(transparent)]
    Apk(#[from] rusk_apk::ApkError),
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read the project's Cargo.toml at {0}")]
    CargoTomlMissing(PathBuf),
    #[error("`cargo build` failed for target {triple} (exit {status})")]
    CargoBuildFailed { triple: String, status: i32 },
    #[error("built library not found at {0}; check that Cargo.toml declares `crate-type = [\"cdylib\"]`")]
    MissingBuiltLib(PathBuf),
    #[error("`javac` failed compiling the generated activity shim (exit {0})")]
    JavacFailed(i32),
    #[error("rustup could not install target \"{triple}\" (exit {status}); if you're not using rustup, add the target's prebuilt std another way, then re-run rusk build")]
    RustupTargetFailed { triple: String, status: i32 },
    #[error("`rustup` was not found on PATH, and target \"{0}\" is not installed; either install rustup or add this Android target's std some other way")]
    RustupMissing(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Debug,
    Release,
}

impl Profile {
    fn cargo_flag(&self) -> Option<&'static str> {
        match self {
            Profile::Debug => None,
            Profile::Release => Some("--release"),
        }
    }
    fn dir_name(&self) -> &'static str {
        match self {
            Profile::Debug => "debug",
            Profile::Release => "release",
        }
    }
}

pub struct BuildOptions {
    pub project_root: PathBuf,
    pub profile: Profile,
    pub flavor: Option<String>,
}

pub struct BuildOutcome {
    pub apk_path: PathBuf,
}

#[derive(Deserialize)]
struct CargoTomlPackage {
    package: CargoPackageName,
}
#[derive(Deserialize)]
struct CargoPackageName {
    name: String,
}

fn read_crate_name(project_root: &Path) -> Result<String, BuildError> {
    let cargo_toml = project_root.join("Cargo.toml");
    let text = std::fs::read_to_string(&cargo_toml)
        .map_err(|_| BuildError::CargoTomlMissing(cargo_toml.clone()))?;
    let parsed: CargoTomlPackage =
        toml::from_str(&text).map_err(|_| BuildError::CargoTomlMissing(cargo_toml.clone()))?;
    Ok(parsed.package.name)
}

/// Keeps only the last `max_chars` Unicode scalar values of `s`, used to
/// bound `BuildStepRecord::error_tail` — a locate-the-failure index in
/// the download cache log, not a full error transcript.
fn truncate_tail(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        s.to_string()
    } else {
        let skip = char_count - max_chars;
        format!("…{}", s.chars().skip(skip).collect::<String>())
    }
}

pub fn build(manifest: &RuskManifest, opts: &BuildOptions) -> Result<BuildOutcome, BuildError> {
    let build_start = std::time::Instant::now();
    let (manifest, cargo_features) = manifest
        .apply_flavor(opts.flavor.as_deref())
        .map_err(|e| BuildError::Io {
            path: opts.project_root.clone(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()),
        })?;
    let manifest = &manifest;

    rusk_ui::compiler_banner(
        &format!(
            "{}{}",
            manifest.package.name,
            opts.flavor.as_deref().map(|f| format!(" [{f}]")).unwrap_or_default()
        ),
        env!("CARGO_PKG_VERSION"),
    );

    let ndk = rusk_ndk::ensure_ndk(manifest.ndk.version.as_deref())?;
    let crate_name = read_crate_name(&opts.project_root)?;
    let lib_file_stem = crate_name.replace('-', "_");

    let cache_dir = rusk_cache::project_cache_dir(&opts.project_root).ok();
    let mut download_cache = cache_dir
        .as_deref()
        .map(|d| rusk_cache::DownloadCache::load(&rusk_cache::download_cache_path(d)).unwrap_or_default())
        .unwrap_or_default();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    download_cache.record_download(rusk_cache::DownloadRecord {
        kind: "ndk".to_string(),
        label: format!("NDK {}", ndk.version),
        source_url: "https://dl.google.com/android/repository/".to_string(),
        local_path: ndk.root.clone(),
        size_bytes: 0, // directory, not a single file — size isn't meaningful here
        sha256: None,
        downloaded_at: now,
        last_verified_at: now,
        duration_ms: 0,
    });

    let targets = manifest.resolved_targets();
    let mut native_libs: Vec<(String, PathBuf)> = Vec::new();

    let incremental_record_path = rusk_incremental::record_path(&opts.project_root);
    let mut incremental_record = rusk_incremental::IncrementalRecord::load(&incremental_record_path)
        .unwrap_or_default();

    for target in &targets {
        let toolchain_ctx = rusk_incremental::ToolchainContext {
            ndk_version: ndk.version.clone(),
            build_tools_version: manifest.sdk.build_tools_version.clone(),
            rust_triple: target.rust_triple.to_string(),
            release: opts.profile == Profile::Release,
            features: cargo_features.clone(),
        };
        let fingerprint = rusk_incremental::compute(&opts.project_root, &toolchain_ctx).ok();

        if let Some(fp) = &fingerprint {
            if let rusk_incremental::CacheDecision::Hit(cached_path) =
                incremental_record.check(target.rust_triple, fp)
            {
                let step = rusk_ui::Step::start(format!(
                    "Compiling for {} ({}) — unchanged since last build",
                    target.abi, target.rust_triple
                ));
                native_libs.push((target.jni_libs_dir.to_string(), cached_path));
                step.ok();
                continue;
            }
        }

        let step = rusk_ui::Step::start(format!(
            "Compiling for {} ({})",
            target.abi, target.rust_triple
        ));
        let step_started = std::time::Instant::now();
        let step_started_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let result = compile_for_target(
            &opts.project_root,
            &ndk,
            target,
            manifest.package.sdk_min,
            opts.profile,
            &cargo_features,
        );
        let duration_ms = step_started.elapsed().as_millis() as u64;
        download_cache.record_build_step(rusk_cache::BuildStepRecord {
            tool: "cargo-build".to_string(),
            context: target.rust_triple.to_string(),
            started_at: step_started_unix,
            duration_ms,
            success: result.is_ok(),
            error_tail: result.as_ref().err().map(|e| truncate_tail(&e.to_string(), 400)),
        });
        match result {
            Ok(so_path) => {
                if let Some(fp) = fingerprint {
                    let _ = incremental_record.record(target.rust_triple, fp, so_path.clone());
                }
                native_libs.push((target.jni_libs_dir.to_string(), so_path));
                step.ok();
            }
            Err(e) => {
                step.fail(&e.to_string());
                if let Some(dir) = cache_dir.as_deref() {
                    let _ = download_cache.write(&rusk_cache::download_cache_path(dir));
                }
                return Err(e);
            }
        }
    }
    let _ = incremental_record.write(&incremental_record_path);

    let staging = opts.project_root.join("target/rusk-staging");
    std::fs::create_dir_all(&staging).map_err(|source| BuildError::Io {
        path: staging.clone(),
        source,
    })?;

    let (tools, android_jar) = resolve_sdk_tools(manifest)?;

    let jni_src_dir = opts.project_root.join("src");
    let jni_functions = rusk_jnigen::scan_dir(&jni_src_dir, &manifest.package.id);
    for f in &jni_functions {
        for bad_ty in &f.unrecognized_types {
            rusk_ui::warn(format!(
                "JNI binding {}.{}: unrecognized Rust param type \"{bad_ty}\" mapped to Java Object — double-check it by hand",
                f.class_name, f.method_name
            ));
        }
    }
    if !jni_functions.is_empty() {
        rusk_ui::info(format!(
            "found {} JNI export(s) — generating matching Java native declarations",
            jni_functions.len()
        ));
    }

    let has_java_deps = !manifest.dependencies.java.is_empty();
    let has_code = has_java_deps || !jni_functions.is_empty();

    let manifest_xml = rusk_androidgen::generate_manifest(manifest, &lib_file_stem, has_code);
    let strings_xml = rusk_androidgen::generate_strings_xml(manifest);

    let mut java_class_dir: Option<PathBuf> = None;
    let mut java_dep_jars: Vec<PathBuf> = Vec::new();
    let mut java_dep_coordinates: Vec<rusk_javadeps::Coordinate> = Vec::new();
    let mut java_source_files: Vec<PathBuf> = Vec::new();

    let src_dir = staging.join("java-src");
    let pkg_dir = src_dir.join(manifest.package.id.replace('.', "/"));
    if has_code {
        std::fs::create_dir_all(&pkg_dir).map_err(|source| BuildError::Io {
            path: pkg_dir.clone(),
            source,
        })?;
    }

    if has_java_deps {
        let step = rusk_ui::Step::start("Resolving Java dependencies (Maven)");
        let roots: Vec<(String, String)> = manifest
            .java_dependencies()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let jars = match rusk_javadeps::resolve_and_fetch(&roots, &manifest.dependencies.repositories)
        {
            Ok(j) => j,
            Err(e) => {
                step.fail(&e.to_string());
                return Err(e.into());
            }
        };
        step.ok();
        rusk_ui::dependency_table(&rusk_javadeps::ui_rows(&jars));

        let dep_now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        for jar in &jars {
            download_cache.record_download(rusk_cache::DownloadRecord {
                kind: "java-dependency".to_string(),
                label: format!("{}:{}:{}", jar.coordinate.group, jar.coordinate.artifact, jar.coordinate.version),
                source_url: String::new(),
                local_path: jar.jar_path.clone(),
                size_bytes: jar.size_bytes,
                sha256: None,
                downloaded_at: dep_now,
                last_verified_at: dep_now,
                duration_ms: 0,
            });
        }

        // AARs (androidx, Play Services, ...) can carry their own native
        // libraries in jni/<abi>/ — those need to sit in the APK right
        // alongside the .so files rustc just produced.
        for jar in &jars {
            for (abi, so_path) in &jar.native_libs {
                native_libs.push((abi.clone(), so_path.clone()));
            }
        }
        java_dep_coordinates = jars.iter().map(|j| j.coordinate.clone()).collect();
        java_dep_jars = jars.into_iter().map(|j| j.jar_path).collect();

        let (class_name, java_src) = rusk_androidgen::generate_activity_shim_java(manifest);
        let src_file = pkg_dir.join(format!("{class_name}.java"));
        std::fs::write(&src_file, &java_src).map_err(|source| BuildError::Io {
            path: src_file.clone(),
            source,
        })?;
        java_source_files.push(src_file);
    }

    if !jni_functions.is_empty() {
        let mut by_class: std::collections::BTreeMap<String, Vec<rusk_jnigen::JniFunction>> =
            std::collections::BTreeMap::new();
        for f in jni_functions {
            by_class.entry(f.class_name.clone()).or_default().push(f);
        }
        for (class_name, funcs) in &by_class {
            let java_src = rusk_jnigen::generate_bindings_class(
                &manifest.package.id,
                class_name,
                &lib_file_stem,
                funcs,
            );
            let src_file = pkg_dir.join(format!("{class_name}.java"));
            std::fs::write(&src_file, &java_src).map_err(|source| BuildError::Io {
                path: src_file.clone(),
                source,
            })?;
            java_source_files.push(src_file);
        }
    }

    if has_code {
        let step = rusk_ui::Step::start(format!(
            "Compiling {} generated Java source file(s)",
            java_source_files.len()
        ));
        let class_dir = staging.join("java-classes");
        std::fs::create_dir_all(&class_dir).map_err(|source| BuildError::Io {
            path: class_dir.clone(),
            source,
        })?;

        let mut cmd = Command::new("javac");
        cmd.arg("-d").arg(&class_dir);
        cmd.arg("-classpath").arg(&android_jar);
        for src_file in &java_source_files {
            cmd.arg(src_file);
        }
        let status = cmd.status().map_err(|source| BuildError::Io {
            path: PathBuf::from("javac"),
            source,
        })?;
        if !status.success() {
            step.fail("javac exited with an error");
            return Err(BuildError::JavacFailed(status.code().unwrap_or(-1)));
        }
        step.ok();
        java_class_dir = Some(class_dir);
    }

    let keystore = match &manifest.signing {
        Some(sign) => rusk_apk::KeystoreConfig {
            path: opts.project_root.join(&sign.keystore),
            alias: sign.alias.clone(),
            password: std::env::var(&sign.password_env).unwrap_or_default(),
        },
        None => rusk_apk::ensure_debug_keystore()?,
    };

    let unsigned = staging.join("unsigned.apk");
    let out_dir = opts.project_root.join("target/rusk-out");
    std::fs::create_dir_all(&out_dir).map_err(|source| BuildError::Io {
        path: out_dir.clone(),
        source,
    })?;
    let apk_name = match &opts.flavor {
        Some(f) => format!("{}-{f}.apk", manifest.package.name),
        None => format!("{}.apk", manifest.package.name),
    };
    let signed = out_dir.join(apk_name);

    let inputs = rusk_apk::ApkBuildInputs {
        staging_dir: &staging,
        manifest_xml: &manifest_xml,
        strings_xml: &strings_xml,
        native_libs: &native_libs,
        java_class_dir: java_class_dir.as_deref(),
        java_dep_jars: &java_dep_jars,
        sdk_compile: manifest.package.sdk_compile,
        android_jar: &android_jar,
        out_apk_unsigned: &unsigned,
    };
    rusk_apk::build_apk(&tools, inputs, &keystore, &signed)?;

    let build_started_elapsed = build_start.elapsed();
    let apk_size = std::fs::metadata(&signed).map(|m| m.len()).unwrap_or(0);
    let per_abi_sizes: Vec<(String, u64)> = native_libs
        .iter()
        .filter(|(dir, _)| targets.iter().any(|t| t.jni_libs_dir == dir.as_str()))
        .map(|(dir, path)| (dir.clone(), std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)))
        .collect();
    rusk_ui::build_summary(&signed, apk_size, &per_abi_sizes, build_started_elapsed);

    if let Some(dir) = cache_dir.as_deref() {
        let lock = rusk_cache::RuskLock::new(
            ndk.version.clone(),
            manifest.sdk.build_tools_version.clone(),
            manifest.package.sdk_compile,
            java_dep_coordinates
                .iter()
                .map(|c| rusk_cache::LockedJavaDep {
                    group: c.group.clone(),
                    artifact: c.artifact.clone(),
                    version: c.version.clone(),
                    sha256: None,
                })
                .collect(),
        );
        if let Err(e) = rusk_cache::sync_lock(&opts.project_root, dir, &lock, &download_cache) {
            rusk_ui::warn(format!("could not update Rusk.lock/Rusk.downloadcache: {e}"));
        }
    }

    Ok(BuildOutcome { apk_path: signed })
}

/// Picks up build-tools and the platform jar from `ANDROID_HOME` /
/// `ANDROID_SDK_ROOT` when it's set and looks usable; otherwise falls
/// back to `rusk-sdkmgr`, which downloads exactly the pinned
/// `[sdk].build_tools_version` + matching platform + platform-tools into
/// `~/.rusk/android-sdk` on first use. Either way the caller gets back
/// the same [`rusk_apk::ToolPaths`] shape.
fn resolve_sdk_tools(manifest: &RuskManifest) -> Result<(rusk_apk::ToolPaths, PathBuf), BuildError> {
    let has_android_home = std::env::var_os("ANDROID_HOME")
        .or_else(|| std::env::var_os("ANDROID_SDK_ROOT"))
        .is_some();

    if has_android_home {
        if let Ok(jar) = rusk_apk::locate_android_jar(manifest.package.sdk_compile) {
            let ndk_tools = rusk_ndk::locate_build_tools()?;
            return Ok((
                rusk_apk::ToolPaths {
                    aapt2: ndk_tools.aapt2,
                    d8: ndk_tools.d8,
                    zipalign: ndk_tools.zipalign,
                    apksigner: ndk_tools.apksigner,
                },
                jar,
            ));
        }
    }

    rusk_ui::info("ANDROID_HOME not set (or incomplete) — provisioning a managed SDK under ~/.rusk/android-sdk");
    let managed = rusk_sdkmgr::ensure_components(&manifest.sdk.build_tools_version, manifest.package.sdk_compile)?;
    let managed_tools = managed.build_tools();
    Ok((
        rusk_apk::ToolPaths {
            aapt2: managed_tools.aapt2,
            d8: managed_tools.d8,
            zipalign: managed_tools.zipalign,
            apksigner: managed_tools.apksigner,
        },
        managed.platform_jar,
    ))
}

/// The single most common first-run failure this toolchain hits isn't
/// anything Android-specific — it's `error[E0463]: can't find crate for
/// `std`` because the Android target's prebuilt std was never added via
/// rustup. `rustup target add <triple>` is idempotent (a no-op if it's
/// already installed), so it's simplest to just always run it before
/// `cargo build` rather than trying to detect the installed set first.
fn ensure_rust_target_installed(triple: &str) -> Result<(), BuildError> {
    let step = rusk_ui::Step::start(format!("Ensuring rustup target {triple} is installed"));
    let status = Command::new("rustup")
        .arg("target")
        .arg("add")
        .arg(triple)
        .status();
    match status {
        Ok(s) if s.success() => {
            step.ok();
            Ok(())
        }
        Ok(s) => {
            let err = BuildError::RustupTargetFailed {
                triple: triple.to_string(),
                status: s.code().unwrap_or(-1),
            };
            step.fail(&err.to_string());
            Err(err)
        }
        Err(_) => {
            let err = BuildError::RustupMissing(triple.to_string());
            step.fail(&err.to_string());
            Err(err)
        }
    }
}

fn compile_for_target(
    project_root: &Path,
    ndk: &rusk_ndk::NdkHome,
    target: &AbiTarget,
    api_level: u32,
    profile: Profile,
    cargo_features: &[String],
) -> Result<PathBuf, BuildError> {
    ensure_rust_target_installed(target.rust_triple)?;

    let clang = ndk.clang_driver(target.clang_target, api_level);
    let clang_cxx = ndk.clang_cxx_driver(target.clang_target, api_level);
    let ar = ndk.toolchain_bin().join(if cfg!(windows) { "llvm-ar.exe" } else { "llvm-ar" });

    let mut cmd = Command::new("cargo");
    cmd.current_dir(project_root);
    cmd.arg("build").arg("--target").arg(target.rust_triple).arg("--lib");
    if let Some(flag) = profile.cargo_flag() {
        cmd.arg(flag);
    }
    if !cargo_features.is_empty() {
        cmd.arg("--features").arg(cargo_features.join(","));
    }

    let triple_env = target.rust_triple.to_uppercase().replace('-', "_");
    let triple_underscored = target.rust_triple.replace('-', "_");
    cmd.env(format!("CARGO_TARGET_{triple_env}_LINKER"), &clang);
    // The `cc` crate (used by many native-dep build scripts) looks up
    // `CC_<target>` with the triple's hyphens kept as-is; cargo's own
    // linker override uses the underscored+uppercased form above. Both
    // are set so either lookup convention finds the right compiler.
    cmd.env(format!("CC_{}", target.rust_triple), &clang);
    cmd.env(format!("CXX_{}", target.rust_triple), &clang_cxx);
    cmd.env(format!("AR_{}", target.rust_triple), &ar);
    cmd.env(format!("CC_{triple_underscored}"), &clang);
    cmd.env(format!("CXX_{triple_underscored}"), &clang_cxx);
    cmd.env(format!("AR_{triple_underscored}"), &ar);
    cmd.env("ANDROID_NDK_HOME", &ndk.root);

    // clang.exe (used directly on Windows to avoid the .cmd wrapper's
    // batch re-parsing of --version-script= arguments — see
    // NdkHome::clang_driver) doesn't infer its target triple from its
    // filename the way the version-suffixed .cmd/shell-script drivers
    // do, so it has to be passed explicitly as a link arg here. On Unix
    // the driver binary is still the version-suffixed shell script, so
    // this isn't needed there.
    let mut rustflags = String::from("-C link-arg=-landroid -C link-arg=-llog");
    if cfg!(windows) {
        rustflags = format!(
            "-C link-arg=--target={}{api_level} {rustflags}",
            target.clang_target
        );
    }
    cmd.env(format!("CARGO_TARGET_{triple_env}_RUSTFLAGS"), rustflags);

    let status = cmd.status().map_err(|source| BuildError::Io {
        path: PathBuf::from("cargo"),
        source,
    })?;
    if !status.success() {
        return Err(BuildError::CargoBuildFailed {
            triple: target.rust_triple.to_string(),
            status: status.code().unwrap_or(-1),
        });
    }

    let crate_name = read_crate_name(project_root)?.replace('-', "_");
    let so_path = project_root
        .join("target")
        .join(target.rust_triple)
        .join(profile.dir_name())
        .join(format!("lib{crate_name}.so"));
    if !so_path.is_file() {
        return Err(BuildError::MissingBuiltLib(so_path));
    }
    Ok(so_path)
}

/// Convenience used by `rusk doctor` to sanity-check that a declared ABI
/// string is one Rusk actually knows how to cross-compile for.
pub fn check_abi(name: &str) -> bool {
    resolve_abi(name).is_some()
}
