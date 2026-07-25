//! rusk-symbolize: turns a raw native crash backtrace from `adb logcat`
//! (a list of bare hex addresses — that's all a stripped release `.so`
//! leaves behind, since `rusk build --release` strips symbols by
//! default to shrink the APK) back into function names and source file
//! + line numbers, using the *unstripped* copy of the library that
//! `cargo build` already produced before stripping, plus the NDK's own
//! `llvm-addr2line`.
//!
//! Without this, a release-build native crash report is close to
//! useless: `#00 pc 0002a4f0  libmyapp.so` tells you an offset into a
//! binary, not which function crashed. This is the same fundamental
//! workflow as Android Studio's built-in "ndk-stack"/crash symbolication
//! tool, driven from the command line so it fits a Rusk project that
//! never opens Android Studio at all.

use std::path::{Path, PathBuf};
use std::process::Command;

use rusk_ndk::NdkHome;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SymbolizeError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("unstripped library not found at {0}; symbolication needs the debug-info copy cargo produced before `rusk build --release` stripped it — check target/<triple>/release/deps/")]
    UnstrippedLibMissing(PathBuf),
    #[error("`llvm-addr2line` exited with status {0}")]
    ToolFailed(i32),
}

/// One parsed frame from an Android native backtrace, e.g.:
///   `#01 pc 0002a4f0  libmy_app.so (my_app::render::draw_frame+128)`
/// or, in the more common stripped-release case with no symbol at all:
///   `#01 pc 0002a4f0  libmy_app.so`
#[derive(Debug, Clone)]
pub struct BacktraceFrame {
    pub frame_number: u32,
    pub program_counter: String,
    pub library: String,
    /// Present when logcat already had a symbol (debug builds, or a
    /// release build that wasn't stripped) — symbolization is only
    /// needed when this is `None`.
    pub existing_symbol: Option<String>,
}

/// Parses the `#NN pc <hex> <lib> (<symbol>)?` lines Android's native
/// crash handler (`libc`'s debuggerd) writes to logcat under a
/// `backtrace:` header. Lines that don't match this shape are ignored
/// rather than erroring, since a real logcat capture has plenty of
/// unrelated surrounding lines mixed in.
pub fn parse_backtrace(logcat_text: &str) -> Vec<BacktraceFrame> {
    let mut frames = Vec::new();
    for line in logcat_text.lines() {
        let Some(hash_idx) = line.find('#') else { continue };
        let rest = &line[hash_idx + 1..];
        let mut parts = rest.split_whitespace();
        let Some(num_str) = parts.next() else { continue };
        let Ok(frame_number) = num_str.parse::<u32>() else { continue };
        let Some(pc_marker) = parts.next() else { continue };
        if pc_marker != "pc" {
            continue;
        }
        let Some(pc) = parts.next() else { continue };
        let Some(lib) = parts.next() else { continue };

        let remainder: String = parts.collect::<Vec<_>>().join(" ");
        let existing_symbol = remainder
            .trim()
            .strip_prefix('(')
            .and_then(|s| s.strip_suffix(')'))
            .map(|s| s.to_string());

        frames.push(BacktraceFrame {
            frame_number,
            program_counter: pc.to_string(),
            library: lib.trim_end_matches(')').to_string(),
            existing_symbol,
        });
    }
    frames
}

/// A frame after symbolication: the function name and, when debug info
/// is present in the unstripped library, the source file + line.
pub struct SymbolizedFrame {
    pub frame_number: u32,
    pub program_counter: String,
    pub library: String,
    pub function: String,
    pub file_line: Option<String>,
}

/// Resolves every frame in `frames` whose library matches
/// `unstripped_lib`'s file name against that library via
/// `llvm-addr2line -f -C -e <lib> <address>...`, batching all addresses
/// into a single invocation rather than one process spawn per frame.
pub fn symbolize(
    ndk: &NdkHome,
    unstripped_lib: &Path,
    frames: &[BacktraceFrame],
) -> Result<Vec<SymbolizedFrame>, SymbolizeError> {
    if !unstripped_lib.is_file() {
        return Err(SymbolizeError::UnstrippedLibMissing(unstripped_lib.to_path_buf()));
    }
    let lib_name = unstripped_lib
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let relevant: Vec<&BacktraceFrame> = frames.iter().filter(|f| f.library == lib_name).collect();
    if relevant.is_empty() {
        return Ok(Vec::new());
    }

    let addr2line = addr2line_path(ndk);
    let mut cmd = Command::new(&addr2line);
    cmd.arg("-f") // print function names
        .arg("-C") // demangle
        .arg("-e")
        .arg(unstripped_lib);
    for frame in &relevant {
        cmd.arg(&frame.program_counter);
    }

    let output = cmd.output().map_err(|source| SymbolizeError::Io {
        path: addr2line.clone(),
        source,
    })?;
    if !output.status.success() {
        return Err(SymbolizeError::ToolFailed(output.status.code().unwrap_or(-1)));
    }

    // llvm-addr2line -f prints two lines per input address: function
    // name, then "file:line" (or "??:0" when no debug info is present).
    let text = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = text.lines().collect();

    let mut out = Vec::new();
    for (i, frame) in relevant.iter().enumerate() {
        let function = lines.get(i * 2).map(|s| s.to_string()).unwrap_or_else(|| "??".to_string());
        let file_line = lines.get(i * 2 + 1).map(|s| s.to_string()).filter(|s| s != "??:0");
        out.push(SymbolizedFrame {
            frame_number: frame.frame_number,
            program_counter: frame.program_counter.clone(),
            library: frame.library.clone(),
            function,
            file_line,
        });
    }
    Ok(out)
}

fn addr2line_path(ndk: &NdkHome) -> PathBuf {
    let exe = if cfg!(windows) { ".exe" } else { "" };
    ndk.toolchain_bin().join(format!("llvm-addr2line{exe}"))
}

/// Renders symbolized frames as a readable backtrace, one frame per
/// line, closely mirroring the format `ndk-stack`/Android Studio's own
/// crash symbolication output uses so it's immediately familiar.
pub fn format_backtrace(frames: &[SymbolizedFrame]) -> String {
    let mut out = String::new();
    for f in frames {
        let loc = f.file_line.as_deref().unwrap_or("<no debug info>");
        out.push_str(&format!(
            "#{:02} pc {}  {} ({} at {})\n",
            f.frame_number, f.program_counter, f.library, f.function, loc
        ));
    }
    out
}
