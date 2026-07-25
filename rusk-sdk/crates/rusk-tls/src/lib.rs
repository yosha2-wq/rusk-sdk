//! rusk-tls: a `rustls`-based TLS client set up correctly for Android.
//!
//! # Why not the NDK's own OpenSSL
//!
//! The Android NDK bundles OpenSSL 1.1.1, which reached end-of-life and
//! stopped receiving security patches. `rustls` has no C dependency at
//! all — it's TLS implemented in Rust, compiled the same way the rest
//! of a Rusk project's own code is, with no separate native library to
//! keep patched.
//!
//! # The one-call gotcha this crate exists to prevent
//!
//! `rustls-platform-verifier` (the recommended way to validate server
//! certificates against the OS's actual trust store rather than a
//! bundled list) requires an explicit initialization call on Android —
//! `rustls_platform_verifier::android::init_hosted()` — before the
//! first TLS handshake. Skipping it doesn't fail gracefully; it panics
//! at the first connection attempt. This has caused real crash reports
//! in unrelated projects that pull in `rustls-platform-verifier`
//! transitively without knowing about the Android-specific
//! initialization requirement. [`init`] does this call for you, in the
//! one place a Rusk project already has an `ANativeActivity_onCreate`
//! (or `rusk-render::AppShell::run`) to call it from.

use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TlsError {
    #[error("rustls client config build failed: {0}")]
    ConfigBuild(String),
    #[error("rusk_tls::init() was not called before building a client config — call it once early in ANativeActivity_onCreate")]
    NotInitialized,
}

#[cfg(target_os = "android")]
static INITIALIZED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Performs the Android-specific one-time setup `rustls-platform-verifier`
/// requires before any TLS connection is attempted. Call this once, early
/// in `ANativeActivity_onCreate` — before any networking code runs, not
/// just before the first `rustls` call, since it's easy for an unrelated
/// dependency (an HTTP client, a websocket library) to make the first
/// TLS connection before a project's own networking code does.
///
/// On non-Android targets this is a no-op — `rustls-platform-verifier`
/// only needs this call on Android, so [`init`] is safe to call
/// unconditionally in cross-platform code without `#[cfg]` guards at
/// every call site.
///
/// # Safety
/// Must be called from a thread attached to the JVM with a valid
/// `JNIEnv` reachable the way `rustls-platform-verifier`'s own
/// `android::init_hosted()` expects — in practice, this means calling
/// it from `ANativeActivity_onCreate` or another JNI entry point, not
/// from an arbitrary background thread spawned before any JNI call has
/// happened on it.
pub fn init() {
    #[cfg(target_os = "android")]
    {
        use std::sync::atomic::Ordering;
        if INITIALIZED.swap(true, Ordering::AcqRel) {
            return; // already initialized; init_hosted() is not safe to call twice
        }
        rustls_platform_verifier::android::init_hosted();
    }
}

/// Builds a `rustls::ClientConfig` using the OS's own certificate trust
/// store via `rustls-platform-verifier` — the verification approach the
/// `rustls` project itself recommends as the best default for an
/// application shipped to end users, since it tracks the OS's own
/// certificate trust decisions (including anything an enterprise MDM or
/// the user has explicitly added) instead of a fixed list baked into the
/// binary at compile time.
///
/// Returns [`TlsError::NotInitialized`] if called before [`init`] on
/// Android, rather than letting the underlying panic surface — a clear
/// error a project can log and recover from is a better failure mode
/// than an opaque crash the first time a network request happens.
pub fn client_config() -> Result<Arc<rustls::ClientConfig>, TlsError> {
    #[cfg(target_os = "android")]
    {
        use std::sync::atomic::Ordering;
        if !INITIALIZED.load(Ordering::Acquire) {
            return Err(TlsError::NotInitialized);
        }
    }

    // `rustls::ClientConfig::builder()` below (the plain, no-provider
    // form) relies on a process-wide default `CryptoProvider` having
    // been installed already — nothing else in this crate does that, so
    // it's done here. `install_default` takes the *owned* `CryptoProvider`
    // by value (not `Arc<CryptoProvider>` — Arc-wrapping it first, as an
    // earlier version of this function did, is backwards: the method
    // wraps it in an `Arc` internally on success, and returns one back
    // to you on failure) and returns `Err` if a default was already
    // installed by something else in the process; that's fine, it just
    // means we don't need to (hence `let _ =`).
    let _ = rustls::crypto::ring::default_provider().install_default();

    // `Verifier::new()` takes no arguments and returns `Self` directly
    // (not a `Result`) as of rustls-platform-verifier 0.3.4 — the
    // provider-argument, fallible constructor this line used to call
    // doesn't exist on that version. This is exactly the "check the
    // resolved version's exact signature" case the module docs already
    // flagged as lower-confidence.
    let verifier = rustls_platform_verifier::Verifier::new();

    let mut config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(Arc::new(config))
}

/// Known, documented limitations of the platform verifier on Android —
/// surfaced here rather than left implicit, since silently different
/// behavior on one platform is exactly the kind of thing that produces
/// a confusing bug report from a real user later:
/// - Extra/custom root certificates beyond the OS trust store are not
///   currently supported on Android by `rustls-platform-verifier` (this
///   is upstream's own documented gap, not something `rusk-tls` adds).
///   A project needing to trust a private/internal CA on Android needs
///   a different verifier setup than [`client_config`] provides.
/// - A self-signed certificate trusted via a user-installed CA on the
///   device can be reported as revoked rather than accepted, per an
///   open upstream issue — this is a known rough edge in the platform
///   verifier's Android certificate-chain handling, not a `rusk-tls` bug.
pub const ANDROID_LIMITATIONS: &str = "see rusk-tls crate docs: ANDROID_LIMITATIONS";