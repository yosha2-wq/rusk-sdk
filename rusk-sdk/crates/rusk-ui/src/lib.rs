//! rusk-ui: terminal rendering for the Rusk toolchain.
//!
//! No external progress-bar crate is used on purpose: the whole visual
//! language of the `rusk` compiler front-end (spinner phases, gradient
//! bars, ETA estimation, byte-size formatting, step trees) lives here so
//! every tool in the workspace renders output the same way, and stays
//! legible in both Windows Terminal and legacy `cmd.exe` consoles.

use std::io::Write;
use std::time::{Duration, Instant};

pub mod color {
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const DIM: &str = "\x1b[2m";
    pub const ITALIC: &str = "\x1b[3m";
    pub const RED: &str = "\x1b[38;5;203m";
    pub const ORANGE: &str = "\x1b[38;5;215m";
    pub const GREEN: &str = "\x1b[38;5;114m";
    pub const TEAL: &str = "\x1b[38;5;79m";
    pub const YELLOW: &str = "\x1b[38;5;221m";
    pub const BLUE: &str = "\x1b[38;5;75m";
    pub const CYAN: &str = "\x1b[38;5;80m";
    pub const MAGENTA: &str = "\x1b[38;5;176m";
    pub const PURPLE: &str = "\x1b[38;5;141m";
    pub const GRAY: &str = "\x1b[38;5;244m";
    pub const DARK_GRAY: &str = "\x1b[38;5;238m";

    /// A five-stop cyan → teal → green ramp used to color the filled
    /// portion of progress bars, giving completion a sense of "warming
    /// up" instead of a single flat color the whole way through.
    pub const BAR_RAMP: &[&str] = &[
        "\x1b[38;5;75m",
        "\x1b[38;5;80m",
        "\x1b[38;5;79m",
        "\x1b[38;5;114m",
        "\x1b[38;5;120m",
    ];
}

/// Braille spinner frames — smooth 10-frame rotation, safe in both modern
/// and legacy Windows consoles (Windows Terminal, and `cmd.exe` with
/// UTF-8 codepage 65001, which Rusk enables on startup on Windows).
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// A secondary "pulse" glyph cycled alongside the spinner on long-running
/// steps (NDK unpacking, dependency resolution) so a step that's been
/// running for several seconds visibly keeps breathing rather than
/// looking stuck on the same frozen character.
const PULSE_FRAMES: &[&str] = &["·", "•", "●", "•"];

/// On Windows, legacy `cmd.exe`/PowerShell consoles default to a codepage
/// that mangles the box-drawing/braille glyphs Rusk prints (this is what
/// produced the CP866 mojibake in linker output on older builds — the
/// same underlying codepage issue, just hitting our own UI text this
/// time instead of clang's). Rusk calls this once at startup to force
/// UTF-8 (65001) and enable ANSI escape processing, which every modern
/// Windows 10/11 console honors.
///
/// This also disables **QuickEdit Mode** on the console's input buffer.
/// QuickEdit is on by default in `cmd.exe` (and inherited by many
/// PowerShell profiles); it means a left-click, drag-select, or even a
/// mouse-wheel scroll over the console window puts the console into
/// text-selection mode, which suspends the *entire* process — not just
/// scrolling — until the user presses Enter or Esc. For a long-running
/// build that streams live progress bars, this looks exactly like a
/// hang: output stops mid-download and the process appears frozen
/// whenever the person tries to scroll back to read something. Rusk
/// turns this off unconditionally on startup so scrolling/selecting
/// never blocks the build; selection can still be done by holding Shift
/// while dragging if the console host supports it, or by resizing the
/// buffer, without the freeze.
#[cfg(windows)]
pub fn init_console() {
    use std::os::windows::io::AsRawHandle;
    unsafe {
        #[link(name = "kernel32")]
        extern "system" {
            fn SetConsoleOutputCP(wCodePageID: u32) -> i32;
            fn SetConsoleMode(hConsoleHandle: isize, dwMode: u32) -> i32;
            fn GetConsoleMode(hConsoleHandle: isize, lpMode: *mut u32) -> i32;
            fn GetStdHandle(nStdHandle: i32) -> isize;
        }

        // --- UTF-8 output + ANSI escape processing (stderr, since
        // that's the handle every rusk-ui function writes through) ---
        SetConsoleOutputCP(65001);
        let out_handle = std::io::stderr().as_raw_handle() as isize;
        let mut out_mode: u32 = 0;
        if GetConsoleMode(out_handle, &mut out_mode) != 0 {
            const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
            SetConsoleMode(out_handle, out_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }

        // --- Disable QuickEdit Mode on the console's input buffer ---
        const STD_INPUT_HANDLE: i32 = -10;
        const ENABLE_QUICK_EDIT_MODE: u32 = 0x0040;
        const ENABLE_EXTENDED_FLAGS: u32 = 0x0080;
        // ENABLE_EXTENDED_FLAGS must be set for SetConsoleMode to honor
        // any change to ENABLE_QUICK_EDIT_MODE at all — omitting it
        // makes the call silently do nothing.
        const ENABLE_INSERT_MODE: u32 = 0x0020;
        const ENABLE_PROCESSED_INPUT: u32 = 0x0001;

        let in_handle = GetStdHandle(STD_INPUT_HANDLE);
        if in_handle != 0 && in_handle != -1isize {
            let mut in_mode: u32 = 0;
            if GetConsoleMode(in_handle, &mut in_mode) != 0 {
                let new_mode = (in_mode & !ENABLE_QUICK_EDIT_MODE)
                    | ENABLE_EXTENDED_FLAGS
                    | ENABLE_INSERT_MODE
                    | ENABLE_PROCESSED_INPUT;
                SetConsoleMode(in_handle, new_mode);
            }
        }
    }
}

#[cfg(not(windows))]
pub fn init_console() {}

/// Renders the `rusk` compiler front-end banner shown once at the start
/// of a `build`/`run` invocation — deliberately more detailed than a
/// bare title so the very first thing printed communicates "this is a
/// real toolchain doing real work", not a one-line script.
pub fn compiler_banner(project_name: &str, version: &str) {
    let rule: String = "─".repeat(54);
    eprintln!();
    eprintln!("{}{}┌{}┐{}", color::BOLD, color::PURPLE, rule, color::RESET);
    eprintln!(
        "{}{}│{}  {}{}rusk{} {}{}  ·  Rust → Android APK toolchain{}",
        color::BOLD,
        color::PURPLE,
        color::RESET,
        color::BOLD,
        color::CYAN,
        color::RESET,
        color::DIM,
        version,
        color::RESET,
    );
    eprintln!(
        "{}{}│{}  building {}{}{}",
        color::BOLD,
        color::PURPLE,
        color::RESET,
        color::BOLD,
        project_name,
        color::RESET,
    );
    eprintln!("{}{}└{}┘{}", color::BOLD, color::PURPLE, rule, color::RESET);
}

/// A single labelled build/toolchain step, rendered like:
///   ➤ Resolving NDK toolchain
///   ✔ Resolving NDK toolchain            (312ms)
///   ✘ Resolving NDK toolchain            (failed)
///
/// Steps can be nested one level deep (`Step::start_nested`) so a
/// composite operation like "Provisioning Android SDK" can show its own
/// sub-steps (platform-tools, build-tools, platform jar) indented
/// underneath it, giving a build log a real sense of structure instead
/// of one flat list of lines.
pub struct Step {
    label: String,
    started: Instant,
    finished: bool,
    depth: usize,
}

impl Step {
    pub fn start(label: impl Into<String>) -> Self {
        Self::start_at_depth(label, 0)
    }

    pub fn start_nested(label: impl Into<String>) -> Self {
        Self::start_at_depth(label, 1)
    }

    fn start_at_depth(label: impl Into<String>, depth: usize) -> Self {
        let label = label.into();
        let indent = "  ".repeat(depth);
        let marker = if depth == 0 { "➤" } else { "↳" };
        eprintln!(
            "{indent}{}{}{marker}{} {}",
            color::BOLD,
            color::CYAN,
            color::RESET,
            label
        );
        Self {
            label,
            started: Instant::now(),
            finished: false,
            depth,
        }
    }

    pub fn ok(mut self) {
        self.finished = true;
        let indent = "  ".repeat(self.depth);
        let elapsed = self.started.elapsed();
        eprintln!(
            "{indent}{}{}✔{} {} {}({}){}",
            color::BOLD,
            color::GREEN,
            color::RESET,
            self.label,
            color::GRAY,
            fmt_duration(elapsed),
            color::RESET
        );
    }

    pub fn fail(mut self, reason: &str) {
        self.finished = true;
        let indent = "  ".repeat(self.depth);
        eprintln!(
            "{indent}{}{}✘{} {} {}— {}{}",
            color::BOLD,
            color::RED,
            color::RESET,
            self.label,
            color::RED,
            reason,
            color::RESET
        );
    }

    /// A middle ground between `ok` and `fail`: the step completed but
    /// with something worth flagging (e.g. an unverified checksum, a
    /// fallback path taken). Rendered in orange rather than red/green.
    pub fn warn_ok(mut self, note: &str) {
        self.finished = true;
        let indent = "  ".repeat(self.depth);
        let elapsed = self.started.elapsed();
        eprintln!(
            "{indent}{}{}◆{} {} {}({}, {}){}",
            color::BOLD,
            color::ORANGE,
            color::RESET,
            self.label,
            color::GRAY,
            fmt_duration(elapsed),
            note,
            color::RESET
        );
    }
}

impl Drop for Step {
    fn drop(&mut self) {
        if !self.finished {
            // Step was dropped without an explicit ok()/fail() — most
            // likely because of an early `?` return. Surface it plainly
            // rather than silently losing the failure context.
            let indent = "  ".repeat(self.depth);
            eprintln!(
                "{indent}{}{}✘{} {} {}— aborted{}",
                color::BOLD,
                color::RED,
                color::RESET,
                self.label,
                color::RED,
                color::RESET
            );
        }
    }
}

pub fn info(msg: impl AsRef<str>) {
    eprintln!("{}  {}{}", color::BLUE, msg.as_ref(), color::RESET);
}

pub fn warn(msg: impl AsRef<str>) {
    eprintln!(
        "{}{}warning{}: {}",
        color::BOLD,
        color::YELLOW,
        color::RESET,
        msg.as_ref()
    );
}

pub fn error(msg: impl AsRef<str>) {
    eprintln!(
        "{}{}error{}: {}",
        color::BOLD,
        color::RED,
        color::RESET,
        msg.as_ref()
    );
}

pub fn header(title: &str) {
    eprintln!(
        "\n{}{}== {} =={}",
        color::BOLD,
        color::MAGENTA,
        title,
        color::RESET
    );
}

/// Human-readable byte sizes, e.g. `42.3 MB`.
pub fn fmt_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit_idx = 0;
    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }
    if unit_idx == 0 {
        format!("{bytes} {}", UNITS[unit_idx])
    } else {
        format!("{size:.1} {}", UNITS[unit_idx])
    }
}

fn fmt_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else if d.as_secs() < 60 {
        format!("{:.2}s", d.as_secs_f64())
    } else {
        let secs = d.as_secs();
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}

/// Picks a color stop from the ramp indexed by how far through it the
/// current completion ratio has progressed — this is what gives a bar
/// its "warming up as it fills" gradient rather than a single flat color
/// from 0% to 100%.
fn ramp_color_for_ratio(ratio: f64) -> &'static str {
    let idx = ((ratio * (color::BAR_RAMP.len() - 1) as f64).round() as usize)
        .min(color::BAR_RAMP.len() - 1);
    color::BAR_RAMP[idx]
}

/// A download progress bar that is aware of the total payload size and
/// renders a gradient-filled bar plus a live ETA, e.g.:
///   ⠴ NDK 27.0.12077973  [██████████░░░░░░░░]  55%  61.2 MB / 118.4 MB  8.4 MB/s  ETA 6s
pub struct DownloadProgress {
    label: String,
    total: u64,
    downloaded: u64,
    last_render: Instant,
    started: Instant,
    frame: usize,
    /// Rolling window of (elapsed_secs, bytes) samples used to smooth the
    /// speed/ETA estimate instead of it jittering every redraw.
    samples: std::collections::VecDeque<(f64, u64)>,
}

impl DownloadProgress {
    pub fn new(label: impl Into<String>, total_bytes: u64) -> Self {
        let s = Self {
            label: label.into(),
            total: total_bytes,
            downloaded: 0,
            last_render: Instant::now() - Duration::from_secs(1),
            started: Instant::now(),
            frame: 0,
            samples: std::collections::VecDeque::with_capacity(16),
        };
        s.render();
        s
    }

    /// Advance the bar by `delta` bytes. Rendering is throttled to ~15Hz
    /// so piping to a log file doesn't produce megabytes of redraws.
    pub fn add(&mut self, delta: u64) {
        self.downloaded += delta;
        let elapsed = self.started.elapsed().as_secs_f64();
        self.samples.push_back((elapsed, self.downloaded));
        if self.samples.len() > 20 {
            self.samples.pop_front();
        }
        if self.last_render.elapsed() >= Duration::from_millis(66) || self.downloaded >= self.total
        {
            self.render();
        }
    }

    fn render(&self) {
        let width = 30usize;
        let ratio = if self.total == 0 {
            0.0
        } else {
            (self.downloaded as f64 / self.total as f64).min(1.0)
        };
        let filled = (ratio * width as f64).round() as usize;
        let bar_color = ramp_color_for_ratio(ratio);
        let bar: String = (0..width)
            .map(|i| if i < filled { '█' } else { '░' })
            .collect();
        let frame = SPINNER_FRAMES[self.frame % SPINNER_FRAMES.len()];
        let pct = (ratio * 100.0) as u32;
        let speed = self.speed_bps();
        let eta = self.eta(speed);
        eprint!(
            "\r{}{}{}{} {}  {}{}{}  {:>3}%  {}{} / {}{}  {}{}{}  {}ETA {}{}   ",
            color::CYAN,
            frame,
            color::RESET,
            color::BOLD,
            self.label,
            bar_color,
            bar,
            color::RESET,
            pct,
            color::BOLD,
            fmt_bytes(self.downloaded),
            fmt_bytes(self.total),
            color::RESET,
            color::GRAY,
            fmt_speed(speed),
            color::RESET,
            color::DIM,
            eta,
            color::RESET,
        );
        let _ = std::io::stderr().flush();
    }

    /// Bytes/sec estimated from the recent sample window rather than the
    /// full-transfer average, so the number reacts to a connection
    /// speeding up or throttling instead of slowly drifting.
    fn speed_bps(&self) -> f64 {
        if self.samples.len() < 2 {
            let secs = self.started.elapsed().as_secs_f64();
            return if secs <= 0.0 { 0.0 } else { self.downloaded as f64 / secs };
        }
        let (t0, b0) = self.samples.front().copied().unwrap();
        let (t1, b1) = self.samples.back().copied().unwrap();
        let dt = t1 - t0;
        if dt <= 0.0 {
            0.0
        } else {
            (b1 - b0) as f64 / dt
        }
    }

    fn eta(&self, speed_bps: f64) -> String {
        if speed_bps <= 0.0 || self.total == 0 {
            return "—".to_string();
        }
        let remaining = self.total.saturating_sub(self.downloaded) as f64;
        let secs = (remaining / speed_bps).round() as u64;
        if secs == 0 {
            "0s".to_string()
        } else if secs < 60 {
            format!("{secs}s")
        } else {
            format!("{}m {:02}s", secs / 60, secs % 60)
        }
    }

    pub fn tick_frame(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

    pub fn finish(self) {
        eprintln!(
            "\r{}{}✔{} {}  {}{} downloaded in {} {}({}){}                    ",
            color::BOLD,
            color::GREEN,
            color::RESET,
            self.label,
            color::GRAY,
            fmt_bytes(self.downloaded),
            fmt_duration(self.started.elapsed()),
            color::DIM,
            fmt_speed(self.speed_bps()),
            color::RESET,
        );
    }
}

/// Live per-file progress for archive extraction — shows which entry is
/// being written right now, how many are left, a mini progress bar, and
/// a running total of bytes written, rather than a single opaque
/// "Unpacking..." line for what can be several thousand files.
pub struct ExtractProgress {
    total: usize,
    done: usize,
    bytes_written: u64,
    started: Instant,
    last_render: Instant,
    frame: usize,
}

impl ExtractProgress {
    pub fn new(_label: impl Into<String>, total_entries: usize) -> Self {
        Self {
            total: total_entries,
            done: 0,
            bytes_written: 0,
            started: Instant::now(),
            last_render: Instant::now() - Duration::from_secs(1),
            frame: 0,
        }
    }

    /// Call once per file as it's written. `entry_name` is truncated so
    /// long archive paths don't wrap the terminal line.
    pub fn advance(&mut self, entry_name: &str) {
        self.advance_sized(entry_name, 0);
    }

    /// Same as [`advance`](Self::advance), but also accumulates
    /// `entry_bytes` into a running total shown alongside the file count
    /// — useful when the caller already knows each entry's uncompressed
    /// size (e.g. from the zip's central directory) and extraction is
    /// slow enough that "bytes written so far" is meaningful feedback.
    pub fn advance_sized(&mut self, entry_name: &str, entry_bytes: u64) {
        self.done += 1;
        self.bytes_written += entry_bytes;
        self.frame = self.frame.wrapping_add(1);
        if self.last_render.elapsed() < Duration::from_millis(50) && self.done != self.total {
            return;
        }
        self.last_render = Instant::now();

        let width = 18usize;
        let ratio = if self.total == 0 {
            1.0
        } else {
            self.done as f64 / self.total as f64
        };
        let filled = (ratio * width as f64).round() as usize;
        let bar_color = ramp_color_for_ratio(ratio);
        let bar: String = (0..width)
            .map(|i| if i < filled { '▰' } else { '▱' })
            .collect();

        let short = if entry_name.len() > 34 {
            format!("…{}", &entry_name[entry_name.len() - 33..])
        } else {
            entry_name.to_string()
        };
        let pulse = PULSE_FRAMES[(self.frame / 3) % PULSE_FRAMES.len()];
        let size_note = if self.bytes_written > 0 {
            format!("  {}{}{}", color::GRAY, fmt_bytes(self.bytes_written), color::RESET)
        } else {
            String::new()
        };
        eprint!(
            "\r{}{}{}{}  {}{}{}  {}{}/{}{}{}  {}{}{}                    ",
            color::CYAN,
            pulse,
            color::RESET,
            color::BOLD,
            bar_color,
            bar,
            color::RESET,
            color::GREEN,
            self.done,
            self.total,
            color::RESET,
            size_note,
            color::GRAY,
            short,
            color::RESET,
        );
        let _ = std::io::stderr().flush();
    }

    pub fn finish(self) {
        let bytes_note = if self.bytes_written > 0 {
            format!("  {}{}{}", color::GRAY, fmt_bytes(self.bytes_written), color::RESET)
        } else {
            String::new()
        };
        eprintln!(
            "\r{}{}✔{} {}{}/{}{} files in {}{}{}                                                  ",
            color::BOLD,
            color::GREEN,
            color::RESET,
            color::BOLD,
            self.total,
            self.total,
            color::RESET,
            fmt_duration(self.started.elapsed()),
            bytes_note,
            color::RESET,
        );
    }
}

fn fmt_speed(bps: f64) -> String {
    format!("{}/s", fmt_bytes(bps as u64))
}

/// Renders a "dependency weight" table, used after Java/Maven resolution
/// so the developer sees exactly what got pulled in and how much it costs.
pub fn dependency_table(rows: &[(String, String, u64)]) {
    if rows.is_empty() {
        return;
    }
    let name_w = rows.iter().map(|r| r.0.len()).max().unwrap_or(4).max(10);
    let ver_w = rows.iter().map(|r| r.1.len()).max().unwrap_or(7).max(7);
    eprintln!(
        "  {}{:<name_w$}  {:<ver_w$}  {:>10}{}",
        color::BOLD,
        "DEPENDENCY",
        "VERSION",
        "SIZE",
        color::RESET,
        name_w = name_w,
        ver_w = ver_w
    );
    let mut total = 0u64;
    for (name, version, size) in rows {
        total += size;
        eprintln!(
            "  {:<name_w$}  {}{:<ver_w$}{}  {:>10}",
            name,
            color::GRAY,
            version,
            color::RESET,
            fmt_bytes(*size),
            name_w = name_w,
            ver_w = ver_w
        );
    }
    eprintln!(
        "  {}{:<name_w$}  {:<ver_w$}  {:>10}{}",
        color::DIM,
        "",
        "total",
        fmt_bytes(total),
        color::RESET,
        name_w = name_w,
        ver_w = ver_w
    );
}

/// Renders a build summary box at the end of a successful `rusk build`,
/// listing every ABI produced with its final `.so` size — a more
/// detailed closing screen than a single "Build complete" line.
pub fn build_summary(
    apk_path: &std::path::Path,
    apk_size: u64,
    per_abi: &[(String, u64)],
    elapsed: Duration,
) {
    let rule: String = "─".repeat(54);
    eprintln!();
    eprintln!("{}{}┌{}┐{}", color::BOLD, color::GREEN, rule, color::RESET);
    eprintln!(
        "{}{}│{}  {}✔ build succeeded{}  {}in {}{}",
        color::BOLD,
        color::GREEN,
        color::RESET,
        color::BOLD,
        color::RESET,
        color::DIM,
        fmt_duration(elapsed),
        color::RESET,
    );
    for (abi, size) in per_abi {
        eprintln!(
            "{}{}│{}    {}{:<14}{}  {}",
            color::BOLD,
            color::GREEN,
            color::RESET,
            color::GRAY,
            abi,
            color::RESET,
            fmt_bytes(*size),
        );
    }
    eprintln!(
        "{}{}│{}  {}APK{}  {} {}({}){}",
        color::BOLD,
        color::GREEN,
        color::RESET,
        color::BOLD,
        color::RESET,
        apk_path.display(),
        color::GRAY,
        fmt_bytes(apk_size),
        color::RESET,
    );
    eprintln!("{}{}└{}┘{}", color::BOLD, color::GREEN, rule, color::RESET);
}

/// A lightweight indeterminate spinner for steps with no known total
/// (e.g. "resolving latest NDK version"), distinct from [`Step`] in that
/// it re-renders in place on a timer rather than printing once at start.
pub struct Spinner {
    label: String,
    frame: usize,
    started: Instant,
}

impl Spinner {
    pub fn new(label: impl Into<String>) -> Self {
        let s = Self {
            label: label.into(),
            frame: 0,
            started: Instant::now(),
        };
        s.render();
        s
    }

    pub fn tick(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        self.render();
    }

    fn render(&self) {
        let frame = SPINNER_FRAMES[self.frame % SPINNER_FRAMES.len()];
        eprint!(
            "\r{}{}{}{} {}  {}{}{}   ",
            color::CYAN,
            frame,
            color::RESET,
            color::BOLD,
            self.label,
            color::DIM,
            fmt_duration(self.started.elapsed()),
            color::RESET,
        );
        let _ = std::io::stderr().flush();
    }

    pub fn finish_ok(self) {
        eprintln!(
            "\r{}{}✔{} {} {}({}){}                    ",
            color::BOLD,
            color::GREEN,
            color::RESET,
            self.label,
            color::GRAY,
            fmt_duration(self.started.elapsed()),
            color::RESET,
        );
    }
}
