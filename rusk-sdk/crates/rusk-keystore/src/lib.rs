//! rusk-keystore: everything about Android app-signing keys beyond the
//! auto-generated debug keystore `rusk-apk::ensure_debug_keystore`
//! already provides for local/dev installs.
//!
//! A debug keystore is fine for `rusk run` on your own device, but a
//! release build meant for the Play Store or side-loaded distribution
//! needs a *stable, backed-up, long-lived* signing key — losing it means
//! losing the ability to ship updates to an already-installed app,
//! forever. This crate covers the release-signing lifecycle `rusk-apk`
//! deliberately doesn't: generating one interactively with sane
//! defaults, inspecting what's in an existing keystore, exporting the
//! public certificate for Play Console's "upload key" flow, and running
//! `apksigner verify` against a built APK to confirm what actually
//! signed it before you ship it.

use std::path::{Path, PathBuf};
use std::process::Command;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum KeystoreError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("`{tool}` is not on PATH; install a JDK (17+ recommended) and make sure its `bin/` directory is on PATH")]
    ToolNotFound { tool: &'static str },
    #[error("`{tool}` exited with status {status}")]
    ToolFailed { tool: &'static str, status: i32 },
    #[error("keystore already exists at {0}; pass --force to overwrite, or choose a different path")]
    AlreadyExists(PathBuf),
    #[error("passwords did not match")]
    PasswordMismatch,
    #[error("password must be at least 6 characters (keytool's own minimum)")]
    PasswordTooShort,
    #[error("invalid distinguished-name field \"{0}\": must not contain a comma or `=`")]
    InvalidDnField(String),
}

/// The distinguished-name fields `keytool -genkeypair -dname` expects.
/// Only `common_name` is meaningfully user-facing; the rest exist mostly
/// because `keytool` requires *some* value for each RDN component.
pub struct DistinguishedName {
    pub common_name: String,
    pub organizational_unit: String,
    pub organization: String,
    pub locality: String,
    pub state: String,
    pub country_code: String,
}

impl Default for DistinguishedName {
    fn default() -> Self {
        Self {
            common_name: "Unknown".to_string(),
            organizational_unit: "Unknown".to_string(),
            organization: "Unknown".to_string(),
            locality: "Unknown".to_string(),
            state: "Unknown".to_string(),
            country_code: "US".to_string(),
        }
    }
}

impl DistinguishedName {
    fn validate(&self) -> Result<(), KeystoreError> {
        for (label, field) in [
            ("common_name", &self.common_name),
            ("organizational_unit", &self.organizational_unit),
            ("organization", &self.organization),
            ("locality", &self.locality),
            ("state", &self.state),
            ("country_code", &self.country_code),
        ] {
            if field.contains(',') || field.contains('=') {
                return Err(KeystoreError::InvalidDnField(label.to_string()));
            }
        }
        Ok(())
    }

    fn to_dname_string(&self) -> String {
        format!(
            "CN={}, OU={}, O={}, L={}, ST={}, C={}",
            self.common_name,
            self.organizational_unit,
            self.organization,
            self.locality,
            self.state,
            self.country_code
        )
    }
}

pub struct GenerateOptions {
    pub keystore_path: PathBuf,
    pub alias: String,
    pub store_password: String,
    pub key_password: String,
    /// Key validity in days. Google recommends at least 25 years
    /// (9125 days) for release keys, since the key must outlive every
    /// update you ever intend to ship — `default_validity_days` reflects
    /// that recommendation rather than `keytool`'s own much shorter
    /// default.
    pub validity_days: u32,
    pub key_size: u32,
    pub dname: DistinguishedName,
    pub force: bool,
}

pub fn default_validity_days() -> u32 {
    9125 // ~25 years, matching Google's own Play Console guidance
}

impl GenerateOptions {
    pub fn validate(&self) -> Result<(), KeystoreError> {
        if self.store_password.len() < 6 {
            return Err(KeystoreError::PasswordTooShort);
        }
        // An empty key_password means "reuse the store password" (see
        // generate()), so only a genuine, non-empty mismatch is invalid.
        if !self.key_password.is_empty() && self.key_password != self.store_password {
            return Err(KeystoreError::PasswordMismatch);
        }
        self.dname.validate()?;
        Ok(())
    }
}

/// Generates a new release keystore via the JDK's `keytool`. This is
/// deliberately a thin wrapper — Rusk does not reimplement PKCS#12
/// keystore generation itself, since getting that subtly wrong is
/// exactly the kind of mistake that becomes unrecoverable once an app
/// has shipped with the resulting key.
pub fn generate(opts: &GenerateOptions) -> Result<(), KeystoreError> {
    opts.validate()?;

    if opts.keystore_path.is_file() && !opts.force {
        return Err(KeystoreError::AlreadyExists(opts.keystore_path.clone()));
    }
    if opts.keystore_path.is_file() && opts.force {
        std::fs::remove_file(&opts.keystore_path).map_err(|source| KeystoreError::Io {
            path: opts.keystore_path.clone(),
            source,
        })?;
    }
    if let Some(parent) = opts.keystore_path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| KeystoreError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let key_password = if opts.key_password.is_empty() {
        opts.store_password.clone()
    } else {
        opts.key_password.clone()
    };

    let step = rusk_ui::Step::start(format!(
        "Generating release keystore ({} bit RSA, {} day validity)",
        opts.key_size, opts.validity_days
    ));

    let mut cmd = keytool_command()?;
    cmd.arg("-genkeypair")
        .arg("-keystore")
        .arg(&opts.keystore_path)
        .arg("-alias")
        .arg(&opts.alias)
        .arg("-keyalg")
        .arg("RSA")
        .arg("-keysize")
        .arg(opts.key_size.to_string())
        .arg("-validity")
        .arg(opts.validity_days.to_string())
        .arg("-storepass")
        .arg(&opts.store_password)
        .arg("-keypass")
        .arg(&key_password)
        .arg("-dname")
        .arg(opts.dname.to_dname_string())
        .arg("-storetype")
        .arg("PKCS12");

    let status = cmd.status().map_err(|source| KeystoreError::Io {
        path: opts.keystore_path.clone(),
        source,
    })?;
    if !status.success() {
        step.fail("keytool exited with an error");
        return Err(KeystoreError::ToolFailed {
            tool: "keytool",
            status: status.code().unwrap_or(-1),
        });
    }
    step.ok();
    Ok(())
}

/// One alias entry as reported by `keytool -list -v`, parsed out of its
/// human-oriented text output into fields a caller can actually use
/// (e.g. to print a table, or to flag "expires in under 30 days").
pub struct KeystoreEntry {
    pub alias: String,
    pub entry_type: String,
    pub creation_date: String,
    pub sha256_fingerprint: Option<String>,
    pub valid_from: Option<String>,
    pub valid_until: Option<String>,
}

/// Runs `keytool -list -v` and parses out the alias entries. Password is
/// required — `keytool` refuses to list a keystore's contents without it
/// even for non-sensitive metadata like the alias name.
pub fn list_entries(keystore_path: &Path, store_password: &str) -> Result<Vec<KeystoreEntry>, KeystoreError> {
    let mut cmd = keytool_command()?;
    cmd.arg("-list")
        .arg("-v")
        .arg("-keystore")
        .arg(keystore_path)
        .arg("-storepass")
        .arg(store_password);
    let output = cmd.output().map_err(|source| KeystoreError::Io {
        path: keystore_path.to_path_buf(),
        source,
    })?;
    if !output.status.success() {
        return Err(KeystoreError::ToolFailed {
            tool: "keytool",
            status: output.status.code().unwrap_or(-1),
        });
    }
    Ok(parse_keytool_list_output(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_keytool_list_output(text: &str) -> Vec<KeystoreEntry> {
    let mut entries = Vec::new();
    let mut current: Option<KeystoreEntry> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Alias name: ") {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            current = Some(KeystoreEntry {
                alias: rest.trim().to_string(),
                entry_type: String::new(),
                creation_date: String::new(),
                sha256_fingerprint: None,
                valid_from: None,
                valid_until: None,
            });
        } else if let Some(rest) = trimmed.strip_prefix("Creation date: ") {
            if let Some(e) = current.as_mut() {
                e.creation_date = rest.trim().to_string();
            }
        } else if let Some(rest) = trimmed.strip_prefix("Entry type: ") {
            if let Some(e) = current.as_mut() {
                e.entry_type = rest.trim().to_string();
            }
        } else if trimmed.starts_with("SHA256:") {
            if let Some(e) = current.as_mut() {
                e.sha256_fingerprint = Some(trimmed.trim_start_matches("SHA256:").trim().to_string());
            }
        } else if trimmed.starts_with("Valid from:") {
            // Format: "Valid from: <date> until: <date>"
            if let Some(e) = current.as_mut() {
                if let Some((from, until)) = trimmed.trim_start_matches("Valid from:").split_once("until:") {
                    e.valid_from = Some(from.trim().to_string());
                    e.valid_until = Some(until.trim().to_string());
                }
            }
        }
    }
    if let Some(entry) = current.take() {
        entries.push(entry);
    }
    entries
}

/// Exports the public certificate for `alias` as a `.pem` file, the
/// format Google Play Console's "upload key certificate" field expects
/// when registering a new signing key.
pub fn export_certificate(
    keystore_path: &Path,
    alias: &str,
    store_password: &str,
    out_pem: &Path,
) -> Result<(), KeystoreError> {
    let step = rusk_ui::Step::start(format!("Exporting certificate for alias \"{alias}\""));
    let mut cmd = keytool_command()?;
    cmd.arg("-exportcert")
        .arg("-keystore")
        .arg(keystore_path)
        .arg("-alias")
        .arg(alias)
        .arg("-storepass")
        .arg(store_password)
        .arg("-rfc")
        .arg("-file")
        .arg(out_pem);
    let status = cmd.status().map_err(|source| KeystoreError::Io {
        path: out_pem.to_path_buf(),
        source,
    })?;
    if !status.success() {
        step.fail("keytool exited with an error");
        return Err(KeystoreError::ToolFailed {
            tool: "keytool",
            status: status.code().unwrap_or(-1),
        });
    }
    step.ok();
    Ok(())
}

/// Result of `apksigner verify`, parsed into structured fields — the raw
/// output is a wall of `Verified using v1/v2/v3 scheme` lines that's
/// tedious to eyeball for the one thing that actually matters ("did this
/// verify at all, and with which scheme versions").
pub struct VerifyResult {
    pub verified: bool,
    pub v1_scheme: bool,
    pub v2_scheme: bool,
    pub v3_scheme: bool,
    pub v4_scheme: bool,
    pub raw_output: String,
}

/// Runs `apksigner verify -v` against a built APK. `apksigner_path`
/// should be the one from the project's provisioned build-tools
/// (`rusk_sdkmgr::ManagedSdk::build_tools().apksigner` or
/// `rusk_ndk::BuildTools::apksigner`) so the check uses the same tool
/// version that produced the signature.
pub fn verify_apk(apksigner_path: &Path, apk_path: &Path) -> Result<VerifyResult, KeystoreError> {
    let step = rusk_ui::Step::start(format!("Verifying signature of {}", apk_path.display()));
    let output = Command::new(apksigner_path)
        .arg("verify")
        .arg("-v")
        .arg(apk_path)
        .output()
        .map_err(|source| KeystoreError::Io {
            path: apk_path.to_path_buf(),
            source,
        })?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let verified = output.status.success();
    if verified {
        step.ok();
    } else {
        step.fail("apksigner reported the APK signature did not verify");
    }
    Ok(VerifyResult {
        verified,
        v1_scheme: text.contains("Verified using v1 scheme (JAR signing): true"),
        v2_scheme: text.contains("Verified using v2 scheme (APK Signature Scheme v2): true"),
        v3_scheme: text.contains("Verified using v3 scheme (APK Signature Scheme v3): true"),
        v4_scheme: text.contains("Verified using v4 scheme (APK Signature Scheme v4): true"),
        raw_output: text,
    })
}

fn keytool_command() -> Result<Command, KeystoreError> {
    which::which("keytool")
        .map(Command::new)
        .map_err(|_| KeystoreError::ToolNotFound { tool: "keytool" })
}
