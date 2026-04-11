//! SQLite persistence layer: schema, migrations, paths, connection wrapper.

pub mod paths;
pub mod schema;

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

pub use paths::QuayDirs;

/// Owns the SQLite `Connection` for the lifetime of the app. Constructed once
/// at startup; clones are not allowed because rusqlite's `Connection` is not
/// `Sync`. Stores share access via `&Connection`.
pub struct Database {
    pub conn: Connection,
}

impl Database {
    /// Open or create the database file at `path`, enabling WAL mode and
    /// foreign-key enforcement, and run any pending schema migrations.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("open sqlite at {}", path.display()))?;
        Self::configure(&conn)?;
        schema::run_migrations(&conn)?;
        Ok(Self { conn })
    }

    /// Open an in-memory database for tests. Migrations are run immediately.
    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("open in-memory sqlite")?;
        // WAL is not applicable to in-memory DBs but foreign keys still are.
        conn.pragma_update(None, "foreign_keys", "ON")?;
        schema::run_migrations(&conn)?;
        Ok(Self { conn })
    }

    fn configure(conn: &Connection) -> Result<()> {
        // WAL gives us concurrent readers + a single writer with much lower
        // contention than the default rollback journal.
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("enable WAL mode")?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .context("enable foreign keys")?;
        // Tighter durability guarantees: synchronous=NORMAL is the WAL sweet
        // spot — durable on commit, no fsync on every write.
        conn.pragma_update(None, "synchronous", "NORMAL")
            .context("set synchronous=NORMAL")?;
        Ok(())
    }
}
