# Rusk — a Rust-to-Android SDK

Rusk is a `cargo`-adjacent toolchain for building Android APKs directly
from a Rust crate. `Rusk.toml` describes the app; `rusk build` translates
that into the concrete `cargo` / NDK-clang / `aapt2` / `d8` / `zipalign` /
`apksigner` invocations needed to produce a signed, installable APK — no
Gradle, no `AndroidManifest.xml` template file, no hand-maintained Java
project.

## What changed since the first cut

- **`rustup target add <triple>` runs automatically** before every
  cross-compile. The single most common first-run error with any
  Android-from-Rust setup is `error[E0463]: can't find crate for` std``
  because the target's prebuilt std was never installed — `rusk build`
  no longer requires you to do that by hand.
- **No `ANDROID_HOME` required at all.** `rusk-sdkmgr` reads Google's own
  package repository manifest (the same one `sdkmanager` uses) and
  downloads exactly `platform-tools` + `build-tools;<version>` +
  `platforms;android-<api>` into `~/.rusk/android-sdk` the first time
  they're needed. `ANDROID_HOME` is still honored if it's set.
- **AAR dependencies, not just plain jars.** `[dependencies.java]`
  resolves `.aar` artifacts (most of androidx is AARs, not jars),
  extracts `classes.jar` for the classpath and `jni/<abi>/*.so` straight
  into the APK next to the libraries `rustc` produced.
- **`rusk-jnigen`** scans your Rust source for
  `extern "system" fn Java_<package>_<Class>_<method>` exports and
  generates the matching Java `native` method declarations automatically
  — the Rust side and the Java side of a JNI binding are written once,
  not twice.
- **`rusk-ndk-sys`** is a standalone crate of hand-written FFI bindings
  to the core NDK C APIs (`native_activity.h`, `native_window.h`,
  `looper.h`, `input.h`, `asset_manager.h`, `log.h`) for projects that
  want to talk to the platform directly without pulling in `bindgen`.
- **`rusk new` also generates a real `build.rs`** (unless run with
  `--no-bindgen`) that runs `bindgen` against the *actual* NDK sysroot
  headers under the NDK `rusk-ndk` already downloaded — so the bindings
  it produces are exactly whatever headers ship in the pinned NDK
  version, not a hand-transcribed copy of them. This needs `libclang` on
  the host (`apt install libclang-dev` / install LLVM on Windows /
  `xcode-select --install` on macOS) — `rusk doctor` checks for it.
- **`[[flavor]]` build variants** in `Rusk.toml` — `rusk build --flavor
  pro` overlays an id suffix, extra Cargo features, a different ABI list
  or extra Java deps onto the base manifest without duplicating it.
- **Per-file extraction progress.** Unpacking the NDK or an SDK component
  now shows which file inside the archive is being written and how many
  are left, not just a single opaque "Unpacking..." line.

## Why bindgen needs LLVM (closing the loop on "do we need LLVM?")

Two different things both happen to involve LLVM, and it's worth being
precise about which is which:

1. **Cross-compiling your Rust code** uses the clang/LLVM that's already
   bundled inside the Android NDK (`toolchains/llvm/prebuilt/<host>/`).
   `rusk-ndk` points `cargo` at that directly — nothing extra to install.
2. **`bindgen`** (used by the generated `build.rs`, optional) is a
   different consumer of LLVM: it links against `libclang` on your *host
   machine* to parse the NDK's C headers and emit matching Rust
   declarations. That's a separate, standard LLVM/Clang install
   (`libclang-dev`, an LLVM release, or Xcode command line tools) — it
   has nothing to do with the NDK's bundled copy, and you only need it if
   you keep the default `rusk new` behavior instead of passing
   `--no-bindgen`.

## Workspace layout (updated for 0.2.4)

```
crates/
  rusk-cli/          the `rusk` binary — new / build / run / doctor /
                       emulator / lint / keystore / bundle / cache / clean
  rusk-manifest/     Rusk.toml schema, parsing, validation, flavors
  rusk-ndk/          NDK discovery + on-demand download/unpack
  rusk-sdkmgr/       auto-installs platform-tools/build-tools/platform,
                      plus cmdline-tools/emulator/system-images for local
                      AVDs, from Google's package repository manifest —
                      every archive it downloads is SHA-1 verified
  rusk-ndk-sys/      hand-written FFI bindings to core NDK C APIs
  rusk-androidgen/   AndroidManifest.xml / strings.xml / activity shim /
                      Cargo.toml / lib.rs / build.rs — generated in
                      memory, no static template files
  rusk-jnigen/       scans Rust source for JNI exports, generates the
                      matching Java native method declarations
  rusk-javadeps/     Maven dependency + AAR resolution and fetching
  rusk-apk/          aapt2/d8/zipalign/apksigner orchestration + signing
  rusk-build/        ties the above into the end-to-end build pipeline
  rusk-keystore/     release keystore generation, inspection, cert
                      export, apksigner verification
  rusk-proguard/     R8/ProGuard keep-rule generation from actual JNI
                      exports (not guessed), R8 minification
  rusk-bundle/       Android App Bundle (.aab) via bundletool, for Play
                      Store upload — separate from the sideload-ready APK
  rusk-lint/         static Rusk.toml checks: SDK version sanity,
                      sensitive permissions, debug-signed release builds,
                      com.example.* namespace, unpinned dependencies
  rusk-symbolize/    turns a stripped-release native crash backtrace back
                      into function names + source lines via the NDK's
                      llvm-addr2line
  rusk-profile/      on-device CPU profiling via the NDK's bundled
                      simpleperf — no Android Studio Profiler needed
  rusk-workspace/    multi-project support (Rusk.workspace.toml) for
                      repos with more than one Rusk app
  rusk-cache/        Rusk.lock (committed, reproducibility record) +
                      Rusk.downloadcache (local-only download/build log)
                      + a compressed, non-encrypted rusk.lock.cache.xz
                      snapshot of both, under ~/.rusk/cache/<project>/
  rusk-render/       window lifecycle tracking (window/input-queue
                      create/resize/destroy) + a software framebuffer
                      render loop — the fix for a black screen on a bare
                      NativeActivity, without requiring a GPU API
  rusk-ui/           terminal UI — gradient progress bars with live ETA,
                      a braille phase spinner, nested step trees, a
                      compiler banner, and a build summary box — shared
                      by every crate
```

Each crate is independently usable as a library — `rusk-cli` is a thin
wrapper around them, not where the logic lives.

## External-crate integrations (0.2.5)

A few more crates joined the workspace, each wrapping a well-established
external dependency rather than reimplementing it:

```
rusk-diagnostics/  panic → logcat bridge with real symbolized backtraces
                    (std::backtrace doesn't work reliably on Android —
                    this uses the `backtrace` + `log-panics` crates
                    instead), plus a `log` → __android_log_write bridge
                    via `android_logger`
rusk-jni-safe/      safe wrappers over the `jni` crate: pending-exception
                    checking, method calls without hand-written JNI type
                    signatures — a complement to rusk-jnigen's raw
                    extern "C" generation, not a replacement for it
rusk-embed-jvm/     wraps `j4rs` to call an existing, unmodified Java/
                    Maven library from Rust by embedding a real JVM —
                    for using a library as-is, not for converting Rust
                    into Java (see "Why there's no Rust-to-Java
                    translator" below)
rusk-tls/           a `rustls`-based TLS client, with the Android-
                    specific `rustls-platform-verifier` init call wired
                    in automatically (skipping it panics on the first
                    connection, not gracefully) — used instead of the
                    NDK's own bundled OpenSSL 1.1.1, which is EOL
rusk-sqlite/        `rusqlite` with its `bundled` SQLite build (compiled
                     from C source per-ABI during `cargo build`, so no
                     prebuilt native library to manage), plus an
                     app-data-directory helper and a small dependency-
                     free schema-migration runner
```

### Why there's no Rust-to-Java translator

There is no general-purpose source-to-source translator between Rust and
Java in this SDK, and there isn't one anywhere else either — this was
checked, not assumed. Academic work studying the reverse direction
(translating *to* Rust, which is the easier direction since Rust's rules
are stricter) has found that rule-based transpilation doesn't scale
across the semantic gap between a garbage-collected language's object
model and Rust's ownership/borrowing model. A hand-rolled translator
built quickly would not be a smaller version of that solution — it would
generate code that looks plausible and fails silently or incorrectly the
first time it hits a trait, a generic, or a closure, which is worse than
no tool at all. `rusk-jni-safe` and `rusk-embed-jvm` exist instead as
honest answers to the two real needs "I want to translate Rust to Java"
usually turns out to actually mean: calling Java from Rust safely, and
using an existing Java library without rewriting it.

### Why `android-activity`/`winit`/`egui` aren't wrapped here

These are excellent, actively maintained crates and a reasonable choice
for a Rusk project's rendering/windowing layer — but they are not
wrapped as a `rusk-*` crate the way `rusk-tls`/`rusk-sqlite` are, because
doing so would create exactly the wrong impression: `android-activity`
(and `rusk-render`'s own `AppShell`) both claim ownership of the
`ANativeActivity` callback table and the app's main event loop. A
project uses **one** of `rusk-render` or `android-activity`, never both
— they are alternatives, not composable layers, and a thin `rusk-`
wrapper around `android-activity` would misleadingly suggest otherwise.
To use `android-activity` (with `winit`/`egui` on top) instead of
`rusk-render`, add it directly as a normal dependency in a project's own
`Cargo.toml` and skip `rusk new --with-render`.

## The cache system: Rusk.lock vs Rusk.downloadcache

These are two deliberately different files with two different jobs:

- **`Rusk.lock`** lives at the project root next to `Rusk.toml` and is
  meant to be committed to version control, the same way `Cargo.lock`
  is. It records the exact NDK version, Android build-tools version,
  platform API level, and fully-resolved Java/Maven dependency versions
  a successful build used — so a second machine (or the same machine a
  year later, after Google has shipped newer NDK releases) building the
  same `Rusk.toml` gets the same build.
- **`Rusk.downloadcache`** is local-only and is never committed (`rusk
  new` writes it into `.gitignore` automatically). It's an append-only
  log of every artifact Rusk has downloaded (NDK, SDK components, Java
  jars, bundletool — with size, source, and timestamp) and every
  compiler/toolchain invocation Rusk has run (`cargo build` per ABI,
  with duration and success/failure, and a truncated error tail on
  failure) — a complete local history of what your last several builds
  actually did, without needing to capture `--verbose` output by hand.

Both are mirrored into `~/.rusk/cache/<project-fingerprint>/` (keyed by a
hash of the project's canonical path), alongside a compressed —
deliberately **not encrypted** — snapshot of both together,
`rusk.lock.cache.xz`, refreshed on every successful build. It's plain XZ
specifically so it can be inspected with any standard `xz`/`unxz` tool
without a passphrase; none of the data in it (version numbers, download
sizes, build timings) is sensitive, so encryption would only get in the
way of eyeballing it after copying it to another machine.

```
rusk cache stats             # total cached bytes, per-tool build health
rusk cache inspect           # print the full Rusk.downloadcache log
rusk cache inspect-snapshot  # decompress and print rusk.lock.cache.xz
rusk cache clear             # delete the local cache dir (Rusk.lock at
                               # the project root is untouched)
```

## Why a fresh project shows a black screen, and how to fix it

`android.app.NativeActivity` (what `rusk new` targets by default) hands
your code a native window through `onNativeWindowCreated` and then does
*nothing else* — no swapchain, no draw loop, no clear color. A minimal
`ANativeActivity_onCreate` that only implements the required callback
contract genuinely produces an undefined/black window. This is not a
Rust-specific gap, and it isn't something a "Rust-to-Java translator"
would fix either — Java's own NativeActivity equivalent has exactly the
same empty-by-default behavior. What's missing is a render loop, not a
different source language.

`rusk-render`'s `AppShell` is the fix:

```
rusk new my_app --with-render
cd my_app
rusk build && rusk run
```

This scaffolds an `ANativeActivity_onCreate` that hands control to
`AppShell::run`, which correctly tracks the window/input-queue lifecycle
(the resize/destroy half of that contract is the other common source of
an intermittently-black screen after a device rotation, not just the
creation half) and calls your `draw` closure once per frame with a
locked, safe `FrameBuffer` to write RGBA8888 pixels into. The generated
demo draws a moving square on a dark background — proof the loop is
really running at whatever frame rate the device delivers, not a static
image.

This is deliberately CPU/software rendering — no GPU driver dependency,
identical behavior on every device and emulator. For GPU-accelerated
rendering, `AppShell::native_window()` exposes the same lifecycle-tracked
`*mut ANativeWindow` pointer for handing to `wgpu`'s
`raw_window_handle::RawWindowHandle::AndroidNdk` or `glutin`'s Android
EGL surface constructor instead — the window lifecycle tracking is the
part that's easy to get subtly wrong and is shared by both paths.

`--with-render` generates `path = "..."` dependencies pointing at the
SDK's own `rusk-render`/`rusk-ndk-sys` crates (not published to
crates.io), resolved relative to the running `rusk` binary's own
location — this only works when `rusk` is run from inside (or still next
to) the SDK source checkout it was built from.

## `Rusk.toml`

```toml
[package]
name = "my_app"
id = "com.example.my_app"      # reverse-DNS Android application id
version = "0.1.0"
entry = "src/main.rs"           # host-side entry point (cargo run target)
sdk_min = 24
sdk_target = 34
sdk_compile = 34

[app]
label = "My App"
orientation = "unspecified"     # unspecified | portrait | landscape | sensor
fullscreen = false
debuggable = true

[abi]
targets = ["arm64-v8a", "x86_64"]   # arm64-v8a, armeabi-v7a, x86_64, x86

[permissions]
list = ["INTERNET", "CAMERA"]        # expands to android.permission.*

[dependencies.java]
"androidx.core:core-ktx" = "1.13.1"  # pulled straight from Maven

[ndk]
version = "27.0.12077973"            # optional; latest known if omitted

[sdk]
build_tools_version = "34.0.0"       # used both for ANDROID_HOME lookup
                                       # and for rusk-sdkmgr auto-install

[signing]                             # optional; falls back to an
keystore = "release.keystore"         # auto-generated debug keystore
alias = "release"
password_env = "RUSK_KEYSTORE_PASSWORD"

[[flavor]]                            # optional named build variants
name = "pro"
id_suffix = "pro"                     # -> com.example.my_app.pro
label_suffix = "Pro"
cargo_features = ["pro"]
```

## How a build actually works

1. `rusk-ndk` makes sure the requested NDK is unpacked under
   `~/.rusk/ndk/<version>/`, downloading it from Google's distribution
   point with a byte-aware progress bar if it isn't cached yet.
2. `rusk-build` cross-compiles the crate's `cdylib` target once per ABI in
   `[abi].targets`, pointing `cargo` at the NDK's bundled `clang` via
   `CARGO_TARGET_<TRIPLE>_LINKER` / `CC_<triple>` / `CXX_<triple>`.
3. `rusk-androidgen` renders `AndroidManifest.xml` and `strings.xml` from
   the parsed manifest. The default activity is the stock
   `android.app.NativeActivity`, so a project with no
   `[dependencies.java]` needs zero hand-written Java.
4. If `[dependencies.java]` is non-empty, `rusk-javadeps` resolves the
   transitive Maven closure, downloads + caches the jars, and
   `rusk-androidgen` generates a minimal `NativeActivity` subclass so the
   APK has the `android:hasCode="true"` entry point Android requires; it
   is compiled with `javac` and dexed with `d8` alongside the fetched
   jars.
5. `rusk-apk` drives `aapt2 compile` / `aapt2 link` to produce the base
   APK, stuffs the compiled `.so` files under `lib/<abi>/`, adds
   `classes.dex` if applicable, then `zipalign`s and `apksigner sign`s the
   result — using the project's keystore if `[signing]` is set, otherwise
   an auto-generated `~/.rusk/debug.keystore`.

## Running without a physical device

`rusk emulator` provisions and drives a local AVD with no Android Studio
involved:

```
rusk emulator create [name]   # fetches cmdline-tools + emulator + a
                                # google_apis system image for the current
                                # project's package.sdk_target if they
                                # aren't cached yet, then creates the AVD
rusk emulator list             # lists AVDs already created
rusk emulator start <name>     # boots the AVD in the background
rusk run                       # once it's booted, installs + launches
                                # exactly like it would on a real device
```

`rusk run` and `rusk doctor` both resolve `adb` from
`~/.rusk/android-sdk/platform-tools` first (where `rusk build` already
provisions it), falling back to `ANDROID_HOME`/PATH — so a fresh machine
with nothing installed still gets a working `rusk run` after one
`rusk build`, with no separate "install platform-tools" step.

## Fixed: mixed path separators breaking the NDK linker on Windows

Early Windows builds could fail with a linker error report that only
appeared to be about a `--version-script=` argument, with corrupted
(wrong-codepage) text in the "note" line. The actual cause was upstream
of that argument: `toolchain_bin()` built the NDK's `clang.cmd` directory
by joining a *single string containing forward slashes*
(`.join("toolchains/llvm/prebuilt")`) onto a `PathBuf`. `PathBuf::join`
doesn't normalize separators inside a joined string, so the resulting
path mixed `/` and `\` — mostly harmless for plain file I/O, but the
NDK's `clang.cmd` batch wrapper re-parses its own argument list
(including that path) before invoking `clang.exe`, and a mixed-separator
absolute path there could make that parse step fail unpredictably.
`rusk-ndk` and `rusk-androidgen` (which builds the same path for its
`bindgen` sysroot argument) now build every multi-segment toolchain path
from individual `.join()` calls, so `PathBuf` always renders the host's
native separator.

## Why no bundled LLVM

The Android NDK already ships a full clang/LLVM toolchain
(`toolchains/llvm/prebuilt/<host>/bin/`), and that's what actually needs
to compile the `cdylib` for each ABI — `rusk-ndk` points `cargo` at it
directly. A second, separately-fetched LLVM install would just be a
second copy of the same compiler; it isn't part of this SDK.

## Requirements on the host

- A working `cargo`/`rustc`, plus `rustup` on PATH so `rusk build` can
  install missing Android targets automatically (you can add them
  yourself instead — `rustup target add aarch64-linux-android
  x86_64-linux-android ...` — if you're not using rustup for that).
- A JDK on `PATH` (`javac`, `keytool`) — used to compile the tiny
  generated activity/JNI-binding shims and to create the debug keystore.
- Android SDK build-tools + platform jar: **optional.** If `ANDROID_HOME`
  / `ANDROID_SDK_ROOT` is set and complete, Rusk uses it; otherwise
  `rusk-sdkmgr` downloads exactly what's needed into
  `~/.rusk/android-sdk` on first build.
- `adb`, `emulator`, `cmdline-tools`, and a system image: **all
  optional** — `rusk build` provisions `adb` automatically, and `rusk
  emulator create` provisions everything else on first use. Nothing here
  needs to be installed by hand.
- `libclang` — **only** if you keep the default `rusk new` bindgen
  `build.rs` (pass `--no-bindgen` to skip it and avoid this requirement).

Run `rusk doctor` to check all of the above at once.

## License

Rusk is licensed under MIT terms with an Acceptable Use Addendum — see
[`LICENSE`](LICENSE) for the full text. In short: you can use, modify,
redistribute, and build commercial or personal Android projects with
this SDK freely, the same as any MIT-licensed project. The addendum
states (without legally narrowing the MIT grant above it) that the
authors don't want this SDK or its generated code used to build malware,
stealers, or anything that tampers with another party's security
without authorization — that's a statement of intent, not an additional
legal condition on top of the permissive grant.

## Honesty about scope (0.9.7)

This is a genuinely working, from-scratch toolchain, not a wrapper around
Gradle or `cargo-apk` — but it is still early, not a mature SDK:

- Java dependency resolution reads `<dependency>` blocks straight out of
  POM XML; it does not resolve `${property}` placeholders or inherited
  parent POMs, and conflict resolution is "first version requested wins".
  Artifacts that need either will need their version pinned explicitly.
- Only `android.app.NativeActivity`-hosted apps are supported — there is
  no generator yet for a fully custom multi-Activity Java/Kotlin app.
- The NDK release table in `rusk-ndk` verifies downloads against the
  SHA-1 checksums Google's own `android/ndk` GitHub releases publish per
  platform archive (Google does not publish SHA-256 for these archives,
  so `rusk-ndk` matches what the source actually provides rather than a
  stronger-sounding algorithm with no reference value to check against).
  Adding a new NDK version to `KNOWN_RELEASES` ahead of confirming its
  published hash is still safe — an unfilled/short checksum is treated
  as "not pinned yet" and skipped with a visible warning rather than
  silently trusted. `rusk-sdkmgr`'s Android SDK component downloads
  (build-tools, platform-tools, emulator, system images) are separately
  SHA-1 verified against Google's own repository manifest, which
  publishes a checksum per archive there too.
- Incremental build caching (`rusk-incremental`) is wired into
  `rusk-build`'s per-ABI compile loop: before running `cargo build` for
  a target, `rusk build` content-hashes the project's source files,
  `Rusk.toml`, `Cargo.toml`, and the resolved toolchain versions (NDK,
  build-tools, release/debug, enabled features), and skips straight to
  reusing the cached `.so` when nothing that would affect the output has
  changed. The record lives at `target/rusk-incremental.toml`, so it's
  cleaned automatically whenever `target/` is. This only skips the
  per-ABI compile step itself — resource linking, dexing, and signing
  still run on every `rusk build` regardless of a cache hit, since those
  steps combine all ABIs' outputs together and a partial skip there
  would need more bookkeeping (which parts of the combined APK came from
  which target) than has been built yet.
- `rusk-sdkmgr` streams Google's full `repository2-3.xml` package
  manifest to find the handful of packages it needs; that manifest is
  large, so the very first auto-provisioned build has a "Fetching Android
  SDK package manifest" step that takes a few seconds before any actual
  download starts.
- `rusk-jnigen`'s Rust-source scan is a textual scan, not a full parser:
  it expects the straightforward `pub extern "system" fn Java_pkg_Class_method(env: ..., _class: ..., arg: Type, ...)`
  shape and assumes neither the Java class name nor method name contains
  a literal underscore (JNI's own name-mangling makes that ambiguous
  without a full C++-style demangler).
- `rusk emulator start` launches the emulator detached and returns
  immediately; it does not currently poll `adb wait-for-device` for you,
  so `rusk run` right after `rusk emulator start` may need a retry while
  the AVD finishes booting.
- `rusk-bundle` supports real multi-module assembly
  (`build_bundle_multi_module`, packaging a base module plus any number
  of named feature-module directories into one `.aab`), but the `rusk
  bundle` CLI command still only drives the single-module path — a
  project needing dynamic feature modules currently calls
  `rusk_bundle::build_bundle_multi_module` directly from its own build
  script rather than through `rusk bundle` itself. This crate does not
  generate a feature module's `<dist:module>` manifest metadata; that
  still needs to be authored by hand in the module's own
  `AndroidManifest.xml`. Inspect the resulting `.aab` with
  `bundletool dump manifest` before uploading to Play Console.
- `rusk-symbolize` needs the *unstripped* copy of a release `.so` (the
  one `cargo build` produces before `[build] strip_release` runs
  `llvm-strip` on it) — if that intermediate has been cleaned, only a
  fresh rebuild can regenerate it; Rusk doesn't separately archive
  unstripped copies of past release builds.
- `rusk-profile` shells out to the NDK's bundled `simpleperf/report.py`,
  which is a Python script — a Python 3 interpreter needs to be on PATH
  for `rusk profile report` even though nothing else in the toolchain
  needs one.
- `rusk-workspace` (`Rusk.workspace.toml`) is not yet wired into any CLI
  command — `rusk build --all`/`rusk lint --all` don't exist yet; the
  crate's discovery/member-resolution logic is complete and unit-testable
  on its own, but nothing in `rusk-cli` calls it yet.
- `rusk keystore generate`'s password prompt disables terminal echo via
  `stty -echo` on Unix (shelled out, to avoid depending on a
  platform-specific `termios` struct layout that differs between Linux
  and macOS) and the Win32 Console API directly on Windows.
- `rusk-render`'s `AppShell` is software (CPU) rendering only —
  `ANativeWindow_lock`/`unlockAndPost`, no OpenGL/Vulkan. It's enough to
  prove a render loop is running and to build simple CPU-rasterized UI,
  but it will not get you GPU-accelerated 3D; `AppShell::native_window()`
  is the intended handoff point to `wgpu`/`glutin` for that, not
  something this crate does itself.
- `rusk new --with-render` only works when the `rusk` binary being run
  still lives inside (or next to) the SDK source checkout it was built
  from — it locates `rusk-render`/`rusk-ndk-sys` by walking up from its
  own executable path looking for the SDK's workspace `Cargo.toml`, since
  those two crates aren't published to crates.io yet. A `rusk` binary
  copied somewhere else on its own won't find them.
- `AppShell::drain_input_events` (backed by `rusk_render::input`) safely
  reads touch and key events off the input queue, but does not currently
  expose `AMotionEvent_getPointerId` (not yet in `rusk-ndk-sys`'s
  bindings) — multi-touch pointers are given by their current index in
  the event, not a stable per-finger identity that survives across
  frames as fingers are added/removed mid-gesture. A project doing
  serious multi-touch gesture recognition (pinch-to-zoom tracking a
  specific pair of fingers across many frames) will need that addition
  first.
- `rusk-tls`'s exact `rustls-platform-verifier::Verifier::new` call has
  shifted signature across that crate's versions in the past; if it
  doesn't compile against whatever version Cargo resolves, check that
  crate's docs.rs page for the constructor your resolved version
  actually exposes — this is the one call in `rusk-tls` built from a
  lower-confidence source than the rest of this SDK.
- `rustls-platform-verifier` (which `rusk-tls` uses) does not currently
  support trusting extra/custom root certificates beyond the OS trust
  store on Android — a project needing to trust a private/internal CA on
  Android needs a different verifier setup than `rusk_tls::client_config`
  provides. This is an upstream limitation, not something `rusk-tls`
  adds on top.
- `rusk-sqlite`'s `Migrations` runner is intentionally minimal — ordered
  SQL statements applied once, tracked by a single version number. It
  has no down-migrations, no dry-run/diff mode, and no per-migration
  checksum verification; a project with more demanding migration needs
  should reach for a dedicated migration crate instead.
