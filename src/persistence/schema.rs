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
pub const CURRENT_VERSION: i64 = 6;

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
    // v1 → v2: agent session fields on tasks. Enables per-task agent CLI
    // selection (Strategy pattern — Claude Code, OpenCode, or bare shell),
    // an initial `instructions` prompt for the agent, a worktree strategy,
    // and a session state badge.
    //
    // `DEFAULT 'claude'` on cli_selection means existing tasks upgraded
    // from v1 implicitly select Claude Code — the user can still override
    // per task later via the Settings dropdown (Fase 5).
    r#"
    ALTER TABLE tasks ADD COLUMN instructions TEXT;

    ALTER TABLE tasks ADD COLUMN cli_selection TEXT NOT NULL DEFAULT 'claude'
        CHECK (cli_selection IN ('claude','opencode','bare'));

    ALTER TABLE tasks ADD COLUMN start_mode TEXT
        CHECK (start_mode IN ('plan','implement'));

    ALTER TABLE tasks ADD COLUMN worktree_strategy TEXT NOT NULL DEFAULT 'create'
        CHECK (worktree_strategy IN ('create','none','select'));

    ALTER TABLE tasks ADD COLUMN session_state TEXT NOT NULL DEFAULT 'idle'
        CHECK (session_state IN ('idle','busy','awaiting','stopped','exited','error'));

    ALTER TABLE tasks ADD COLUMN process_pid INTEGER;
    "#,
    // v2 → v3: expand the `state` CHECK constraint to include Review and
    // Misc so the kanban can match Lanes' 6-column workflow.
    //
    // SQLite cannot ALTER an existing CHECK constraint, so we follow the
    // standard 6-step table recreate pattern: create a new table with the
    // updated constraint, copy data over with an explicit column list,
    // drop the old table, rename, recreate indexes. `run_migrations`
    // disables `foreign_keys` around the savepoint so dropping the
    // referenced `tasks` table does not trip the FK check from `sessions`.
    r#"
    CREATE TABLE tasks_v3 (
        id                TEXT PRIMARY KEY,
        title             TEXT NOT NULL,
        description       TEXT,
        instructions      TEXT,
        state             TEXT NOT NULL
                              CHECK (state IN ('backlog','planning','implementation','review','done','misc')),
        repo_path         TEXT NOT NULL,
        worktree_path     TEXT,
        branch_name       TEXT,
        agent_kind        TEXT NOT NULL,
        cli_selection     TEXT NOT NULL DEFAULT 'claude'
                              CHECK (cli_selection IN ('claude','opencode','bare')),
        start_mode        TEXT
                              CHECK (start_mode IN ('plan','implement')),
        worktree_strategy TEXT NOT NULL DEFAULT 'create'
                              CHECK (worktree_strategy IN ('create','none','select')),
        session_state     TEXT NOT NULL DEFAULT 'idle'
                              CHECK (session_state IN ('idle','busy','awaiting','stopped','exited','error')),
        process_pid       INTEGER,
        position          INTEGER NOT NULL,
        created_at        INTEGER NOT NULL,
        updated_at        INTEGER NOT NULL
    );

    INSERT INTO tasks_v3 (
        id, title, description, instructions, state, repo_path,
        worktree_path, branch_name, agent_kind, cli_selection, start_mode,
        worktree_strategy, session_state, process_pid, position,
        created_at, updated_at
    )
    SELECT
        id, title, description, instructions, state, repo_path,
        worktree_path, branch_name, agent_kind, cli_selection, start_mode,
        worktree_strategy, session_state, process_pid, position,
        created_at, updated_at
    FROM tasks;

    DROP TABLE tasks;
    ALTER TABLE tasks_v3 RENAME TO tasks;

    CREATE INDEX IF NOT EXISTS tasks_state_position
        ON tasks(state, position);
    "#,
    // v3 → v4: add `claude_session_id` so Phase 3's `--resume` path has
    // somewhere to persist the Claude Code session id between app runs.
    // Nullable because most tasks won't have one (fresh tasks before
    // their first session, or Opencode/Bare which don't support resume).
    r#"
    ALTER TABLE tasks ADD COLUMN claude_session_id TEXT;
    "#,
    // v4 → v5: labels + task dependencies for Phase 4.
    //
    // `labels` holds the name/color definitions (13 Lanes-style presets +
    // any user-created custom labels). `task_labels` is the many-to-many
    // join. `task_dependencies` is an explicit directed edge: B depends
    // on A means B cannot proceed until A is Done. Cycle detection is
    // enforced in Rust via a recursive CTE at insert time, not via a
    // SQLite constraint — SQLite has no way to express acyclicity.
    r#"
    CREATE TABLE IF NOT EXISTS labels (
        id    TEXT PRIMARY KEY,        -- uuid
        name  TEXT NOT NULL UNIQUE,
        color TEXT NOT NULL             -- hex #rrggbb
    );

    CREATE TABLE IF NOT EXISTS task_labels (
        task_id  TEXT NOT NULL REFERENCES tasks(id)  ON DELETE CASCADE,
        label_id TEXT NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
        PRIMARY KEY (task_id, label_id)
    );
    CREATE INDEX IF NOT EXISTS task_labels_label  ON task_labels(label_id);

    CREATE TABLE IF NOT EXISTS task_dependencies (
        task_id    TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
        depends_on TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
        PRIMARY KEY (task_id, depends_on),
        CHECK (task_id <> depends_on)   -- no self-edges
    );
    CREATE INDEX IF NOT EXISTS task_dependencies_deps ON task_dependencies(depends_on);
    "#,
    // v5 → v6: user settings (KV) + custom quick actions. Phase 5.
    //
    // `settings` is a simple key→value store. Everything is stored as
    // TEXT; complex values (JSON blobs, structured config) round-trip via
    // serde_json at the `settings::Settings` layer.
    //
    // `quick_actions` holds the user's custom shortcut commands. `kind`
    // distinguishes Claude-type (inject a prompt into the active session)
    // from Shell-type (run a shell command in the task's cwd). `position`
    // keeps the list ordering stable so Cmd+Alt+N always targets the same
    // action across runs.
    r#"
    CREATE TABLE IF NOT EXISTS settings (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS quick_actions (
        id        TEXT PRIMARY KEY,
        name      TEXT NOT NULL,
        kind      TEXT NOT NULL CHECK (kind IN ('claude', 'shell')),
        body      TEXT NOT NULL,
        category  TEXT NOT NULL DEFAULT 'general'
                  CHECK (category IN ('general', 'worktree')),
        position  INTEGER NOT NULL,
        created_at INTEGER NOT NULL
    );

    CREATE INDEX IF NOT EXISTS quick_actions_position ON quick_actions(position);
    "#,
];

/// Bring the database up to `CURRENT_VERSION`, applying any pending migrations
/// in order. Idempotent — calling it on an already-migrated DB is a no-op.
///
/// Foreign key enforcement is temporarily disabled around the savepoint
/// because some migrations (e.g. v3) recreate the `tasks` table, which
/// would otherwise trip the FK check from `sessions.task_id`. The pragma
/// is re-enabled unconditionally at the end so startup always leaves the
/// connection in the same state regardless of success/failure.
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

    // PRAGMA foreign_keys only takes effect OUTSIDE any active transaction
    // or savepoint, so we must flip it before starting the savepoint.
    conn.execute_batch("PRAGMA foreign_keys = OFF;")?;

    let result = migrate_inner(conn, current);

    // Always re-enable FK enforcement, even if the migration failed. This
    // leaves the connection in the expected steady state so subsequent
    // queries still enforce referential integrity.
    if let Err(err) = conn.execute_batch("PRAGMA foreign_keys = ON;") {
        tracing::warn!(%err, "failed to re-enable foreign_keys after migration");
    }

    result
}

fn migrate_inner(conn: &Connection, current: i64) -> Result<()> {
    let savepoint_name = "quay_migrate";
    conn.execute_batch(&format!("SAVEPOINT {savepoint_name};"))?;

    let result = (|| -> Result<()> {
        for (idx, sql) in MIGRATIONS.iter().enumerate() {
            let target = (idx + 1) as i64;
            if target > current {
                conn.execute_batch(sql)
                    .with_context(|| format!("apply migration v{target}"))?;
            }
        }

        conn.execute("DELETE FROM schema_version", [])
            .context("clear schema_version row")?;
        conn.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            rusqlite::params![CURRENT_VERSION],
        )
        .context("write schema_version row")?;

        Ok(())
    })();

    match result {
        Ok(()) => {
            conn.execute_batch(&format!("RELEASE {savepoint_name};"))?;
            tracing::info!(version = CURRENT_VERSION, "schema migrated");
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch(&format!("ROLLBACK TO {savepoint_name};"));
            let _ = conn.execute_batch(&format!("RELEASE {savepoint_name};"));
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

    #[test]
    fn migration_v2_adds_agent_session_columns() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        // All new columns should appear in the tasks table schema.
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(tasks)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .map(Result::unwrap)
            .collect();

        for expected in [
            "instructions",
            "cli_selection",
            "start_mode",
            "worktree_strategy",
            "session_state",
            "process_pid",
        ] {
            assert!(
                cols.contains(&expected.to_string()),
                "migration v2 missing column {expected}, got: {cols:?}"
            );
        }
    }

    #[test]
    fn migration_v2_default_cli_selection_is_claude() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        conn.execute(
            "INSERT INTO tasks (id, title, state, repo_path, agent_kind, position, created_at, updated_at)
             VALUES ('a', 't', 'backlog', '/tmp', 'claude', 0, 0, 0)",
            [],
        )
        .unwrap();

        let cli: String = conn
            .query_row("SELECT cli_selection FROM tasks WHERE id = 'a'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(cli, "claude");

        let strat: String = conn
            .query_row(
                "SELECT worktree_strategy FROM tasks WHERE id = 'a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(strat, "create");

        let sess: String = conn
            .query_row(
                "SELECT session_state FROM tasks WHERE id = 'a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sess, "idle");
    }

    #[test]
    fn migration_v2_rejects_unknown_cli_selection() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        let result = conn.execute(
            "INSERT INTO tasks (id, title, state, repo_path, agent_kind, cli_selection, position, created_at, updated_at)
             VALUES ('a', 't', 'backlog', '/tmp', 'claude', 'cursor', 0, 0, 0)",
            [],
        );
        assert!(result.is_err(), "CHECK should reject unknown cli_selection");
    }

    #[test]
    fn migration_v3_allows_review_and_misc_states() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        for state in ["backlog", "planning", "implementation", "review", "done", "misc"] {
            let result = conn.execute(
                "INSERT INTO tasks (id, title, state, repo_path, agent_kind, position, created_at, updated_at)
                 VALUES (?1, ?2, ?3, '/tmp', 'claude', 0, 0, 0)",
                rusqlite::params![state, state, state],
            );
            assert!(
                result.is_ok(),
                "state {state:?} should be accepted after v3 migration: {result:?}"
            );
        }
    }

    #[test]
    fn migration_v3_rejects_unknown_state_after_upgrade() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        let result = conn.execute(
            "INSERT INTO tasks (id, title, state, repo_path, agent_kind, position, created_at, updated_at)
             VALUES ('x', 'x', 'archived', '/tmp', 'claude', 0, 0, 0)",
            [],
        );
        assert!(result.is_err(), "'archived' is still not a valid state");
    }

    /// Going v1 → v3 in one shot: check that the table recreate in v3
    /// preserves the v2 rows we inserted along the way.
    #[test]
    fn migration_v1_to_v3_preserves_data() {
        let conn = Connection::open_in_memory().unwrap();

        // Set up an explicit v1 database with a row.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)",
            [],
        )
        .unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.execute("INSERT INTO schema_version (version) VALUES (1)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO tasks (id, title, state, repo_path, agent_kind, position, created_at, updated_at)
             VALUES ('legacy', 'Legacy v1 task', 'planning', '/tmp', 'claude', 3, 100, 200)",
            [],
        )
        .unwrap();

        // Jump to head.
        run_migrations(&conn).unwrap();

        let v: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(v, CURRENT_VERSION);

        // Legacy row survived the v3 table recreate.
        let (title, state, position, cli_selection, session_state): (String, String, i64, String, String) =
            conn.query_row(
                "SELECT title, state, position, cli_selection, session_state
                 FROM tasks WHERE id = 'legacy'",
                [],
                |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
                },
            )
            .unwrap();
        assert_eq!(title, "Legacy v1 task");
        assert_eq!(state, "planning");
        assert_eq!(position, 3);
        assert_eq!(cli_selection, "claude");
        assert_eq!(session_state, "idle");
    }

    #[test]
    fn migration_v3_cascades_preserved_for_sessions() {
        // After the v3 table recreate, the sessions.task_id -> tasks.id
        // foreign key with ON DELETE CASCADE should still be functional:
        // deleting a task removes its sessions.
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        conn.execute(
            "INSERT INTO tasks (id, title, state, repo_path, agent_kind, position, created_at, updated_at)
             VALUES ('t1', 'T', 'backlog', '/tmp', 'claude', 0, 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, task_id, pty_log_path, cols, rows, cwd, command, env_json, started_at)
             VALUES ('s1', 't1', '/tmp/s.bin', 80, 24, '/tmp', '[]', '{}', 0)",
            [],
        )
        .unwrap();

        conn.execute("DELETE FROM tasks WHERE id = 't1'", []).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions WHERE id = 's1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "session should have cascaded away with its task");
    }

    /// Simulate a v1 database upgrading to head (currently v3). Existing
    /// rows get the defaults on the newly added NOT NULL columns and
    /// survive the v3 table recreate with all values intact.
    #[test]
    fn migration_v1_to_head_preserves_existing_rows() {
        let conn = Connection::open_in_memory().unwrap();

        // Manually run just the v1 migration.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)",
            [],
        )
        .unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.execute("INSERT INTO schema_version (version) VALUES (1)", [])
            .unwrap();

        // Insert a row as if on v1 (no cli_selection column yet).
        conn.execute(
            "INSERT INTO tasks (id, title, state, repo_path, agent_kind, position, created_at, updated_at)
             VALUES ('old', 'Legacy task', 'backlog', '/tmp', 'claude', 0, 100, 100)",
            [],
        )
        .unwrap();

        // Now run the full migration — should apply v2 and v3 in sequence.
        run_migrations(&conn).unwrap();

        let v: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(v, CURRENT_VERSION);

        // Legacy row still present, defaults applied.
        let (title, cli, strat, sess): (String, String, String, String) = conn
            .query_row(
                "SELECT title, cli_selection, worktree_strategy, session_state FROM tasks WHERE id = 'old'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(title, "Legacy task");
        assert_eq!(cli, "claude");
        assert_eq!(strat, "create");
        assert_eq!(sess, "idle");
    }
}
