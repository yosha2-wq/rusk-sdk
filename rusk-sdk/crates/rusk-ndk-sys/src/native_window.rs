//! Bindings for `<android/native_window.h>` (`libandroid.so`) — the
//! surface a renderer (GL/Vulkan/software) actually draws into.

use std::os::raw::c_void;

#[repr(C)]
pub struct ANativeWindow {
    _private: [u8; 0],
}

#[repr(C)]
pub struct ANativeWindow_Buffer {
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub format: i32,
    pub bits: *mut c_void,
    pub reserved: [u32; 6],
}

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowFormat {
    Rgba8888 = 1,
    Rgbx8888 = 2,
    Rgb565 = 4,
}

extern "C" {
    pub fn ANativeWindow_acquire(window: *mut ANativeWindow);
    pub fn ANativeWindow_release(window: *mut ANativeWindow);
    pub fn ANativeWindow_getWidth(window: *mut ANativeWindow) -> i32;
    pub fn ANativeWindow_getHeight(window: *mut ANativeWindow) -> i32;
    pub fn ANativeWindow_getFormat(window: *mut ANativeWindow) -> i32;
    pub fn ANativeWindow_setBuffersGeometry(
        window: *mut ANativeWindow,
        width: i32,
        height: i32,
        format: i32,
    ) -> i32;
    pub fn ANativeWindow_lock(
        window: *mut ANativeWindow,
        out_buffer: *mut ANativeWindow_Buffer,
        in_out_dirty_bounds: *mut super::native_activity::ARect,
    ) -> i32;
    pub fn ANativeWindow_unlockAndPost(window: *mut ANativeWindow) -> i32;
}
