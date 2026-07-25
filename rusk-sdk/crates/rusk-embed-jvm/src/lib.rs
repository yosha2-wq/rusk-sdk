//! rusk-embed-jvm: use an existing, unmodified Java/Maven library from
//! Rust by embedding a real JVM in the process, instead of rewriting
//! that library in Rust — a wrapper around the `j4rs` crate.
//!
//! # Why this exists instead of a "translator"
//!
//! There is no general-purpose Rust-to-Java or Java-to-Rust source
//! translator, and there can't sensibly be one: Java's garbage-collected
//! object model and Rust's ownership/borrowing model don't have a
//! structural correspondence a rewrite tool could follow mechanically.
//! Academic work in this space (e.g. the VERT study, arXiv:2404.18852)
//! finds the same thing from the opposite direction (Java/Go → Rust) —
//! rule-based transpilation doesn't scale across that particular
//! semantic gap.
//!
//! What *does* work, and is what this crate wraps, is running a real JVM
//! inside the Rust process and calling into it — the Java library keeps
//! running as actual Java (correct by construction, since it's
//! unmodified), and Rust code talks to it through `j4rs`'s reflection-
//! based invocation API. This is the right tool specifically for "I need
//! functionality that already exists as a Java/Kotlin library and don't
//! want to reimplement it" — not for turning a Rust codebase into Java
//! source, which isn't what any tool in this space actually does.
//!
//! # How this differs from `rusk-jni-safe` / `rusk-jnigen`
//!
//! `rusk-jnigen`/`rusk-jni-safe` are for the *thin bridge* case: a small,
//! app-specific Java shim (the generated `NativeActivity` subclass, a
//! handful of `native` methods) that's part of the app itself. This
//! crate is for the *whole existing library* case: pulling in something
//! like a Maven-hosted PDF renderer, crypto library, or SDK that only
//! ships as a JAR, with no intention of touching its source. The two
//! are not mutually exclusive — a project can use `rusk-jnigen` for its
//! own small native bridge and `rusk-embed-jvm` for a specific
//! third-party Java dependency at the same time.
//!
//! j4rs's own documentation notes it's been tested on Android using the
//! same native-only entry-point approach `rusk-render`'s `AppShell`
//! uses (a `NativeActivity`/`android-activity`-style Rust-first app,
//! rather than a Java-first app calling into Rust) — this crate assumes
//! that same setup.

use j4rs::{InvocationArg, Jvm, JvmBuilder, MavenArtifact};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EmbedJvmError {
    #[error("failed to start the embedded JVM: {0}")]
    JvmStart(String),
    #[error("failed to deploy Maven artifact \"{artifact}\": {reason}")]
    MavenDeploy { artifact: String, reason: String },
    #[error("failed to create Java instance of \"{class}\": {reason}")]
    CreateInstance { class: String, reason: String },
    #[error("failed to invoke \"{method}\" on \"{class}\": {reason}")]
    Invoke {
        class: String,
        method: String,
        reason: String,
    },
    #[error("failed to convert the Java return value to the requested Rust type: {0}")]
    Conversion(String),
}

/// Starts an embedded JVM with default settings — the classpath, JVM
/// options, and Maven repositories `j4rs` uses out of the box. For
/// anything beyond the default (extra JVM args, a private Maven
/// repository, an explicit classpath entry for a local jar not on
/// Maven), build a `j4rs::JvmBuilder` directly instead of using this
/// convenience function — this crate re-exports `j4rs` in full for
/// exactly that reason (see the crate's `pub use j4rs` below).
pub fn start_jvm() -> Result<Jvm, EmbedJvmError> {
    JvmBuilder::new().build().map_err(|e| EmbedJvmError::JvmStart(e.to_string()))
}

/// Downloads (if not already cached) and adds a Maven artifact to the
/// embedded JVM's classpath, in `group:artifact:version` form — the same
/// coordinate format `rusk-javadeps` already uses for `[dependencies.java]`
/// in `Rusk.toml`, so a developer moving a dependency from "linked into
/// the app's own APK via rusk-javadeps" to "called via an embedded JVM"
/// doesn't have to learn a different coordinate syntax.
///
/// Note: this deploys only the named artifact, not its transitive
/// dependencies (a `j4rs` limitation, not one this wrapper adds) — a
/// library with its own dependencies needs each of them deployed
/// explicitly, or bundled as a single "fat/shaded" jar upstream.
pub fn deploy_maven_artifact(jvm: &Jvm, coordinate: &str) -> Result<(), EmbedJvmError> {
    let artifact = MavenArtifact::from(coordinate);
    jvm.deploy_artifact(&artifact)
        .map_err(|e| EmbedJvmError::MavenDeploy {
            artifact: coordinate.to_string(),
            reason: e.to_string(),
        })
}

/// Creates a Java instance of `class` with no-argument construction —
/// the common case (a library's entry point is usually a default
/// constructor plus a fluent/builder API from there). For a
/// parameterized constructor, call `jvm.create_instance` directly with
/// real `InvocationArg`s; this function only exists to skip the
/// empty-args-slice boilerplate for the argument-free case.
pub fn new_instance(jvm: &Jvm, class: &str) -> Result<j4rs::Instance, EmbedJvmError> {
    jvm.create_instance(class, &Vec::<InvocationArg>::new())
        .map_err(|e| EmbedJvmError::CreateInstance {
            class: class.to_string(),
            reason: e.to_string(),
        })
}

/// Calls a no-argument instance method and converts the result to a
/// Rust `String` — the single most common shape for "call this library
/// and get text back" (a formatted result, a status message, a
/// serialized response). For methods taking arguments or returning other
/// types, call `jvm.invoke` / `jvm.to_rust` directly; this crate
/// re-exports `j4rs` in full so nothing here is a hard requirement.
pub fn invoke_returning_string(
    jvm: &Jvm,
    instance: &j4rs::Instance,
    method: &str,
) -> Result<String, EmbedJvmError> {
    let result = jvm
        .invoke(instance, method, &Vec::<InvocationArg>::new())
        .map_err(|e| EmbedJvmError::Invoke {
            class: "<instance>".to_string(),
            method: method.to_string(),
            reason: e.to_string(),
        })?;
    jvm.to_rust(result).map_err(|e| EmbedJvmError::Conversion(e.to_string()))
}

/// Convenience for the "call a static utility method" shape common in
/// Java libraries (`SomeUtil.doThing(...)`), converting a single string
/// argument and a string return value — the most common signature shape
/// for glue/utility calls into an embedded library.
pub fn invoke_static_string_to_string(
    jvm: &Jvm,
    class: &str,
    method: &str,
    argument: &str,
) -> Result<String, EmbedJvmError> {
    let arg = InvocationArg::try_from(argument).map_err(|e| EmbedJvmError::Conversion(e.to_string()))?;
    let result = jvm
        .invoke_static(class, method, &[arg])
        .map_err(|e| EmbedJvmError::Invoke {
            class: class.to_string(),
            method: method.to_string(),
            reason: e.to_string(),
        })?;
    jvm.to_rust(result).map_err(|e| EmbedJvmError::Conversion(e.to_string()))
}

// Re-exported so a project that needs `j4rs` APIs beyond this crate's
// small set of convenience wrappers (chained invocations, callbacks from
// Java back into Rust, JavaFX, async futures) can reach them without
// adding a second, separately-versioned `j4rs` dependency that could
// drift out of sync with the version this crate was built against.
pub use j4rs;