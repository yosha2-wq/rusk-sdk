//! Bindings for `<android/looper.h>` — the event loop primitive a
//! NativeActivity app polls to receive input queue and lifecycle events
//! without a Java `Looper`/`Handler` anywhere in the picture.

use std::os::raw::{c_int, c_void};

#[repr(C)]
pub struct ALooper {
    _private: [u8; 0],
}

pub const ALOOPER_PREPARE_ALLOW_NON_CALLBACKS: c_int = 1 << 0;

pub const ALOOPER_POLL_WAKE: c_int = -1;
pub const ALOOPER_POLL_CALLBACK: c_int = -2;
pub const ALOOPER_POLL_TIMEOUT: c_int = -3;
pub const ALOOPER_POLL_ERROR: c_int = -4;

pub const ALOOPER_EVENT_INPUT: c_int = 1 << 0;
pub const ALOOPER_EVENT_OUTPUT: c_int = 1 << 1;
pub const ALOOPER_EVENT_ERROR: c_int = 1 << 2;
pub const ALOOPER_EVENT_HANGUP: c_int = 1 << 3;
pub const ALOOPER_EVENT_INVALID: c_int = 1 << 4;

pub type ALooper_callbackFunc =
    extern "C" fn(fd: c_int, events: c_int, data: *mut c_void) -> c_int;

extern "C" {
    pub fn ALooper_forThread() -> *mut ALooper;
    pub fn ALooper_prepare(opts: c_int) -> *mut ALooper;
    pub fn ALooper_acquire(looper: *mut ALooper);
    pub fn ALooper_release(looper: *mut ALooper);
    pub fn ALooper_pollOnce(
        timeout_millis: c_int,
        out_fd: *mut c_int,
        out_events: *mut c_int,
        out_data: *mut *mut c_void,
    ) -> c_int;
    pub fn ALooper_pollAll(
        timeout_millis: c_int,
        out_fd: *mut c_int,
        out_events: *mut c_int,
        out_data: *mut *mut c_void,
    ) -> c_int;
    pub fn ALooper_wake(looper: *mut ALooper);
    pub fn ALooper_addFd(
        looper: *mut ALooper,
        fd: c_int,
        ident: c_int,
        events: c_int,
        callback: Option<ALooper_callbackFunc>,
        data: *mut c_void,
    ) -> c_int;
    pub fn ALooper_removeFd(looper: *mut ALooper, fd: c_int) -> c_int;
}
