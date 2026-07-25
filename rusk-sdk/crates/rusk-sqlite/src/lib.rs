//! rusk-sqlite: a thin layer over `rusqlite` (with its `bundled`
//! feature), configured for the common case of a Rusk Android project
//! that just wants a local on-device SQLite database with no manual
//! native-library setup.
//!
//! # Why `bundled` instead of linking a system SQLite
//!
//! Android does not guarantee a usable system SQLite for an app to link
//! against directly the way a desktop Linux distribution does —
//! `rusqlite`'s `bundled` feature instead compiles SQLite's own C source
//! straight into the crate during `cargo build`, using the same `cc`
//! crate machinery that already knows how to reach the NDK's `clang`
//! through the `CC_<target>`/`AR_<target>` environment variables
//! `rusk-build` sets for every ABI it compiles. This means SQLite gets
//! rebuilt correctly for each ABI in `[abi] targets` automatically, with
//! no separate step and no prebuilt `.so` to manage.
//!
//! # What this crate adds on top of `rusqlite`
//!
//! - [`open_app_database`] resolves the right on-device path for a
//!   per-app SQLite file, since "just pick a path" is a surprisingly
//!   easy thing to get subtly wrong on Android (an app's home directory
//!   isn't `$HOME`, and a hardcoded path risks colliding with another
//!   app or landing somewhere the app doesn't have write permission to).
//! - [`Migrations`] is a minimal, dependency-free schema-versioning
//!   helper — a numbered list of SQL statements applied in order,
//!   tracked in a `rusk_schema_version` table, so a project doesn't
//!   need to hand-roll "did I already run this migration" bookkeeping
//!   or reach for a heavier migration framework for what is often a
//!   handful of `CREATE TABLE` statements.

use std::path::{Path, PathBuf};
use thiserror::Error;

pub use rusqlite;
pub use rusqlite::{Connection, Result as SqliteResult};

#[derive(Debug, Error)]
pub enum SqliteError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("migration {version} failed: {source}")]
    Migration {
        version: u32,
        #[source]
        source: rusqlite::Error,
    },
    #[error("could not determine an app data directory to store the database in")]
    NoDataDir,
}

/// Opens (creating if needed) a SQLite database at
/// `<data_dir>/<file_name>`, where `data_dir` is the directory an
/// Android app is actually meant to store private, persistent data in.
///
/// On Android there is no environment-variable-based "home directory"
/// the way `$HOME` works on desktop Linux — an app's private storage
/// path is handed to it by the framework (`Context.getFilesDir()` on
/// the Java side) rather than being derivable from a fixed convention.
/// Since a Rusk project's native code has no `Context` to call that
/// through, `data_dir` must be passed in explicitly — get it either
/// from a `[[bindings]]` entry that calls `Context.getFilesDir()` and
/// hands the resulting path to Rust over JNI, or from
/// `ANativeActivity::internalDataPath`, which the NDK populates with
/// the same directory without needing a JNI round-trip at all.
pub fn open_app_database(data_dir: &Path, file_name: &str) -> Result<Connection, SqliteError> {
    std::fs::create_dir_all(data_dir).map_err(|source| SqliteError::Io {
        path: data_dir.to_path_buf(),
        source,
    })?;
    let db_path = data_dir.join(file_name);
    let conn = Connection::open(&db_path)?;

    // WAL mode is the right default for a mobile app: it allows a
    // background thread to write while the UI thread reads without
    // blocking either, which matters more on a phone (where blocking
    // the render/input thread on disk I/O is directly visible as
    // jank) than it typically does on a desktop batch job.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;

    Ok(conn)
}

/// Opens an in-memory database — useful for tests, or for caches that
/// deliberately shouldn't survive a process restart.
pub fn open_in_memory() -> Result<Connection, SqliteError> {
    Ok(Connection::open_in_memory()?)
}

/// One schema migration: a version number and the SQL to run to move
/// the database from `version - 1` to `version`. Versions must be
/// listed in [`Migrations::new`] in ascending order starting from 1;
/// [`Migrations::apply`] runs every migration whose version is greater
/// than the database's currently recorded version, in order, inside a
/// single transaction per migration.
pub struct Migration {
    pub version: u32,
    pub sql: &'static str,
}

pub struct Migrations {
    migrations: Vec<Migration>,
}

impl Migrations {
    /// Builds a migration set from an ordered list of SQL statements,
    /// implicitly numbered 1, 2, 3, ... in the order given — the common
    /// case where migrations are just "everything my schema has ever
    /// needed, in the order I wrote it," without needing to track
    /// explicit version numbers by hand as the list grows.
    pub fn new(sql_statements: &[&'static str]) -> Self {
        let migrations = sql_statements
            .iter()
            .enumerate()
            .map(|(i, sql)| Migration {
                version: (i + 1) as u32,
                sql,
            })
            .collect();
        Self { migrations }
    }

    /// Applies every migration whose version is greater than the
    /// database's current recorded version, each inside its own
    /// transaction — a failure partway through a multi-statement
    /// migration rolls that one migration back rather than leaving the
    /// schema in a half-applied state, while migrations that already
    /// succeeded in a previous run stay applied.
    pub fn apply(&self, conn: &Connection) -> Result<(), SqliteError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS rusk_schema_version (version INTEGER NOT NULL)",
        )?;
        let current: u32 = conn
            .query_row("SELECT COALESCE(MAX(version), 0) FROM rusk_schema_version", [], |row| row.get(0))
            .unwrap_or(0);

        for migration in self.migrations.iter().filter(|m| m.version > current) {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(migration.sql).map_err(|source| SqliteError::Migration {
                version: migration.version,
                source,
            })?;
            tx.execute("INSERT INTO rusk_schema_version (version) VALUES (?1)", [migration.version])
                .map_err(|source| SqliteError::Migration {
                    version: migration.version,
                    source,
                })?;
            tx.commit().map_err(|source| SqliteError::Migration {
                version: migration.version,
                source,
            })?;
        }
        Ok(())
    }

    /// Convenience combining [`open_app_database`] and [`apply`](Self::apply)
    /// in one call — the common "open the app's database and make sure
    /// its schema is current" startup sequence.
    pub fn open_and_apply(&self, data_dir: &Path, file_name: &str) -> Result<Connection, SqliteError> {
        let conn = open_app_database(data_dir, file_name)?;
        self.apply(&conn)?;
        Ok(conn)
    }
}
