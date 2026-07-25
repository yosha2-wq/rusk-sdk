//! Bindings for `<android/asset_manager.h>` — reads files packed under
//! `assets/` inside the APK without going through Java's `AssetManager`.

use std::os::raw::{c_char, c_int, c_void};

#[repr(C)]
pub struct AAssetManager {
    _private: [u8; 0],
}

#[repr(C)]
pub struct AAsset {
    _private: [u8; 0],
}

#[repr(C)]
pub struct AAssetDir {
    _private: [u8; 0],
}

pub const AASSET_MODE_UNKNOWN: c_int = 0;
pub const AASSET_MODE_RANDOM: c_int = 1;
pub const AASSET_MODE_STREAMING: c_int = 2;
pub const AASSET_MODE_BUFFER: c_int = 3;

extern "C" {
    pub fn AAssetManager_openDir(mgr: *mut AAssetManager, dir_name: *const c_char) -> *mut AAssetDir;
    pub fn AAssetManager_open(
        mgr: *mut AAssetManager,
        filename: *const c_char,
        mode: c_int,
    ) -> *mut AAsset;

    pub fn AAssetDir_getNextFileName(assetDir: *mut AAssetDir) -> *const c_char;
    pub fn AAssetDir_rewind(assetDir: *mut AAssetDir);
    pub fn AAssetDir_close(assetDir: *mut AAssetDir);

    pub fn AAsset_read(asset: *mut AAsset, buf: *mut c_void, count: usize) -> c_int;
    pub fn AAsset_seek(asset: *mut AAsset, offset: i64, whence: c_int) -> i64;
    pub fn AAsset_close(asset: *mut AAsset);
    pub fn AAsset_getLength(asset: *mut AAsset) -> i64;
    pub fn AAsset_getRemainingLength(asset: *mut AAsset) -> i64;
    pub fn AAsset_getBuffer(asset: *mut AAsset) -> *const c_void;
    pub fn AAsset_isAllocated(asset: *mut AAsset) -> c_int;
}

/// Safe-ish convenience: reads an entire asset into a `Vec<u8>` in one
/// call. Still `unsafe` because it dereferences the raw `AAssetManager`
/// pointer handed to `ANativeActivity_onCreate`.
///
/// # Safety
/// `mgr` must be a valid, non-null `AAssetManager*` obtained from
/// `ANativeActivity.asset_manager`.
pub unsafe fn read_asset(mgr: *mut AAssetManager, path: &str) -> Option<Vec<u8>> {
    use std::ffi::CString;
    let c_path = CString::new(path).ok()?;
    let asset = AAssetManager_open(mgr, c_path.as_ptr(), AASSET_MODE_BUFFER);
    if asset.is_null() {
        return None;
    }
    let len = AAsset_getLength(asset) as usize;
    let mut buf = vec![0u8; len];
    let read = AAsset_read(asset, buf.as_mut_ptr() as *mut c_void, len);
    AAsset_close(asset);
    if read < 0 {
        None
    } else {
        buf.truncate(read as usize);
        Some(buf)
    }
}
