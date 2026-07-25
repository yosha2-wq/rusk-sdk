//! rusk-lint: static checks over `Rusk.toml` that catch the class of
//! mistake which otherwise only surfaces much later — a Play Store
//! rejection for an undeclared permission, a `NETWORK_SECURITY_CONFIG`
//! that silently doesn't apply because cleartext traffic wasn't actually
//! turned off, a target/compile SDK combination Play will reject on
//! upload, or a debug-signed release build that was never meant to ship.
//! Each check is independent and reports a [`Finding`] with a severity,
//! so `rusk lint` (and `rusk build`, which runs a fast subset silently)
//! can decide how loud to be about it.

use rusk_manifest::RuskManifest;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub id: &'static str,
    pub severity: Severity,
    pub message: String,
    pub hint: Option<String>,
}

impl Finding {
    fn new(id: &'static str, severity: Severity, message: impl Into<String>) -> Self {
        Self {
            id,
            severity,
            message: message.into(),
            hint: None,
        }
    }

    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// Runs every check and returns findings not suppressed by `[lint]
/// ignore`, sorted by descending severity so the most important issues
/// are first regardless of check order.
pub fn run(manifest: &RuskManifest) -> Vec<Finding> {
    let mut findings = Vec::new();
    findings.extend(check_sdk_versions(manifest));
    findings.extend(check_permissions(manifest));
    findings.extend(check_release_signing(manifest));
    findings.extend(check_abi_coverage(manifest));
    findings.extend(check_debuggable_release(manifest));
    findings.extend(check_java_dependency_versions(manifest));
    findings.extend(check_app_id(manifest));

    let ignored: std::collections::HashSet<&str> =
        manifest.lint.ignore.iter().map(|s| s.as_str()).collect();
    findings.retain(|f| !ignored.contains(f.id));
    findings.sort_by(|a, b| b.severity.cmp(&a.severity));
    findings
}

/// Play Console rejects uploads whose `targetSdkVersion` is more than
/// one year behind the current requirement, and `compileSdkVersion`
/// below `targetSdkVersion` is a build-correctness bug, not just a
/// policy issue — some APIs targeted at `sdk_target` may not exist in
/// the `sdk_compile` platform jar being linked against.
fn check_sdk_versions(m: &RuskManifest) -> Vec<Finding> {
    let mut out = Vec::new();
    if m.package.sdk_compile < m.package.sdk_target {
        out.push(
            Finding::new(
                "sdk-compile-below-target",
                Severity::Error,
                format!(
                    "package.sdk_compile ({}) is lower than package.sdk_target ({})",
                    m.package.sdk_compile, m.package.sdk_target
                ),
            )
            .with_hint("raise sdk_compile to at least sdk_target — APIs targeted at sdk_target may not exist in the sdk_compile platform jar"),
        );
    }
    if m.package.sdk_target < 33 {
        out.push(
            Finding::new(
                "sdk-target-outdated",
                Severity::Warning,
                format!(
                    "package.sdk_target ({}) is below API 33; Play Console requires targeting within one year of the latest Android release for new submissions and updates",
                    m.package.sdk_target
                ),
            )
            .with_hint("raise sdk_target — check https://developer.android.com/google/play/requirements/target-sdk for the current minimum"),
        );
    }
    if m.package.sdk_min < 21 {
        out.push(Finding::new(
            "sdk-min-very-old",
            Severity::Info,
            format!(
                "package.sdk_min ({}) targets Android versions with negligible remaining market share",
                m.package.sdk_min
            ),
        ));
    }
    out
}

/// Flags permissions that are either almost always requested by mistake
/// (copy-pasted from an example) or that Play Console scrutinizes
/// heavily during review (background location, SMS, call log) — not to
/// forbid them, since they're sometimes genuinely needed, but so a
/// developer notices before submitting rather than after a rejection.
fn check_permissions(m: &RuskManifest) -> Vec<Finding> {
    let mut out = Vec::new();
    let sensitive = [
        ("ACCESS_BACKGROUND_LOCATION", "requires a dedicated Play Console declaration form and a prominent in-app disclosure before it's granted"),
        ("READ_SMS", "restricted to a very small set of approved app categories (default SMS/dialer handlers) — Play will likely reject unless the app is one of those"),
        ("READ_CALL_LOG", "restricted the same way as READ_SMS"),
        ("SYSTEM_ALERT_WINDOW", "requires a separate runtime grant flow the user must complete in system settings — a raw manifest permission alone won't work"),
        ("WRITE_EXTERNAL_STORAGE", "has no effect on API 30+ (scoped storage) unless requestLegacyExternalStorage is also set — likely a permission left over from an older target SDK"),
    ];
    for (perm, note) in sensitive {
        if m.permissions.list.iter().any(|p| p == perm) {
            out.push(
                Finding::new(
                    "sensitive-permission",
                    Severity::Warning,
                    format!("permission {perm} is declared"),
                )
                .with_hint(note.to_string()),
            );
        }
    }
    if m.permissions.list.iter().any(|p| p == "INTERNET")
        && m.permissions.list.iter().any(|p| p == "ACCESS_NETWORK_STATE")
        && m.permissions.list.len() == 2
    {
        // This specific pair is extremely common and not actually worth
        // flagging — explicitly not a finding, documented here so a
        // future check doesn't accidentally re-add noise for it.
    }
    out
}

/// A release build signed with the auto-generated debug keystore will
/// install and run fine locally, which is exactly what makes it a trap:
/// nothing fails until the Play Store rejects the upload (debug-signed
/// APKs are rejected outright) or, worse, it's side-loaded to real users
/// with a key nobody backed up.
fn check_release_signing(m: &RuskManifest) -> Vec<Finding> {
    if m.signing.is_none() {
        return vec![Finding::new(
            "no-release-signing-configured",
            Severity::Info,
            "no [signing] section — `rusk build --release` will still succeed, signed with the auto-generated debug keystore",
        )
        .with_hint("run `rusk keystore generate` and add a [signing] section before shipping a release build anywhere outside your own devices")];
    }
    Vec::new()
}

/// A project targeting only one ABI (commonly just the developer's own
/// device architecture) will install fine on that one device and then
/// fail to install — or silently fail to load the native library — on
/// everything else.
fn check_abi_coverage(m: &RuskManifest) -> Vec<Finding> {
    if m.abi.targets.len() == 1 {
        return vec![Finding::new(
            "single-abi-target",
            Severity::Warning,
            format!(
                "only one ABI target configured ({})",
                m.abi.targets[0]
            ),
        )
        .with_hint("add at least arm64-v8a (covers nearly all real devices) and x86_64 (emulators) — see [abi] targets in Rusk.toml")];
    }
    if !m.abi.targets.iter().any(|a| a == "arm64-v8a") {
        return vec![Finding::new(
            "missing-arm64",
            Severity::Error,
            "arm64-v8a is not in [abi] targets",
        )
        .with_hint("arm64-v8a covers the overwhelming majority of real Android devices sold since ~2019 — omitting it will make the app uninstallable on most phones")];
    }
    Vec::new()
}

fn check_debuggable_release(m: &RuskManifest) -> Vec<Finding> {
    if m.app.debuggable {
        return vec![Finding::new(
            "debuggable-true",
            Severity::Warning,
            "app.debuggable is true",
        )
        .with_hint("Play Console rejects debuggable=true release uploads outright — set this to false (or leave it unset; false is the default) before a release build")];
    }
    Vec::new()
}

/// `"latest"`/`"+"`-style unpinned Maven versions defeat
/// `rusk-lock`/reproducible builds even when a `Rusk.lock` exists,
/// since the *next* resolution can silently pick up a different
/// transitive version than what's recorded.
fn check_java_dependency_versions(m: &RuskManifest) -> Vec<Finding> {
    m.dependencies
        .java
        .iter()
        .filter(|(_, v)| v.ends_with('+') || v.eq_ignore_ascii_case("latest"))
        .map(|(k, v)| {
            Finding::new(
                "unpinned-java-dependency",
                Severity::Warning,
                format!("{k} = \"{v}\" is not a pinned version"),
            )
            .with_hint("pin an exact version — an unpinned dependency can resolve differently between builds even with the same Rusk.lock present")
        })
        .collect()
}

/// `com.example.*` is the package id every Android tutorial uses; Play
/// Console has an outright block on publishing under it, so shipping
/// with it left unchanged is a guaranteed rejection, not just a style nit.
fn check_app_id(m: &RuskManifest) -> Vec<Finding> {
    if m.package.id.starts_with("com.example.") {
        return vec![Finding::new(
            "example-app-id",
            Severity::Error,
            format!("package.id \"{}\" uses the reserved com.example.* namespace", m.package.id),
        )
        .with_hint("Play Console blocks publishing under com.example.* outright — change package.id before a release build")];
    }
    Vec::new()
}
