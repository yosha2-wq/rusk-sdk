//! rusk-profile: drives `simpleperf`, the sampling CPU profiler Google
//! bundles inside the NDK (`simpleperf/<abi>/simpleperf` — a device
//! binary, not a host tool) to profile a running Rusk app without
//! needing Android Studio's Profiler window at all.
//!
//! The workflow is: push the device `simpleperf` binary to
//! `/data/local/tmp/`, run it against the app's PID to record samples
//! into `/data/local/tmp/perf.data`, pull that file back to the host,
//! then convert it into `perf.data.trace.pb` (gzipped protobuf) via the
//! NDK's host-side `report.py`/`pprof_proto_generator.py` scripts —
//! which Firefox Profiler and `pprof` can both open directly.

use std::path::{Path, PathBuf};
use std::process::Command;

use rusk_ndk::NdkHome;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProfileError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("simpleperf device binary not found for abi {0} in this NDK; check that the NDK version supports profiling on this ABI")]
    SimpleperfBinaryMissing(String),
    #[error("`adb {0}` exited with status {1}")]
    AdbFailed(String, i32),
    #[error("could not determine the running PID for package \"{0}\" — is the app currently running? (`rusk run` first)")]
    PidNotFound(String),
}

const DEVICE_TMP_DIR: &str = "/data/local/tmp";

/// Locates the on-device `simpleperf` binary matching `abi` inside the
/// NDK's bundled `simpleperf/` directory (NDK ships one per ABI, since
/// it has to run *on the device*, not the host).
fn device_simpleperf_binary(ndk: &NdkHome, abi: &str) -> Option<PathBuf> {
    let candidate = ndk.root.join("simpleperf").join(abi).join("simpleperf");
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

fn run_adb(adb: &Path, device: Option<&str>, args: &[&str]) -> Result<std::process::Output, ProfileError> {
    let mut cmd = Command::new(adb);
    if let Some(serial) = device {
        cmd.arg("-s").arg(serial);
    }
    cmd.args(args);
    cmd.output().map_err(|source| ProfileError::Io {
        path: adb.to_path_buf(),
        source,
    })
}

/// Resolves the running PID for `package_id` via `adb shell pidof`,
/// needed because `simpleperf record -p <pid>` (attaching to an already-
/// running process) is far less disruptive than `simpleperf record -a`
/// (whole-system profiling) for a one-app profiling session.
pub fn resolve_pid(adb: &Path, device: Option<&str>, package_id: &str) -> Result<u32, ProfileError> {
    let output = run_adb(adb, device, &["shell", "pidof", package_id])?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.trim()
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| ProfileError::PidNotFound(package_id.to_string()))
}

pub struct RecordOptions<'a> {
    pub adb: &'a Path,
    pub ndk: &'a NdkHome,
    pub device: Option<&'a str>,
    pub abi: &'a str,
    pub pid: u32,
    /// Sampling duration in seconds.
    pub duration_secs: u32,
    /// Sampling frequency in Hz (simpleperf default is 4000; lower this
    /// for a longer recording without an oversized perf.data file).
    pub frequency_hz: u32,
    /// Record call graphs (`--call-graph dwarf`) so the resulting trace
    /// shows full stacks, not just leaf-frame hit counts. Costs more
    /// overhead on the device while recording.
    pub call_graph: bool,
}

/// Pushes `simpleperf` to the device (if not already present from a
/// previous run), records `duration_secs` worth of samples against
/// `pid`, then pulls `perf.data` back to `out_local_path` on the host.
pub fn record(opts: &RecordOptions, out_local_path: &Path) -> Result<(), ProfileError> {
    let device_binary = device_simpleperf_binary(opts.ndk, opts.abi)
        .ok_or_else(|| ProfileError::SimpleperfBinaryMissing(opts.abi.to_string()))?;
    let device_dest = format!("{DEVICE_TMP_DIR}/simpleperf");

    let step = rusk_ui::Step::start("Pushing simpleperf to device");
    let out = run_adb(opts.adb, opts.device, &["push", &device_binary.to_string_lossy(), &device_dest])?;
    if !out.status.success() {
        step.fail("adb push failed");
        return Err(ProfileError::AdbFailed("push".to_string(), out.status.code().unwrap_or(-1)));
    }
    step.ok();

    let step = rusk_ui::Step::start("Marking simpleperf executable on device");
    let out = run_adb(opts.adb, opts.device, &["shell", "chmod", "755", &device_dest])?;
    if !out.status.success() {
        step.fail("adb shell chmod failed");
        return Err(ProfileError::AdbFailed("shell chmod".to_string(), out.status.code().unwrap_or(-1)));
    }
    step.ok();

    let pid_str = opts.pid.to_string();
    let freq_str = opts.frequency_hz.to_string();
    let dur_str = opts.duration_secs.to_string();
    let mut record_args: Vec<&str> = vec![
        "shell",
        &device_dest,
        "record",
        "-p",
        &pid_str,
        "-f",
        &freq_str,
        "--duration",
        &dur_str,
        "-o",
        "/data/local/tmp/perf.data",
    ];
    if opts.call_graph {
        record_args.push("--call-graph");
        record_args.push("dwarf");
    }

    let step = rusk_ui::Step::start(format!(
        "Recording {}s of samples at {}Hz (pid {})",
        opts.duration_secs, opts.frequency_hz, opts.pid
    ));
    let out = run_adb(opts.adb, opts.device, &record_args)?;
    if !out.status.success() {
        step.fail("simpleperf record failed on-device");
        return Err(ProfileError::AdbFailed("shell simpleperf record".to_string(), out.status.code().unwrap_or(-1)));
    }
    step.ok();

    let step = rusk_ui::Step::start("Pulling perf.data from device");
    if let Some(parent) = out_local_path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ProfileError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let out = run_adb(
        opts.adb,
        opts.device,
        &["pull", "/data/local/tmp/perf.data", &out_local_path.to_string_lossy()],
    )?;
    if !out.status.success() {
        step.fail("adb pull failed");
        return Err(ProfileError::AdbFailed("pull".to_string(), out.status.code().unwrap_or(-1)));
    }
    step.ok();

    rusk_ui::info(format!(
        "recorded trace saved to {} — inspect with `simpleperf report -i <file>` (host script bundled in the NDK's simpleperf/report.py) or convert to pprof format for Firefox Profiler",
        out_local_path.display()
    ));
    Ok(())
}

/// Runs the NDK-bundled `simpleperf/report.py` (a host-side Python
/// script, not a compiled binary) against a recorded `perf.data`,
/// printing a flat function-level hit-count report to stdout — the
/// fastest "what's actually hot" answer without opening any GUI tool.
pub fn report(ndk: &NdkHome, perf_data: &Path) -> Result<(), ProfileError> {
    let script = ndk.root.join("simpleperf").join("report.py");
    let status = Command::new("python3")
        .arg(&script)
        .arg("-i")
        .arg(perf_data)
        .status()
        .map_err(|source| ProfileError::Io {
            path: script.clone(),
            source,
        })?;
    if !status.success() {
        return Err(ProfileError::AdbFailed("report.py".to_string(), status.code().unwrap_or(-1)));
    }
    Ok(())
}
