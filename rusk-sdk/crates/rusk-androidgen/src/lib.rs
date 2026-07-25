//! rusk-androidgen: everything Rusk needs on the "Android glue" side is
//! generated in-memory from the parsed `Rusk.toml` — there are no
//! `.xml`/`.java` template files shipped with the SDK. This keeps the
//! generated app in lockstep with the manifest schema and avoids the
//! classic problem of stale boilerplate drifting from what the tool
//! actually supports.

use rusk_manifest::RuskManifest;
use std::fmt::Write as _;

/// Builds `AndroidManifest.xml` for the given project. Apps are hosted by
/// the stock `android.app.NativeActivity`, which is what lets a pure-Rust
/// crate run without any hand-written Java: the OS loads our shared
/// library directly and calls `ANativeActivity_onCreate` in it.
pub fn generate_manifest(manifest: &RuskManifest, lib_name: &str, has_code: bool) -> String {
    let mut xml = String::new();
    let _ = writeln!(xml, r#"<?xml version="1.0" encoding="utf-8"?>"#);
    let _ = writeln!(
        xml,
        r#"<manifest xmlns:android="http://schemas.android.com/apk/res/android""#
    );
    let _ = writeln!(xml, r#"    package="{}""#, xml_escape(&manifest.package.id));
    let _ = writeln!(
        xml,
        r#"    android:versionName="{}">"#,
        xml_escape(&manifest.package.version)
    );

    let _ = writeln!(
        xml,
        r#"    <uses-sdk android:minSdkVersion="{}" android:targetSdkVersion="{}" />"#,
        manifest.package.sdk_min, manifest.package.sdk_target
    );

    for perm in &manifest.permissions.list {
        let name = if perm.contains('.') {
            perm.clone()
        } else {
            format!("android.permission.{perm}")
        };
        let _ = writeln!(
            xml,
            r#"    <uses-permission android:name="{}" />"#,
            xml_escape(&name)
        );
    }

    let _ = writeln!(
        xml,
        r#"    <application android:label="{}" android:hasCode="{}" android:debuggable="{}" android:allowBackup="true">"#,
        xml_escape(&manifest.app_label()),
        if has_code { "true" } else { "false" },
        manifest.app.debuggable,
    );

    let _ = writeln!(
        xml,
        r#"        <activity android:name="android.app.NativeActivity""#
    );
    let _ = writeln!(xml, r#"            android:label="{}""#, xml_escape(&manifest.app_label()));
    let _ = writeln!(
        xml,
        r#"            android:configChanges="orientation|keyboardHidden|screenSize|uiMode""#
    );
    let _ = writeln!(
        xml,
        r#"            android:screenOrientation="{}""#,
        manifest.app.orientation.as_android_value()
    );
    if manifest.app.fullscreen {
        let _ = writeln!(
            xml,
            r#"            android:theme="@android:style/Theme.NoTitleBar.Fullscreen""#
        );
    }
    let _ = writeln!(xml, r#"            android:exported="true">"#);
    let _ = writeln!(
        xml,
        r#"            <meta-data android:name="android.app.lib_name" android:value="{lib_name}" />"#
    );
    let _ = writeln!(xml, r#"            <intent-filter>"#);
    let _ = writeln!(
        xml,
        r#"                <action android:name="android.intent.action.MAIN" />"#
    );
    let _ = writeln!(
        xml,
        r#"                <category android:name="android.intent.category.LAUNCHER" />"#
    );
    let _ = writeln!(xml, r#"            </intent-filter>"#);
    let _ = writeln!(xml, r#"        </activity>"#);
    let _ = writeln!(xml, r#"    </application>"#);
    let _ = writeln!(xml, r#"</manifest>"#);
    xml
}

/// Builds a minimal `res/values/strings.xml` containing only the values
/// the manifest actually references, generated rather than copied from a
/// fixed skeleton file.
pub fn generate_strings_xml(manifest: &RuskManifest) -> String {
    let mut xml = String::new();
    let _ = writeln!(xml, r#"<?xml version="1.0" encoding="utf-8"?>"#);
    let _ = writeln!(xml, r#"<resources>"#);
    let _ = writeln!(
        xml,
        r#"    <string name="app_name">{}</string>"#,
        xml_escape(&manifest.app_label())
    );
    let _ = writeln!(xml, r#"</resources>"#);
    xml
}

/// When the project pulls in Java dependencies, `android:hasCode` must be
/// `true` and the APK needs at least one class in its dex. Rather than
/// shipping a fixed `MainActivity.java`, generate a thin subclass of
/// `NativeActivity` on demand, named after the app id so multiple Rusk
/// projects never collide in a shared build cache.
pub fn generate_activity_shim_java(manifest: &RuskManifest) -> (String, String) {
    let class_name = "RuskShimActivity";
    let package = &manifest.package.id;
    let mut src = String::new();
    let _ = writeln!(src, "package {package};");
    let _ = writeln!(src);
    let _ = writeln!(src, "public class {class_name} extends android.app.NativeActivity {{");
    let _ = writeln!(src, "    static {{");
    let _ = writeln!(
        src,
        "        System.loadLibrary(\"{}\");",
        manifest.package.name.replace('-', "_")
    );
    let _ = writeln!(src, "    }}");
    let _ = writeln!(src, "}}");
    (class_name.to_string(), src)
}

fn xml_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Renders the scaffold `Cargo.toml` for `rusk new`. Composed from the
/// project name rather than copied from a fixed skeleton file, so it
/// always matches whatever `rusk-cli`'s current expectations are (e.g.
/// the `cdylib` crate-type NativeActivity loading depends on).
pub fn generate_cargo_toml(project_name: &str, with_bindgen: bool) -> String {
    let build_line = if with_bindgen { "build = \"build.rs\"\n" } else { "" };
    let build_deps = if with_bindgen {
        "\n[target.'cfg(target_os = \"android\")'.build-dependencies]\nbindgen = \"0.69\"\n"
    } else {
        ""
    };
    format!(
        r#"[package]
name = "{project_name}"
version = "0.1.0"
edition = "2021"
{build_line}
[lib]
crate-type = ["cdylib"]
path = "src/lib.rs"

[[bin]]
name = "{project_name}"
path = "src/main.rs"

[dependencies]
{build_deps}"#
    )
}

/// Same as [`generate_cargo_toml`], but with `[dependencies]` pointing
/// at the SDK's own `rusk-render`/`rusk-ndk-sys` crates via absolute
/// filesystem `path =` entries — used by `rusk new --with-render`, since
/// those two crates aren't published to crates.io and a generated
/// project has no other way to find them.
pub fn generate_cargo_toml_with_render(
    project_name: &str,
    with_bindgen: bool,
    render_paths: &RenderCratePathsForCargoToml,
) -> String {
    let build_line = if with_bindgen { "build = \"build.rs\"\n" } else { "" };
    let build_deps = if with_bindgen {
        "\n[target.'cfg(target_os = \"android\")'.build-dependencies]\nbindgen = \"0.69\"\n"
    } else {
        ""
    };
    // TOML basic strings only need `\` and `"` escaped; Windows paths
    // (the only realistic source of backslashes here) need the former
    // so `C:\Users\...` doesn't get misread as escape sequences.
    let render_path = escape_toml_string(&render_paths.render.to_string_lossy());
    let ndk_sys_path = escape_toml_string(&render_paths.ndk_sys.to_string_lossy());
    format!(
        r#"[package]
name = "{project_name}"
version = "0.1.0"
edition = "2021"
{build_line}
[lib]
crate-type = ["cdylib"]
path = "src/lib.rs"

[[bin]]
name = "{project_name}"
path = "src/main.rs"

[dependencies]
rusk-render = {{ path = "{render_path}" }}
rusk-ndk-sys = {{ path = "{ndk_sys_path}" }}
{build_deps}"#
    )
}

/// Filesystem paths to the `rusk-render`/`rusk-ndk-sys` crate
/// directories. Defined here (rather than importing a CLI-specific type)
/// so `rusk-androidgen` has no dependency on `rusk-cli` — the CLI
/// constructs one of these after locating the crates relative to its
/// own binary and passes it in.
pub struct RenderCratePathsForCargoToml {
    pub render: std::path::PathBuf,
    pub ndk_sys: std::path::PathBuf,
}

fn escape_toml_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Renders `src/lib.rs` for `rusk new`: the one function every
/// NativeActivity-hosted app is required to export
/// (`ANativeActivity_onCreate`), plus a single real log call through
/// `rusk-ndk-sys` so `rusk build` produces something observable in
/// `adb logcat` on first run instead of a silent no-op.
pub fn generate_lib_rs(manifest: &RuskManifest, with_bindgen: bool) -> String {
    let tag = manifest.package.name.clone();
    let bindgen_module = if with_bindgen {
        "\n#[cfg(target_os = \"android\")]\n#[allow(non_camel_case_types, non_snake_case, dead_code)]\npub mod ndk {\n    include!(concat!(env!(\"OUT_DIR\"), \"/ndk_bindings.rs\"));\n}\n"
    } else {
        ""
    };
    format!(
        r#"//! Entry point loaded by android.app.NativeActivity.
//!
//! The Android runtime `dlopen`s this library (named via the
//! `android.app.lib_name` meta-data Rusk generates in
//! AndroidManifest.xml) and calls `ANativeActivity_onCreate` directly —
//! there is no Java code in between unless [dependencies.java] or a
//! `Java_...` JNI export pulls some in.
#![cfg(target_os = "android")]

use std::os::raw::c_void;
{bindgen_module}
#[repr(C)]
pub struct ANativeActivity {{
    _private: [u8; 0],
}}

#[no_mangle]
pub extern "C" fn ANativeActivity_onCreate(
    _activity: *mut ANativeActivity,
    _saved_state: *mut c_void,
    _saved_state_size: usize,
) {{
    // `rusk-ndk-sys` wraps `__android_log_write` for exactly this: a
    // real, working log line the moment the app starts, visible with
    // `adb logcat -s {tag}`. Add rusk-ndk-sys to [dependencies] to use
    // it — see crates/rusk-ndk-sys in the Rusk SDK repository for the
    // rest of the native_window / looper / input / asset_manager API.
    //
    // rusk_ndk_sys::log::log_info("{tag}", "ANativeActivity_onCreate");
    //
    // Or, with the `ndk` module above (generated straight from the real
    // NDK sysroot headers via bindgen):
    //
    // #[cfg(target_os = "android")]
    // unsafe {{ ndk::__android_log_write(4, c"{tag}".as_ptr(), c"onCreate".as_ptr()); }}

    // A real app takes it from here: ALooper_prepare + AInputQueue to
    // read events, ANativeWindow_lock to draw, or a windowing crate
    // (winit + android-activity) built on top of the same NDK contract.
}}
"#
    )
}

/// Alternative to [`generate_lib_rs`] for `rusk new --with-render`: a
/// `src/lib.rs` that actually draws something (a solid clear color plus
/// a moving marker rectangle) instead of the bare `onCreate` stub, by
/// building on `rusk-render::AppShell`. This is the direct fix for the
/// "black screen" problem — [`generate_lib_rs`]'s minimal stub is
/// intentionally correct-but-empty (the right default for a project
/// about to pull in its own windowing/game crate), which is exactly what
/// produces an undefined/black window if left as-is.
pub fn generate_lib_rs_with_render(manifest: &RuskManifest) -> String {
    let tag = manifest.package.name.clone();
    format!(
        r#"//! Entry point loaded by android.app.NativeActivity, wired up with
//! `rusk-render::AppShell` so the app draws real pixels immediately
//! instead of showing a black screen — see the `rusk-render` crate docs
//! for why a bare NativeActivity is black by default and what this
//! crate's `AppShell` does about it.
#![cfg(target_os = "android")]

use rusk_render::{{AppShell, FrameBuffer}};
use rusk_ndk_sys::native_activity::ANativeActivity;
use std::os::raw::c_void;

#[no_mangle]
pub extern "C" fn ANativeActivity_onCreate(
    activity: *mut ANativeActivity,
    _saved_state: *mut c_void,
    _saved_state_size: usize,
) {{
    rusk_ndk_sys::log::log_info("{tag}", "starting render loop");

    // SAFETY: `activity` is exactly the pointer Android just handed us;
    // this is the one place that contract is guaranteed to hold.
    let reason = unsafe {{
        AppShell::run(activity, |fb: &mut FrameBuffer, dt_seconds: f32| {{
            draw_frame(fb, dt_seconds);
            true // keep running; return false here to exit cleanly
        }})
    }};
    rusk_ndk_sys::log::log_info("{tag}", &format!("render loop exited: {{reason:?}}"));
}}

/// Replace this with your actual rendering. What's here is deliberately
/// visible and animated (not just a static clear color) so it's
/// immediately obvious the loop is really running at whatever frame rate
/// the device delivers, not just drawn once.
struct DemoState {{
    elapsed: f32,
}}

thread_local! {{
    static STATE: std::cell::RefCell<DemoState> = std::cell::RefCell::new(DemoState {{ elapsed: 0.0 }});
}}

fn draw_frame(fb: &mut FrameBuffer, dt: f32) {{
    STATE.with(|state| {{
        let mut state = state.borrow_mut();
        state.elapsed += dt;

        // A dark blue-gray background — deliberately not pure black, so
        // "the screen is some solid color" vs "the screen is black
        // because nothing drew" are visually distinguishable at a
        // glance while you're debugging.
        fb.clear(0x14161Aff);

        // A bright square that sweeps left-to-right and wraps, proving
        // both that drawing works and that dt-based animation is live.
        let w = fb.width();
        let h = fb.height();
        let size = (w.min(h) / 8).max(8);
        let period_seconds = 3.0;
        let t = (state.elapsed % period_seconds) / period_seconds;
        let x = ((w - size) as f32 * t) as i32;
        let y = (h - size) / 2;
        for py in y..(y + size).min(h) {{
            for px in x..(x + size).min(w) {{
                fb.put_pixel(px, py, 0xE0C34Cff);
            }}
        }}
    }});
}}
"#
    )
}

/// Renders `build.rs` for `rusk new` when real NDK headers should be
/// bound with `bindgen` instead of (or alongside) the hand-written
/// `rusk-ndk-sys` crate. This is what makes the bindings come from the
/// actual `<android/*.h>` headers shipped inside the NDK `rusk-ndk`
/// already downloaded to `~/.rusk/ndk/...` — not a second, separately
/// fetched copy of anything. `ANDROID_NDK_HOME` is set by `rusk build`
/// itself (see `rusk-build::compile_for_target`), so this only needs to
/// read that env var; it does no downloading of its own.
///
/// Skipped entirely on non-Android host builds (`cargo check`/`cargo
/// test` on the desktop) so the crate still builds without an NDK
/// present — bindgen (and the libclang it drives) is only invoked when
/// actually cross-compiling for Android.
pub fn generate_build_rs() -> String {
    r#"//! Generated by `rusk new`. Binds the real Android NDK C headers
//! with `bindgen` at build time -- see crates/rusk-ndk-sys in the Rusk
//! SDK repo for a hand-written, dependency-free alternative covering
//! the same APIs if you'd rather not pull in bindgen + libclang.

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "android" {
        // Host-side `cargo check` / `cargo test` doesn't need Android
        // bindings, and won't have an NDK to bind against anyway.
        return;
    }

    let ndk_home = std::env::var("ANDROID_NDK_HOME").expect(
        "ANDROID_NDK_HOME is not set. Build through `rusk build`, which points this at \
         the NDK it manages under ~/.rusk/ndk/... automatically.",
    );

    let host_tag = if cfg!(target_os = "windows") {
        "windows-x86_64"
    } else if cfg!(target_os = "macos") {
        "darwin-x86_64"
    } else {
        "linux-x86_64"
    };

    let sysroot = std::path::Path::new(&ndk_home)
        .join("toolchains")
        .join("llvm")
        .join("prebuilt")
        .join(host_tag)
        .join("sysroot");
    let include_dir = sysroot.join("usr").join("include");

    let headers = [
        "android/native_activity.h",
        "android/native_window.h",
        "android/looper.h",
        "android/input.h",
        "android/asset_manager.h",
        "android/log.h",
    ];

    let mut builder = bindgen::Builder::default()
        .clang_arg(format!("--sysroot={}", sysroot.display()))
        .clang_arg(format!("-I{}", include_dir.display()))
        // Only the symbols an app loop actually needs -- an unfiltered
        // bind of the whole sysroot pulls in far more than this project
        // uses and slows every build down for no benefit.
        .allowlist_function("ANativeActivity_.*")
        .allowlist_function("ANativeWindow_.*")
        .allowlist_function("ALooper_.*")
        .allowlist_function("AInputQueue_.*")
        .allowlist_function("AInputEvent_.*")
        .allowlist_function("AMotionEvent_.*")
        .allowlist_function("AKeyEvent_.*")
        .allowlist_function("AAssetManager_.*")
        .allowlist_function("AAsset_.*")
        .allowlist_function("__android_log_.*")
        .allowlist_type("ANativeActivity.*")
        .allowlist_type("ARect")
        .derive_default(true);

    for header in headers {
        let path = include_dir.join(header);
        builder = builder.header(path.to_string_lossy().into_owned());
    }

    let bindings = builder
        .generate()
        .expect(
            "bindgen failed to generate NDK bindings -- is libclang installed? \
             (Debian/Ubuntu: `apt install libclang-dev`; Windows: install LLVM \
             and set LIBCLANG_PATH to its `bin` directory; macOS: `xcode-select --install`)",
        );

    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_dir.join("ndk_bindings.rs"))
        .expect("failed to write generated NDK bindings");
}
"#
    .to_string()
}
