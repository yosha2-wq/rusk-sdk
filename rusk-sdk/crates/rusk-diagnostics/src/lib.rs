//! rusk-diagnostics: gets useful crash information out of a release
//! build running on an actual device, which the standard library alone
//! does not reliably give you on Android.
//!
//! # Why this crate exists
//!
//! `std::backtrace::Backtrace` does not produce a symbolized stack on
//! Android even when debug info is present in the binary — this is a
//! confirmed upstream gap (rust-lang/rust#121033), not something a
//! project's own `Cargo.toml` settings can work around. The external
//! `backtrace` crate does not have this gap, so this crate installs a
//! panic hook built on `backtrace` + `log-panics` instead of relying on
//! the standard library's own (broken, on this platform) mechanism.
//!
//! Separately, a panicking Rust thread on Android writes to `stderr`,
//! which is easy to lose entirely — `stderr` isn't connected to
//! `logcat` the way `stdout`/`println!` output is on desktop, so a
//! panic can vanish with no trace unless something explicitly routes it
//! through `__android_log_write`. This crate does that too.
//!
//! This is purely additive: nothing else in the Rusk SDK depends on or
//! is affected by this crate. A project opts in explicitly by calling
//! [`init`] once, early in `ANativeActivity_onCreate` (or
//! `android-activity`'s `android_on_create`, if a project uses that
//! entry point instead of `rusk-render`'s).

pub use log; // re-exported so a project only needs one dependency to get
             // both the logging macros and this crate's setup function.

/// Installs both pieces of Android crash diagnostics in one call:
/// - `android_logger`, so `log::info!`/`log::error!`/etc. reach `adb
///   logcat` under the given tag, exactly like `rusk-ndk-sys::log`
///   already does for its own hand-written calls — this crate doesn't
///   replace that, it gives the same destination to the wider `log`
///   ecosystem (any dependency that logs through the `log` facade
///   becomes visible in logcat too, not just this project's own code).
/// - a panic hook that captures a real, symbolized backtrace via the
///   `backtrace` crate and logs it through the same `log` sink, so a
///   panic on-device produces something actually useful in `adb logcat`
///   instead of a bare message with no stack.
///
/// Safe to call multiple times — `android_logger::init_once` and the
/// panic hook installation are both idempotent; only the first call
/// takes effect, later calls are silently ignored, matching
/// `android_logger`'s own documented behavior.
pub fn init(tag: &str) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_tag(tag)
            .with_max_level(log::LevelFilter::Trace),
    );
    // The `log` crate's own static filter defaults to a level that can
    // silently drop messages before they ever reach android_logger;
    // Android's own logcat has its own level filtering, so the most
    // useful default is to pass everything down and let logcat (and the
    // developer's own `adb logcat -v` filters) decide what to show.
    log::set_max_level(log::LevelFilter::Trace);

    log_panics::Config::new()
        .backtrace_mode(log_panics::BacktraceMode::Resolved)
        .install_panic_hook();
}

/// Manually captures and logs the current call stack without a panic —
/// useful for logging "how did we get here" context around a recoverable
/// error path, not just fatal ones. Uses the same `backtrace` crate the
/// panic hook does, so the output is symbolized the same way.
pub fn log_current_backtrace(tag: &str, context: &str) {
    let bt = backtrace::Backtrace::new();
    log::error!(target: "rusk-diagnostics", "{tag}: {context}\n{bt:?}");
}

/// Sets a custom panic message prefix logged before the backtrace on
/// every panic — useful for a project that wants every crash report to
/// start with, e.g., its own version string or build identifier, without
/// hand-rolling a second panic hook on top of this crate's.
pub fn init_with_prefix(tag: &str, prefix: impl Into<String> + Send + Sync + 'static) {
    init(tag);
    let prefix = prefix.into();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!(target: "rusk-diagnostics", "{prefix}");
        previous(info);
    }));
}
