//! SQLite schema + migrations.
//!
//! Source of truth for the data model. New schema changes go at the end of the
//! `MIGRATIONS` list as additional `&str` entries — we never edit a previously
//! shipped migration.
//!
//! On startup `run_migrations` reads the current `schema_version`, runs every
//! pending migration in order inside a single transaction, and bumps the
//! version on success.

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Current target schema version. Equals the number of migrations below.
pub const CURRENT_VERSION: i64 = 1;

/// Each entry corresponds to one schema version. Index 0 is migration v0→v1,
/// index 1 is v1→v2, etc. Each script must be idempotent in the sense that
/// re-running it after a crash mid-migration cannot break anything.
const MIGRATIONS: &[&str] = &[
    // v0 → v1: initial schema. Tasks for the kanban + sessions for the PTY
    // panes that hang off them.
    r#"
    CREATE TABLE IF NOT EXISTS tasks (
        id            TEXT PRIMARY KEY,
        title         TEXT NOT NULL,
        description   TEXT,
        state         TEXT NOT NULL
                          CHECK (state IN ('backlog','planning','implementation','done')),
        repo_path     TEXT NOT NULL,
        worktree_path TEXT,
        branch_name   TEXT,
        agent_kind    TEXT NOT NULL,
        position      INTEGER NOT NULL,
        created_at    INTEGER NOT NULL,
        updated_at    INTEGER NOT NULL
    );

    CREATE INDEX IF NOT EXISTS tasks_state_position
        ON tasks(state, position);

    CREATE TABLE IF NOT EXISTS sessions (
        id            TEXT PRIMARY KEY,
        task_id       TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
        pty_log_path  TEXT NOT NULL,
        cols          INTEGER NOT NULL,
        rows          INTEGER NOT NULL,
        cwd           TEXT NOT NULL,
        command       TEXT NOT NULL,
        env_json      TEXT NOT NULL,
        exit_status   INTEGER,
        started_at    INTEGER NOT NULL,
        ended_at      INTEGER
    );

    CREATE INDEX IF NOT EXISTS sessions_task ON sessions(task_id);
    "#,
];

/// Bring the database up to `CURRENT_VERSION`, applying any pending migrations
/// in order. Idempotent — calling it on an already-migrated DB is a no-op.
pub fn run_migrations(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)",
        [],
    )
    .context("ensure schema_version table")?;

    let current: i64 = conn
        .query_row(
            "SELECT version FROM schema_version LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    if current >= CURRENT_VERSION {
        return Ok(());
    }

    let tx_conn = conn;
    let savepoint_name = "quay_migrate";
    tx_conn.execute_batch(&format!("SAVEPOINT {savepoint_name};"))?;

    let result = (|| -> Result<()> {
        for (idx, sql) in MIGRATIONS.iter().enumerate() {
            let target = (idx + 1) as i64;
            if target > current {
                tx_conn
                    .execute_batch(sql)
                    .with_context(|| format!("apply migration v{target}"))?;
            }
        }

        tx_conn
            .execute("DELETE FROM schema_version", [])
            .context("clear schema_version row")?;
        tx_conn
            .execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                rusqlite::params![CURRENT_VERSION],
            )
            .context("write schema_version row")?;

        Ok(())
    })();

    match result {
        Ok(()) => {
            tx_conn.execute_batch(&format!("RELEASE {savepoint_name};"))?;
            tracing::info!(
                version = CURRENT_VERSION,
                "schema migrated"
            );
            Ok(())
        }
        Err(e) => {
            let _ = tx_conn.execute_batch(&format!("ROLLBACK TO {savepoint_name};"));
            let _ = tx_conn.execute_batch(&format!("RELEASE {savepoint_name};"));
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn migrate_in_memory_creates_tables() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        // Tables exist.
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(tables.contains(&"tasks".to_string()));
        assert!(tables.contains(&"sessions".to_string()));
        assert!(tables.contains(&"schema_version".to_string()));

        // schema_version row written.
        let v: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(v, CURRENT_VERSION);
    }

    #[test]
    fn migrate_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        run_migrations(&conn).unwrap();
        run_migrations(&conn).unwrap();
        let v: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(v, CURRENT_VERSION);
    }

    #[test]
    fn check_constraint_rejects_unknown_state() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        let result = conn.execute(
            "INSERT INTO tasks (id, title, state, repo_path, agent_kind, position, created_at, updated_at)
             VALUES ('id', 'title', 'bogus', '/tmp', 'claude', 0, 0, 0)",
            [],
        );
        assert!(result.is_err(), "CHECK should reject unknown state");
    }
}
