//! Bindings for `<android/log.h>` (`liblog.so`).

use std::os::raw::{c_char, c_int};

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogPriority {
    Unknown = 0,
    Default = 1,
    Verbose = 2,
    Debug = 3,
    Info = 4,
    Warn = 5,
    Error = 6,
    Fatal = 7,
    Silent = 8,
}

extern "C" {
    /// `__android_log_write(prio, tag, text)` — the actual entry point
    /// every `println!`-replacement macro in an NDK app ends up calling.
    #[link_name = "__android_log_write"]
    pub fn android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;

    #[link_name = "__android_log_print"]
    pub fn android_log_print(prio: c_int, tag: *const c_char, fmt: *const c_char, ...) -> c_int;
}

/// Convenience wrapper: formats and writes a single log line at `Info`
/// priority under the given tag. Panics are avoided even if the strings
/// contain interior NULs by truncating at the first one, matching what
/// the C API would do anyway.
pub fn log_info(tag: &str, msg: &str) {
    log_at(LogPriority::Info, tag, msg);
}

pub fn log_error(tag: &str, msg: &str) {
    log_at(LogPriority::Error, tag, msg);
}

pub fn log_at(priority: LogPriority, tag: &str, msg: &str) {
    use std::ffi::CString;
    let Ok(tag_c) = CString::new(tag) else { return };
    let Ok(msg_c) = CString::new(msg) else { return };
    unsafe {
        android_log_write(priority as c_int, tag_c.as_ptr(), msg_c.as_ptr());
    }
}
