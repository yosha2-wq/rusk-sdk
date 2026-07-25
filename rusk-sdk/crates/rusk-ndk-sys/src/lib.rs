//! rusk-ndk-sys: raw FFI declarations for the Android NDK C APIs a
//! windowing/input loop actually needs. This is the "bind to the C libs"
//! layer — every symbol here corresponds 1:1 to a function or struct in
//! the NDK headers (`android/native_activity.h`, `android/looper.h`,
//! `android/input.h`, `android/native_window.h`,
//! `android/asset_manager.h`, `android/log.h`) so a project can drive the
//! Android platform without any Java in between.
//!
//! These are deliberately *not* wrapped in a safe API — that belongs in
//! a higher-level crate built on top of this one. `rusk-ndk-sys` only
//! promises that the extern "C" signatures match what libandroid.so /
//! liblog.so actually export.
#![allow(non_camel_case_types)]

pub mod asset_manager;
pub mod input;
pub mod log;
pub mod looper;
pub mod native_activity;
pub mod native_window;

use std::os::raw::{c_char, c_int, c_void};

/// Opaque JNI types, re-declared here (rather than depending on the
/// `jni` crate) so `rusk-ndk-sys` has zero non-libc dependencies.
pub type JNIEnv = c_void;
pub type jobject = *mut c_void;
pub type jclass = jobject;

pub type c_str_ptr = *const c_char;
pub type c_int_t = c_int;
