//! rusk-jni-safe: a safe-Rust layer over the `jni` crate for calling
//! Java code from Rust, as an alternative to writing raw `unsafe extern
//! "C"` functions by hand.
//!
//! # How this relates to `rusk-jnigen`
//!
//! `rusk-jnigen` scans a project's Rust source for
//! `Java_<pkg>_<Class>_<method>`-named `extern "C"` functions and
//! generates the matching Java `native` declarations — that half of the
//! bridge (Java calling into Rust) still needs an `extern "C"` function
//! with that exact mangled name; there's no way around that, it's the
//! JNI ABI's own contract, not something this crate can wrap away.
//!
//! What this crate helps with is the *other* direction and the *inside*
//! of those functions: calling Java methods from Rust, converting Java
//! exceptions into `Result`s instead of leaving them pending (which
//! otherwise silently corrupts every subsequent JNI call until the
//! function returns), and building argument/return-type-safe method
//! calls without hand-writing JNI type signature strings
//! (`"(ILjava/lang/String;)V"`) by hand.
//!
//! This crate is purely additive — nothing in `rusk-jnigen`,
//! `rusk-build`, or `rusk-render` depends on it, and a project can use
//! `rusk-jnigen`'s raw generation, this crate's safe wrappers, or both
//! side by side.

use jni::errors::Error as JniError;
use jni::objects::{JObject, JString, JValue};
use jni::JNIEnv;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SafeJniError {
    #[error("JNI call failed: {0}")]
    Jni(#[from] JniError),
    #[error("a Java exception was thrown: {class} — {message}")]
    JavaException { class: String, message: String },
    #[error("expected a non-null Java object but got null")]
    UnexpectedNull,
}

/// Checks for and clears a pending Java exception, converting it into a
/// [`SafeJniError::JavaException`] with the exception class name and
/// `getMessage()` result. **Must** be called after any raw JNI call that
/// can throw, before making another JNI call — the JNI spec forbids
/// calling most JNI functions while an exception is pending, and a
/// hand-rolled bridge that forgets this check is a very easy way to
/// silently corrupt subsequent calls instead of getting a clear error.
pub fn check_exception(env: &mut JNIEnv) -> Result<(), SafeJniError> {
    if !env.exception_check().unwrap_or(false) {
        return Ok(());
    }
    let throwable = env.exception_occurred()?;
    env.exception_clear()?;

    // Chained with `Option::and_then` (via `.ok()` at each step) rather
    // than `Result::and_then` — the steps here mix two different error
    // types (`jni::errors::Error` from raw JNI calls, `SafeJniError`
    // from `jstring_to_string`), which `Result::and_then` can't chain
    // directly since it requires one error type throughout. Since any
    // failure along the way falls back to a placeholder string anyway,
    // `Option` is both correct and simpler than reconciling the two
    // error types just to immediately discard whichever one occurs.
    let class_name = env
        .call_method(&throwable, "getClass", "()Ljava/lang/Class;", &[])
        .ok()
        .and_then(|c| c.l().ok())
        .and_then(|class_obj| {
            env.call_method(class_obj, "getName", "()Ljava/lang/String;", &[]).ok()
        })
        .and_then(|n| n.l().ok())
        .and_then(|jstr| jstring_to_string(env, jstr.into()).ok())
        .unwrap_or_else(|| "<unknown exception class>".to_string());

    let message = env
        .call_method(&throwable, "getMessage", "()Ljava/lang/String;", &[])
        .ok()
        .and_then(|m| m.l().ok())
        .and_then(|obj| {
            if obj.is_null() {
                Some("<no message>".to_string())
            } else {
                jstring_to_string(env, obj.into()).ok()
            }
        })
        .unwrap_or_else(|| "<no message>".to_string());

    Err(SafeJniError::JavaException { class: class_name, message })
}

/// Converts a `JString` to a Rust `String`, checking for a pending
/// exception afterward (a malformed/GC'd string reference can throw).
pub fn jstring_to_string(env: &mut JNIEnv, jstr: JString) -> Result<String, SafeJniError> {
    let result = env.get_string(&jstr).map(|s| s.into());
    check_exception(env)?;
    Ok(result?)
}

/// Calls a static void method with no arguments — the common case for
/// "notify Java something happened" calls, without hand-writing a `"()V"`
/// signature string and a `&[]` argument slice at every call site.
pub fn call_static_void(env: &mut JNIEnv, class: &str, method: &str) -> Result<(), SafeJniError> {
    env.call_static_method(class, method, "()V", &[])?;
    check_exception(env)?;
    Ok(())
}

/// Calls an instance method returning an `int`, propagating any thrown
/// exception as a [`SafeJniError`] instead of leaving it pending.
pub fn call_int_method(
    env: &mut JNIEnv,
    obj: &JObject,
    method: &str,
    signature: &str,
    args: &[JValue],
) -> Result<i32, SafeJniError> {
    let result = env.call_method(obj, method, signature, args)?.i();
    check_exception(env)?;
    Ok(result?)
}

/// Calls an instance method returning a Java `String`, converting it to
/// a Rust `String` and propagating any thrown exception.
pub fn call_string_method(
    env: &mut JNIEnv,
    obj: &JObject,
    method: &str,
    signature: &str,
    args: &[JValue],
) -> Result<String, SafeJniError> {
    let result = env.call_method(obj, method, signature, args)?.l()?;
    check_exception(env)?;
    if result.is_null() {
        return Err(SafeJniError::UnexpectedNull);
    }
    jstring_to_string(env, result.into())
}

/// Looks up a class and returns a global reference to it, wrapping the
/// common "look up a class once, keep it around" pattern with exception
/// checking built in. A `GlobalRef` (unlike a local `JClass`) is safe to
/// store past the current JNI stack frame — the right choice for a class
/// looked up once at startup and reused across many calls.
pub fn find_class_global(env: &mut JNIEnv, name: &str) -> Result<jni::objects::GlobalRef, SafeJniError> {
    let class = env.find_class(name)?;
    check_exception(env)?;
    let global = env.new_global_ref(class)?;
    Ok(global)
}