//! Bindings for `<android/native_activity.h>`. This is the struct the
//! Android runtime hands to `ANativeActivity_onCreate` — the function
//! `rusk new` wires up as the library's entry point (see
//! `rusk-androidgen`'s manifest generator, which points
//! `android:hasCode` / `android.app.lib_name` at this exact contract).

use crate::jobject;
use std::os::raw::{c_char, c_void};

#[repr(C)]
pub struct ANativeActivityCallbacks {
    pub on_start: extern "C" fn(activity: *mut ANativeActivity),
    pub on_resume: extern "C" fn(activity: *mut ANativeActivity),
    pub on_save_instance_state:
        extern "C" fn(activity: *mut ANativeActivity, out_len: *mut usize) -> *mut c_void,
    pub on_pause: extern "C" fn(activity: *mut ANativeActivity),
    pub on_stop: extern "C" fn(activity: *mut ANativeActivity),
    pub on_destroy: extern "C" fn(activity: *mut ANativeActivity),
    pub on_window_focus_changed: extern "C" fn(activity: *mut ANativeActivity, has_focus: c_int),
    pub on_native_window_created:
        extern "C" fn(activity: *mut ANativeActivity, window: *mut c_void),
    pub on_native_window_resized:
        extern "C" fn(activity: *mut ANativeActivity, window: *mut c_void),
    pub on_native_window_redraw_needed:
        extern "C" fn(activity: *mut ANativeActivity, window: *mut c_void),
    pub on_native_window_destroyed:
        extern "C" fn(activity: *mut ANativeActivity, window: *mut c_void),
    pub on_input_queue_created:
        extern "C" fn(activity: *mut ANativeActivity, queue: *mut c_void),
    pub on_input_queue_destroyed:
        extern "C" fn(activity: *mut ANativeActivity, queue: *mut c_void),
    pub on_content_rect_changed: extern "C" fn(activity: *mut ANativeActivity, rect: *const ARect),
    pub on_configuration_changed: extern "C" fn(activity: *mut ANativeActivity),
    pub on_low_memory: extern "C" fn(activity: *mut ANativeActivity),
}

use std::os::raw::c_int;

#[repr(C)]
pub struct ARect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// Mirrors the layout of the real `ANativeActivity` struct exactly — the
/// runtime writes into `callbacks`/`instance` and reads `env`/`clazz`
/// directly, so field order and size must match the NDK header bit for
/// bit. Only the fields a typical app loop actually touches are exposed
/// here; the rest are kept as opaque padding via `_reserved`.
#[repr(C)]
pub struct ANativeActivity {
    pub callbacks: *mut ANativeActivityCallbacks,
    pub vm: *mut c_void,
    pub env: *mut crate::JNIEnv,
    pub clazz: jobject,
    pub internal_data_path: *const c_char,
    pub external_data_path: *const c_char,
    pub sdk_version: i32,
    pub instance: *mut c_void,
    pub asset_manager: *mut crate::asset_manager::AAssetManager,
    pub obb_path: *const c_char,
}

extern "C" {
    /// The single symbol the OS looks up by name (`ANativeActivity_onCreate`)
    /// after `dlopen`-ing the library named in the manifest's
    /// `android.app.lib_name` meta-data. A project must define exactly one
    /// `#[no_mangle] extern "C" fn ANativeActivity_onCreate` — `rusk new`
    /// generates the skeleton for it.
    pub fn ANativeActivity_finish(activity: *mut ANativeActivity);
    pub fn ANativeActivity_setWindowFormat(activity: *mut ANativeActivity, format: i32);
    pub fn ANativeActivity_setWindowFlags(
        activity: *mut ANativeActivity,
        add_flags: u32,
        remove_flags: u32,
    );
    pub fn ANativeActivity_showSoftInput(activity: *mut ANativeActivity, flags: u32);
    pub fn ANativeActivity_hideSoftInput(activity: *mut ANativeActivity, flags: u32);
}
