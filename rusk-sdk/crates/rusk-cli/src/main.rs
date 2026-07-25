use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use rusk_build::{BuildOptions, Profile};
use rusk_manifest::{render_new_manifest, RuskManifest};

#[derive(Parser)]
#[command(name = "rusk", version, about = "Build Android APKs from pure Rust projects")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Scaffold a new Rusk project in a fresh directory.
    New {
        name: String,
        /// Reverse-DNS application id. Defaults to com.rusk.<name>.
        #[arg(long)]
        id: Option<String>,
        /// Skip generating a bindgen-against-real-NDK-headers build.rs
        /// (requires libclang on the host). Off by default because the
        /// point is real headers, not a hand-copy of them.
        #[arg(long)]
        no_bindgen: bool,
        /// Scaffold with `rusk-render`'s AppShell render loop wired up,
        /// so the app draws an animated demo frame instead of a black
        /// screen on first run — the default `onCreate` stub is
        /// intentionally minimal and does not draw anything on its own.
        #[arg(long)]
        with_render: bool,
    },
    /// Cross-compile and package the current project into an APK.
    Build {
        #[arg(long)]
        release: bool,
        /// Name of a [[flavor]] declared in Rusk.toml to apply.
        #[arg(long)]
        flavor: Option<String>,
    },
    /// Build, install over adb, and launch on a connected device/emulator.
    Run {
        #[arg(long)]
        release: bool,
        #[arg(long)]
        flavor: Option<String>,
        /// adb device serial to target when more than one device/emulator
        /// is attached (see `rusk devices` for the list of serials).
        #[arg(long)]
        device: Option<String>,
    },
    /// Check the host toolchain (cargo, rustup targets, NDK, SDK tools, adb).
    Doctor,
    /// Manage local Android emulators (AVDs) — no Android Studio needed.
    Emulator {
        #[command(subcommand)]
        action: EmulatorAction,
    },
    /// List devices/emulators currently visible to adb.
    Devices,
    /// Stream this project's log output from a connected device/emulator
    /// (equivalent to `adb logcat`, pre-filtered to the app's process).
    Logcat {
        /// adb device serial to target when more than one is attached.
        #[arg(long)]
        device: Option<String>,
        /// Show the full unfiltered logcat stream instead of filtering
        /// to the current project's package id.
        #[arg(long)]
        all: bool,
    },
    /// Uninstall this project's package from a connected device/emulator.
    Uninstall {
        #[arg(long)]
        device: Option<String>,
    },
    /// Remove build artifacts (target/rusk-staging, target/rusk-out).
    Clean,
    /// Inspect or manage the local build/download cache
    /// (~/.rusk/cache/<project> — Rusk.lock mirror, Rusk.downloadcache,
    /// and the compressed rusk.lock.cache.xz snapshot).
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
    /// Run static checks over Rusk.toml (permission review, SDK version
    /// sanity, release-signing/debuggable checks, Play Store blockers).
    Lint {
        /// Exit with a non-zero status if any Error-severity finding is
        /// reported — useful in CI to fail the build on real problems
        /// while still printing Warning/Info findings.
        #[arg(long)]
        strict: bool,
    },
    /// Manage the release signing keystore.
    Keystore {
        #[command(subcommand)]
        action: KeystoreAction,
    },
    /// Build an Android App Bundle (.aab) via bundletool, for Play Store
    /// upload — an APK from `rusk build` is for direct install/sideload.
    Bundle {
        #[arg(long)]
        release: bool,
        #[arg(long)]
        flavor: Option<String>,
    },
}

#[derive(Subcommand)]
enum KeystoreAction {
    /// Interactively generate a new release keystore.
    Generate {
        /// Output path for the keystore file.
        #[arg(long, default_value = "release.keystore")]
        path: PathBuf,
        #[arg(long, default_value = "release")]
        alias: String,
        #[arg(long)]
        force: bool,
    },
    /// List the aliases in an existing keystore.
    List {
        #[arg(long, default_value = "release.keystore")]
        path: PathBuf,
    },
    /// Verify the signature on a built APK.
    Verify {
        /// Path to the APK. Defaults to the most recent `rusk build`
        /// output for the current project.
        apk: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum CacheAction {
    /// Summarize what's cached for the current project: total downloaded
    /// bytes, per-tool build step success/failure counts.
    Stats,
    /// Print the full Rusk.downloadcache log (every recorded download and
    /// build step) as-is.
    Inspect,
    /// Decompress and print a rusk.lock.cache.xz snapshot.
    InspectSnapshot,
    /// Delete the current project's local cache directory entirely
    /// (Rusk.lock in the project root is untouched — this only clears
    /// the local, non-committed cache/log data).
    Clear,
}

#[derive(Subcommand)]
enum EmulatorAction {
    /// List AVDs already created under ~/.rusk/android-sdk/avd.
    List,
    /// Download the emulator + a system image (if needed) and create a
    /// new AVD for the current project's `package.sdk_target`.
    Create {
        /// AVD name. Defaults to "rusk-<sdk_target>".
        name: Option<String>,
    },
    /// Start an AVD by name (downloading emulator components first if
    /// this is the first time `rusk emulator` has run).
    Start { name: String },
}

fn main() {
    rusk_ui::init_console();
    if let Err(e) = run() {
        rusk_ui::error(format!("{e:#}"));
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::New { name, id, no_bindgen, with_render } => cmd_new(&name, id, !no_bindgen, with_render),
        Commands::Build { release, flavor } => cmd_build(release, flavor).map(|_| ()),
        Commands::Run { release, flavor, device } => cmd_run(release, flavor, device),
        Commands::Doctor => cmd_doctor(),
        Commands::Emulator { action } => cmd_emulator(action),
        Commands::Devices => cmd_devices(),
        Commands::Logcat { device, all } => cmd_logcat(device, all),
        Commands::Uninstall { device } => cmd_uninstall(device),
        Commands::Clean => cmd_clean(),
        Commands::Cache { action } => cmd_cache(action),
        Commands::Lint { strict } => cmd_lint(strict),
        Commands::Keystore { action } => cmd_keystore(action),
        Commands::Bundle { release, flavor } => cmd_bundle(release, flavor),
    }
}

/// Finds the Rusk SDK's own source checkout relative to the currently
/// running `rusk` binary, since `rusk-render`/`rusk-ndk-sys` are
/// workspace-internal crates (not published to crates.io) — a generated
/// project's `Cargo.toml` needs a real filesystem path to them, and the
/// only thing Rusk can know for certain at `rusk new` time is where its
/// own binary lives.
///
/// This walks up from the binary's location looking for the SDK's own
/// `Cargo.toml` (identified by containing a `[workspace]` with a
/// `rusk-render` member) — covering both `target/release/rusk` (a
/// normal `cargo build --release` inside the SDK checkout) and an
/// installed binary one or two directories deeper.
fn locate_render_crates() -> Result<rusk_androidgen::RenderCratePathsForCargoToml> {
    let exe = std::env::current_exe().context("could not determine the running rusk binary's path")?;
    let mut dir = exe.parent().map(|p| p.to_path_buf());

    for _ in 0..6 {
        let Some(candidate_root) = dir else { break };
        let candidate_manifest = candidate_root.join("Cargo.toml");
        if candidate_manifest.is_file() {
            if let Ok(text) = std::fs::read_to_string(&candidate_manifest) {
                if text.contains("rusk-render") && text.contains("[workspace]") {
                    let render = candidate_root.join("crates/rusk-render");
                    let ndk_sys = candidate_root.join("crates/rusk-ndk-sys");
                    if render.is_dir() && ndk_sys.is_dir() {
                        return Ok(rusk_androidgen::RenderCratePathsForCargoToml { render, ndk_sys });
                    }
                }
            }
        }
        dir = candidate_root.parent().map(|p| p.to_path_buf());
    }

    bail!(
        "couldn't locate the rusk-render/rusk-ndk-sys crates relative to {} — \
         `--with-render` needs to run a `rusk` binary built inside (or still \
         adjacent to) the Rusk SDK source checkout, since those crates aren't \
         published to crates.io yet",
        exe.display()
    )
}

fn cmd_new(name: &str, id: Option<String>, with_bindgen: bool, with_render: bool) -> Result<()> {
    let app_id = id.unwrap_or_else(|| format!("com.rusk.{}", sanitize_ident(name)));
    let dir = PathBuf::from(name);
    if dir.exists() {
        bail!("directory \"{name}\" already exists");
    }
    std::fs::create_dir_all(dir.join("src"))?;

    let render_crate_paths = if with_render { Some(locate_render_crates()?) } else { None };

    let cargo_toml_text = match &render_crate_paths {
        Some(paths) => rusk_androidgen::generate_cargo_toml_with_render(name, with_bindgen, paths),
        None => rusk_androidgen::generate_cargo_toml(name, with_bindgen),
    };
    std::fs::write(dir.join("Cargo.toml"), cargo_toml_text)?;
    let rusk_toml_text = render_new_manifest(name, &app_id);
    std::fs::write(dir.join("Rusk.toml"), &rusk_toml_text)?;

    let manifest: RuskManifest = toml::from_str(&rusk_toml_text)
        .context("internal error: rusk new generated an unparsable Rusk.toml")?;

    // `src/main.rs` is what a plain `cargo run` on the host uses; Android
    // links against the `cdylib` target in `src/lib.rs` via NativeActivity
    // instead, so the two entry points stay independent.
    std::fs::write(
        dir.join("src/main.rs"),
        "fn main() {\n    println!(\"Run `rusk build` to produce an Android APK.\");\n}\n",
    )?;
    let lib_rs_text = if with_render {
        rusk_androidgen::generate_lib_rs_with_render(&manifest)
    } else {
        rusk_androidgen::generate_lib_rs(&manifest, with_bindgen)
    };
    std::fs::write(dir.join("src/lib.rs"), lib_rs_text)?;
    if with_bindgen {
        std::fs::write(dir.join("build.rs"), rusk_androidgen::generate_build_rs())?;
    }

    // Rusk.lock is deliberately NOT ignored — it's the reproducibility
    // record and belongs in version control, same as Cargo.lock.
    // Rusk.downloadcache lives under ~/.rusk/cache/ (not the project
    // directory) so there's normally nothing of its to ignore here, but
    // the entry is included defensively in case a future version ever
    // writes a project-local copy.
    std::fs::write(
        dir.join(".gitignore"),
        "/target\nRusk.downloadcache\n*.tmp-write\n",
    )?;

    rusk_ui::info(format!("created {name}/"));
    rusk_ui::info(format!("  {name}/Rusk.toml   — project + Android descriptor"));
    rusk_ui::info(format!("  {name}/Cargo.toml  — normal cargo manifest"));
    rusk_ui::info(format!("  {name}/src/lib.rs  — android_main entry point"));
    if with_bindgen {
        rusk_ui::info(format!(
            "  {name}/build.rs   — binds the real NDK headers with bindgen (needs libclang; `rusk new --no-bindgen` to skip)"
        ));
    }
    if with_render {
        rusk_ui::info("  wired up with rusk-render — `rusk build && rusk run` should show an animated square, not a black screen");
    } else {
        rusk_ui::info("  note: the default onCreate is intentionally empty (no render loop) — `rusk new --with-render` scaffolds one if you want to see something on screen immediately");
    }
    rusk_ui::info("next: cd into the project and run `rusk build`");
    Ok(())
}

fn cmd_build(release: bool, flavor: Option<String>) -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let (manifest, root) = RuskManifest::discover(&cwd).context("loading Rusk.toml")?;
    let opts = BuildOptions {
        project_root: root,
        profile: if release { Profile::Release } else { Profile::Debug },
        flavor,
    };
    let outcome = rusk_build::build(&manifest, &opts)?;
    Ok(outcome.apk_path)
}

/// Builds an `adb` `Command` with `-s <device>` inserted first when a
/// specific device serial was requested — every adb subcommand
/// (`install`, `shell`, `logcat`, `uninstall`) accepts `-s` in the same
/// position, so this is shared across all of them instead of repeating
/// the same `if let Some(d) = device` branch four times.
fn adb_cmd(adb: &std::path::Path, device: &Option<String>) -> Command {
    let mut cmd = Command::new(adb);
    if let Some(serial) = device {
        cmd.arg("-s").arg(serial);
    }
    cmd
}

fn cmd_run(release: bool, flavor: Option<String>, device: Option<String>) -> Result<()> {
    let apk = cmd_build(release, flavor)?;
    let cwd = std::env::current_dir()?;
    let (manifest, _) = RuskManifest::discover(&cwd)?;

    let adb = locate_adb().context(
        "adb not found; it should have been auto-provisioned under ~/.rusk/android-sdk/platform-tools — try `rusk build` once first, or install Android platform-tools and add it to PATH",
    )?;

    if device.is_none() {
        warn_if_multiple_devices(&adb);
    }

    let step = rusk_ui::Step::start("Installing APK (adb install -r)");
    let status = adb_cmd(&adb, &device).arg("install").arg("-r").arg(&apk).status()?;
    if !status.success() {
        step.fail("adb install failed");
        bail!("adb install failed");
    }
    step.ok();

    let component = format!("{}/android.app.NativeActivity", manifest.package.id);
    let step = rusk_ui::Step::start("Launching (adb shell am start)");
    let status = adb_cmd(&adb, &device)
        .arg("shell")
        .arg("am")
        .arg("start")
        .arg("-n")
        .arg(&component)
        .status()?;
    if !status.success() {
        step.fail("adb shell am start failed");
        bail!("failed to launch {component}");
    }
    step.ok();
    rusk_ui::info("run `rusk logcat` to stream this app's log output");
    Ok(())
}

/// `adb install` fails with an ambiguous "more than one device/emulator"
/// error when several are attached and no `-s` was given. Rather than
/// let that surface as a raw adb error, check up front and point at
/// `rusk devices` / `--device` so the fix is obvious.
fn warn_if_multiple_devices(adb: &std::path::Path) {
    let Ok(output) = Command::new(adb).arg("devices").output() else {
        return;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let serials: Vec<&str> = text
        .lines()
        .skip(1) // header line: "List of devices attached"
        .filter(|l| l.contains("\tdevice"))
        .filter_map(|l| l.split('\t').next())
        .collect();
    if serials.len() > 1 {
        rusk_ui::warn(format!(
            "{} devices/emulators attached — run `rusk devices` to see them, then pass --device <serial> if `adb install` fails with an ambiguity error",
            serials.len()
        ));
    }
}

/// Lists devices/emulators `adb` currently sees, parsed from `adb
/// devices -l` into a slightly friendlier table than adb's own raw
/// output (which mixes tabs and inconsistent spacing).
fn cmd_devices() -> Result<()> {
    let adb = locate_adb().context(
        "adb not found; it should have been auto-provisioned under ~/.rusk/android-sdk/platform-tools — try `rusk build` once first",
    )?;
    let output = Command::new(&adb).arg("devices").arg("-l").output()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut rows: Vec<(String, String)> = Vec::new();
    for line in text.lines().skip(1) {
        if line.trim().is_empty() {
            continue;
        }
        if let Some((serial, rest)) = line.split_once(char::is_whitespace) {
            rows.push((serial.to_string(), rest.trim().to_string()));
        }
    }
    if rows.is_empty() {
        rusk_ui::info("no devices/emulators attached — connect a device with USB debugging enabled, or run `rusk emulator start <name>`");
    } else {
        rusk_ui::header("devices");
        for (serial, detail) in rows {
            rusk_ui::info(format!("  {serial}   {detail}"));
        }
    }
    Ok(())
}

/// Streams `adb logcat`, by default pre-filtered to just this project's
/// process by piping through the PID `adb shell pidof <package>`
/// resolves — without this, `adb logcat` dumps every process on the
/// device at once, which is unusable noise when all you want is your
/// own app's `println!`/`log` output.
fn cmd_logcat(device: Option<String>, show_all: bool) -> Result<()> {
    let adb = locate_adb().context(
        "adb not found; it should have been auto-provisioned under ~/.rusk/android-sdk/platform-tools — try `rusk build` once first",
    )?;

    if show_all {
        rusk_ui::info("streaming full logcat (unfiltered) — Ctrl+C to stop");
        let status = adb_cmd(&adb, &device).arg("logcat").status()?;
        if !status.success() {
            bail!("adb logcat exited with an error");
        }
        return Ok(());
    }

    let cwd = std::env::current_dir()?;
    let (manifest, _) = RuskManifest::discover(&cwd)
        .context("no Rusk.toml found — pass --all to stream unfiltered logcat instead")?;

    rusk_ui::info(format!(
        "streaming logcat filtered to {} — Ctrl+C to stop (--all for unfiltered)",
        manifest.package.id
    ));

    // `--pid` needs the PID at the moment logcat starts; if the app
    // isn't running yet this comes back empty and Rusk falls back to
    // unfiltered logcat with a warning, rather than failing outright.
    let pid_output = adb_cmd(&adb, &device)
        .arg("shell")
        .arg("pidof")
        .arg(&manifest.package.id)
        .output();

    let mut cmd = adb_cmd(&adb, &device);
    cmd.arg("logcat");
    match pid_output {
        Ok(o) if o.status.success() && !o.stdout.is_empty() => {
            let pid = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !pid.is_empty() {
                cmd.arg("--pid").arg(pid);
            }
        }
        _ => {
            rusk_ui::warn(format!(
                "{} doesn't look like it's running yet — showing unfiltered logcat until it starts (run `rusk run` first for PID-filtered output)",
                manifest.package.id
            ));
        }
    }

    let status = cmd.status()?;
    if !status.success() {
        bail!("adb logcat exited with an error");
    }
    Ok(())
}

fn cmd_uninstall(device: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let (manifest, _) = RuskManifest::discover(&cwd)?;
    let adb = locate_adb().context(
        "adb not found; it should have been auto-provisioned under ~/.rusk/android-sdk/platform-tools — try `rusk build` once first",
    )?;

    let step = rusk_ui::Step::start(format!("Uninstalling {}", manifest.package.id));
    let status = adb_cmd(&adb, &device).arg("uninstall").arg(&manifest.package.id).status()?;
    if !status.success() {
        step.fail("adb uninstall failed (package may not be installed)");
        bail!("adb uninstall failed");
    }
    step.ok();
    Ok(())
}

fn cmd_doctor() -> Result<()> {
    rusk_ui::header("rusk doctor");
    check_tool("cargo", &["--version"]);
    check_tool("rustc", &["--version"]);
    check_tool("rustup", &["--version"]);
    check_tool("javac", &["-version"]);
    match locate_adb() {
        Some(path) => rusk_ui::info(format!("adb: found at {}", path.display())),
        None => rusk_ui::warn("adb: not found — will be auto-provisioned on first `rusk build`"),
    }

    if which::which("rustup").is_ok() {
        let out = Command::new("rustup").arg("target").arg("list").arg("--installed").output();
        match out {
            Ok(o) if o.status.success() => {
                let installed = String::from_utf8_lossy(&o.stdout);
                for triple in ["aarch64-linux-android", "armv7-linux-androideabi", "x86_64-linux-android", "i686-linux-android"] {
                    if installed.contains(triple) {
                        rusk_ui::info(format!("rustup target {triple}: installed"));
                    } else {
                        rusk_ui::info(format!("rustup target {triple}: not installed yet — `rusk build` adds it automatically"));
                    }
                }
            }
            _ => rusk_ui::warn("could not list rustup targets"),
        }
    } else {
        rusk_ui::warn("rustup not found on PATH — `rusk build` cross-compiles by invoking `cargo`/`rustup` directly, so at least one of them needs to be on PATH with Android std installed");
    }

    match which::which("clang").or_else(|_| which::which("clang-cl")) {
        Ok(p) => rusk_ui::info(format!("clang (for bindgen/libclang): found at {}", p.display())),
        Err(_) => rusk_ui::info(
            "no system clang found — only relevant if a project's build.rs uses bindgen; install LLVM (libclang-dev on Linux, LLVM installer on Windows) if `rusk build` fails inside build.rs",
        ),
    }

    match std::env::var("ANDROID_HOME").or_else(|_| std::env::var("ANDROID_SDK_ROOT")) {
        Ok(path) => rusk_ui::info(format!("ANDROID_HOME = {path}")),
        Err(_) => rusk_ui::info(
            "ANDROID_HOME / ANDROID_SDK_ROOT not set — rusk build will auto-provision build-tools/platform/platform-tools under ~/.rusk/android-sdk on first use",
        ),
    }

    let cwd = std::env::current_dir()?;
    match RuskManifest::discover(&cwd) {
        Ok((manifest, _)) => {
            rusk_ui::info(format!("Rusk.toml OK — package id {}", manifest.package.id));
            for abi in &manifest.abi.targets {
                if rusk_build::check_abi(abi) {
                    rusk_ui::info(format!("  ABI {abi}: supported"));
                } else {
                    rusk_ui::warn(format!("  ABI {abi}: unknown"));
                }
            }
        }
        Err(e) => rusk_ui::warn(format!("no valid Rusk.toml in this directory tree: {e}")),
    }
    Ok(())
}

fn check_tool(name: &str, version_args: &[&str]) {
    match which::which(name) {
        Ok(path) => {
            let ok = Command::new(&path).args(version_args).output().map(|o| o.status.success()).unwrap_or(false);
            if ok {
                rusk_ui::info(format!("{name}: found at {}", path.display()));
            } else {
                rusk_ui::warn(format!("{name}: found at {} but did not respond as expected", path.display()));
            }
        }
        Err(_) => rusk_ui::warn(format!("{name}: not found on PATH")),
    }
}

/// Prefers the `adb` Rusk auto-installed under `~/.rusk/android-sdk`,
/// falling back to `ANDROID_HOME`/PATH for a developer-managed install.
fn locate_adb() -> Option<PathBuf> {
    if let Some(home) = dirs::home_dir() {
        let exe = if cfg!(windows) { ".exe" } else { "" };
        let candidate = home
            .join(".rusk")
            .join("android-sdk")
            .join("platform-tools")
            .join(format!("adb{exe}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if let Ok(sdk) = std::env::var("ANDROID_HOME").or_else(|_| std::env::var("ANDROID_SDK_ROOT")) {
        let exe = if cfg!(windows) { ".exe" } else { "" };
        let candidate = PathBuf::from(sdk).join("platform-tools").join(format!("adb{exe}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    which::which("adb").ok()
}

fn cmd_emulator(action: EmulatorAction) -> Result<()> {
    match action {
        EmulatorAction::List => {
            let avds = rusk_sdkmgr::list_avds()?;
            if avds.is_empty() {
                rusk_ui::info("no AVDs yet — run `rusk emulator create` to make one");
            } else {
                rusk_ui::header("AVDs");
                for name in avds {
                    rusk_ui::info(format!("  {name}"));
                }
            }
            Ok(())
        }
        EmulatorAction::Create { name } => {
            let cwd = std::env::current_dir()?;
            let (manifest, _) = RuskManifest::discover(&cwd).context(
                "no Rusk.toml found — `rusk emulator create` uses the current project's package.sdk_target to pick a system image",
            )?;
            let api = manifest.package.sdk_target;
            let avd_name = name.unwrap_or_else(|| format!("rusk-{api}"));

            rusk_ui::header(&format!("Provisioning emulator components for android-{api}"));
            let components = rusk_sdkmgr::ensure_emulator_components(api)?;

            let step = rusk_ui::Step::start(format!("Creating AVD \"{avd_name}\""));
            match rusk_sdkmgr::create_avd(&components, &avd_name, api) {
                Ok(()) => step.ok(),
                Err(e) => {
                    step.fail(&e.to_string());
                    return Err(e.into());
                }
            }
            rusk_ui::info(format!("created AVD \"{avd_name}\" — run `rusk emulator start {avd_name}`"));
            Ok(())
        }
        EmulatorAction::Start { name } => {
            let cwd = std::env::current_dir()?;
            let api = RuskManifest::discover(&cwd)
                .map(|(m, _)| m.package.sdk_target)
                .unwrap_or(34);
            let components = rusk_sdkmgr::ensure_emulator_components(api)?;

            let step = rusk_ui::Step::start(format!("Starting emulator \"{name}\""));
            match rusk_sdkmgr::launch_avd(&components, &name) {
                Ok(_child) => {
                    step.ok();
                    rusk_ui::info("emulator launched in the background — `rusk run` once it finishes booting");
                    Ok(())
                }
                Err(e) => {
                    step.fail(&e.to_string());
                    Err(e.into())
                }
            }
        }
    }
}

fn cmd_clean() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let (_, root) = RuskManifest::discover(&cwd)?;
    for sub in ["target/rusk-staging", "target/rusk-out"] {
        let path = root.join(sub);
        if path.exists() {
            std::fs::remove_dir_all(&path)?;
            rusk_ui::info(format!("removed {}", path.display()));
        }
    }
    Ok(())
}

fn cmd_cache(action: CacheAction) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let (_, root) = RuskManifest::discover(&cwd).context(
        "no Rusk.toml found — run this from inside a Rusk project",
    )?;
    let cache_dir = rusk_cache::project_cache_dir(&root)?;

    match action {
        CacheAction::Stats => {
            let download_cache = rusk_cache::DownloadCache::load(&rusk_cache::download_cache_path(&cache_dir))?;
            rusk_ui::header("cache stats");
            rusk_ui::info(rusk_cache::format_stats(&download_cache));
            for (tool, ok, fail) in download_cache.build_step_summary() {
                if fail == 0 {
                    rusk_ui::info(format!("  {tool}: {ok} succeeded"));
                } else {
                    rusk_ui::warn(format!("  {tool}: {ok} succeeded, {fail} failed"));
                }
            }
            let lock_path = rusk_cache::project_lock_path(&root);
            match rusk_cache::RuskLock::load(&lock_path)? {
                Some(lock) => rusk_ui::info(format!(
                    "Rusk.lock: NDK {}, build-tools {}, platform android-{}, {} Java dep(s)",
                    lock.ndk_version,
                    lock.build_tools_version,
                    lock.platform_api,
                    lock.java.len()
                )),
                None => rusk_ui::info("Rusk.lock: not yet generated — run `rusk build` once"),
            }
            rusk_ui::info(format!("cache directory: {}", cache_dir.display()));
        }
        CacheAction::Inspect => {
            let path = rusk_cache::download_cache_path(&cache_dir);
            if !path.is_file() {
                rusk_ui::info("no Rusk.downloadcache yet — run `rusk build` once");
                return Ok(());
            }
            let text = std::fs::read_to_string(&path)?;
            println!("{text}");
        }
        CacheAction::InspectSnapshot => {
            let snapshot_path = cache_dir.join("rusk.lock.cache.xz");
            if !snapshot_path.is_file() {
                rusk_ui::info("no rusk.lock.cache.xz snapshot yet — run `rusk build` once");
                return Ok(());
            }
            let text = rusk_cache::read_compressed_snapshot(&snapshot_path)?;
            println!("{text}");
        }
        CacheAction::Clear => {
            if cache_dir.is_dir() {
                std::fs::remove_dir_all(&cache_dir)?;
                rusk_ui::info(format!("removed {}", cache_dir.display()));
            } else {
                rusk_ui::info("nothing to clear — no local cache directory exists yet");
            }
        }
    }
    Ok(())
}

fn cmd_lint(strict: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let (manifest, _) = RuskManifest::discover(&cwd).context("loading Rusk.toml")?;

    let findings = rusk_lint::run(&manifest);
    if findings.is_empty() {
        rusk_ui::info("no issues found");
        return Ok(());
    }

    rusk_ui::header(&format!("rusk lint — {} finding(s)", findings.len()));
    let mut has_error = false;
    for f in &findings {
        let line = format!("[{}] {}", f.id, f.message);
        match f.severity {
            rusk_lint::Severity::Error => {
                has_error = true;
                rusk_ui::error(&line);
            }
            rusk_lint::Severity::Warning => rusk_ui::warn(&line),
            rusk_lint::Severity::Info => rusk_ui::info(&line),
        }
        if let Some(hint) = &f.hint {
            rusk_ui::info(format!("  hint: {hint}"));
        }
    }

    if strict && has_error {
        bail!("{} error-severity finding(s) — failing due to --strict", findings.iter().filter(|f| f.severity == rusk_lint::Severity::Error).count());
    }
    Ok(())
}

fn cmd_keystore(action: KeystoreAction) -> Result<()> {
    match action {
        KeystoreAction::Generate { path, alias, force } => {
            rusk_ui::header("Generate release keystore");
            let store_password = prompt_password("Keystore password (min 6 chars): ")?;
            let confirm = prompt_password("Confirm keystore password: ")?;
            if store_password != confirm {
                bail!("passwords did not match");
            }
            let common_name = prompt_line("Your name or organization (used as the certificate CN): ")?;

            let opts = rusk_keystore::GenerateOptions {
                keystore_path: path.clone(),
                alias,
                store_password,
                key_password: String::new(), // reuse store password
                validity_days: rusk_keystore::default_validity_days(),
                key_size: 2048,
                dname: rusk_keystore::DistinguishedName {
                    common_name: if common_name.is_empty() { "Unknown".to_string() } else { common_name },
                    ..Default::default()
                },
                force,
            };
            rusk_keystore::generate(&opts)?;
            rusk_ui::info(format!("keystore written to {}", path.display()));
            rusk_ui::info("add a [signing] section to Rusk.toml pointing at this keystore before your next `rusk build --release`");
            rusk_ui::warn("back this file up somewhere safe — losing it means losing the ability to publish updates to an already-shipped app");
            Ok(())
        }
        KeystoreAction::List { path } => {
            let password = prompt_password("Keystore password: ")?;
            let entries = rusk_keystore::list_entries(&path, &password)?;
            if entries.is_empty() {
                rusk_ui::info("keystore contains no entries");
                return Ok(());
            }
            rusk_ui::header(&format!("{} — {} entries", path.display(), entries.len()));
            for e in entries {
                rusk_ui::info(format!("alias: {}  ({})", e.alias, e.entry_type));
                if let Some(fp) = e.sha256_fingerprint {
                    rusk_ui::info(format!("  SHA256: {fp}"));
                }
                if let (Some(from), Some(until)) = (e.valid_from, e.valid_until) {
                    rusk_ui::info(format!("  valid: {from} until {until}"));
                }
            }
            Ok(())
        }
        KeystoreAction::Verify { apk } => {
            let cwd = std::env::current_dir()?;
            let (manifest, root) = RuskManifest::discover(&cwd).context("loading Rusk.toml")?;
            let apk_path = match apk {
                Some(p) => p,
                None => root.join("target/rusk-out").join(format!("{}.apk", manifest.package.name)),
            };
            if !apk_path.is_file() {
                bail!("{} not found — run `rusk build` first, or pass an explicit APK path", apk_path.display());
            }
            let (tools, _) = resolve_apksigner()?;
            let result = rusk_keystore::verify_apk(&tools, &apk_path)?;
            if result.verified {
                rusk_ui::info(format!(
                    "verified — schemes: v1={} v2={} v3={} v4={}",
                    result.v1_scheme, result.v2_scheme, result.v3_scheme, result.v4_scheme
                ));
            } else {
                rusk_ui::error("signature did NOT verify");
                println!("{}", result.raw_output);
            }
            Ok(())
        }
    }
}

/// Locates `apksigner` the same way `rusk-build` does — managed SDK
/// first unless `ANDROID_HOME` is already set and complete — without
/// pulling the whole `rusk-build` internal resolution function in,
/// since `rusk keystore verify` only needs the one tool.
fn resolve_apksigner() -> Result<(PathBuf, ())> {
    if let Some(home) = std::env::var_os("ANDROID_HOME").or_else(|| std::env::var_os("ANDROID_SDK_ROOT")) {
        let base = PathBuf::from(home).join("build-tools");
        if let Ok(entries) = std::fs::read_dir(&base) {
            let mut versions: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
            versions.sort();
            if let Some(latest) = versions.last() {
                let bat = if cfg!(windows) { ".bat" } else { "" };
                let candidate = latest.join(format!("apksigner{bat}"));
                if candidate.is_file() {
                    return Ok((candidate, ()));
                }
            }
        }
    }
    let cwd = std::env::current_dir()?;
    let (manifest, _) = RuskManifest::discover(&cwd)?;
    let managed = rusk_sdkmgr::ensure_components(&manifest.sdk.build_tools_version, manifest.package.sdk_compile)?;
    Ok((managed.build_tools().apksigner, ()))
}

fn cmd_bundle(release: bool, flavor: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let (manifest, root) = RuskManifest::discover(&cwd).context("loading Rusk.toml")?;

    // Building the bundle reuses the same compiled .so files / resources
    // an APK build would produce, so run the normal build pipeline first
    // to guarantee they're fresh, then package the result as an .aab
    // instead of (in addition to) a universal APK.
    let opts = BuildOptions {
        project_root: root.clone(),
        profile: if release { Profile::Release } else { Profile::Debug },
        flavor: flavor.clone(),
    };
    rusk_build::build(&manifest, &opts)?;

    let version = rusk_bundle::bundletool_version_for(&manifest);
    let bundletool_jar = rusk_bundle::ensure_bundletool(&version)?;

    rusk_ui::warn("`rusk bundle` currently packages the same staged resources rusk-build already produced; full per-module bundle assembly (splitting native libs, resources, and dex into a proper bundletool base-module layout) is still basic — inspect the .aab with `bundletool dump manifest` before uploading to Play Console");

    let staging = root.join("target/rusk-staging");
    let out_dir = root.join("target/rusk-out");
    std::fs::create_dir_all(&out_dir)?;
    let aab_name = match &flavor {
        Some(f) => format!("{}-{f}.aab", manifest.package.name),
        None => format!("{}.aab", manifest.package.name),
    };
    let out_aab = out_dir.join(aab_name);

    let inputs = rusk_bundle::BundleInputs {
        bundletool_jar: &bundletool_jar,
        base_module_dir: &staging,
        out_aab: &out_aab,
    };
    rusk_bundle::build_bundle(&inputs)?;
    rusk_ui::info(format!("bundle written to {}", out_aab.display()));
    Ok(())
}

/// Reads a line from stdin with a visible prompt, trimming the trailing
/// newline. Used for non-sensitive input (names, confirmations) where
/// echoing what was typed is fine and expected.
fn prompt_line(prompt: &str) -> Result<String> {
    use std::io::Write;
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// Reads a password from stdin without echoing it to the terminal.
/// Deliberately dependency-free (no external "rpassword"-style crate) —
/// implemented directly against the platform's raw terminal APIs so
/// `rusk-cli` doesn't grow an extra dependency just for this one prompt.
#[cfg(unix)]
fn prompt_password(prompt: &str) -> Result<String> {
    use std::io::Write;
    print!("{prompt}");
    std::io::stdout().flush()?;

    // Disabling terminal echo portably across Linux/macOS/BSD without a
    // raw termios FFI call: the `termios` struct's field layout differs
    // between glibc (Linux) and the BSD-derived libc macOS uses (order
    // of c_cc vs c_ispeed/c_ospeed, and NCCS itself differs), so a
    // hand-written #[repr(C)] struct guessing at one layout is correct
    // on at most one of those platforms. Shelling out to `stty`, which
    // every Unix ships and which already encodes its own platform's
    // correct layout internally, sidesteps the whole problem.
    let echo_disabled = Command::new("stty").arg("-echo").status().map(|s| s.success()).unwrap_or(false);

    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    println!();

    if echo_disabled {
        let _ = Command::new("stty").arg("echo").status();
    }
    Ok(line.trim().to_string())
}

#[cfg(windows)]
fn prompt_password(prompt: &str) -> Result<String> {
    use std::io::Write;
    print!("{prompt}");
    std::io::stdout().flush()?;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(nStdHandle: i32) -> isize;
        fn GetConsoleMode(hConsoleHandle: isize, lpMode: *mut u32) -> i32;
        fn SetConsoleMode(hConsoleHandle: isize, dwMode: u32) -> i32;
    }
    const STD_INPUT_HANDLE: i32 = -10;
    const ENABLE_ECHO_INPUT: u32 = 0x0004;

    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let mut original_mode: u32 = 0;
    let echo_disabled = if handle != 0 && handle != -1isize && unsafe { GetConsoleMode(handle, &mut original_mode) } != 0 {
        unsafe { SetConsoleMode(handle, original_mode & !ENABLE_ECHO_INPUT) != 0 }
    } else {
        false
    };

    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    println!();

    if echo_disabled {
        unsafe {
            SetConsoleMode(handle, original_mode);
        }
    }
    Ok(line.trim().to_string())
}

fn sanitize_ident(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect()
}
