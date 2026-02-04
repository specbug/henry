//! SQLite state management for Henry daemon.
//!
//! Provides persistent state storage for modules, sessions, and audit logs.

use chrono::Utc;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use thiserror::Error;
use tracing::{debug, info};

#[derive(Error, Debug)]
pub enum StateError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("migration error: {0}")]
    Migration(String),

    #[error("failed to create state directory: {0}")]
    CreateDir(#[source] std::io::Error),

    #[error("record not found: {0}")]
    NotFound(String),
}

/// State manager with SQLite backend.
pub struct StateManager {
    pool: SqlitePool,
    path: PathBuf,
}

impl StateManager {
    /// Create a new state manager, initializing the database if needed.
    pub async fn new(path: impl AsRef<Path>) -> Result<Self, StateError> {
        let path = path.as_ref().to_path_buf();

        // Create directory if needed
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(StateError::CreateDir)?;
        }

        // Set file permissions on Unix
        #[cfg(unix)]
        if path.exists() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }

        let options = SqliteConnectOptions::from_str(&format!("sqlite:{}?mode=rwc", path.display()))
            .map_err(|e| StateError::Database(e))?
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;

        let manager = Self { pool, path };
        manager.run_migrations().await?;

        info!("State database initialized at {}", manager.path.display());
        Ok(manager)
    }

    /// Run database migrations.
    async fn run_migrations(&self) -> Result<(), StateError> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS module_state (
                module TEXT PRIMARY KEY,
                enabled INTEGER NOT NULL DEFAULT 1,
                status TEXT NOT NULL DEFAULT 'stopped',
                last_error TEXT,
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS kv_store (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS audit_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL DEFAULT (datetime('now')),
                module TEXT NOT NULL,
                action TEXT NOT NULL,
                details TEXT,
                user_id TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_audit_timestamp ON audit_log(timestamp);
            CREATE INDEX IF NOT EXISTS idx_audit_module ON audit_log(module);
            "#,
        )
        .execute(&self.pool)
        .await?;

        debug!("Database migrations completed");
        Ok(())
    }

    /// Get the database path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    // Module state operations

    /// Get module state.
    pub async fn get_module_state(&self, module: &str) -> Result<Option<ModuleState>, StateError> {
        let row = sqlx::query_as::<_, ModuleState>(
            "SELECT module, enabled, status, last_error, updated_at FROM module_state WHERE module = ?",
        )
        .bind(module)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row)
    }

    /// Set module state.
    pub async fn set_module_state(&self, state: &ModuleState) -> Result<(), StateError> {
        sqlx::query(
            r#"
            INSERT INTO module_state (module, enabled, status, last_error, updated_at)
            VALUES (?, ?, ?, ?, datetime('now'))
            ON CONFLICT(module) DO UPDATE SET
                enabled = excluded.enabled,
                status = excluded.status,
                last_error = excluded.last_error,
                updated_at = datetime('now')
            "#,
        )
        .bind(&state.module)
        .bind(state.enabled)
        .bind(&state.status)
        .bind(&state.last_error)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Get all module states.
    pub async fn get_all_module_states(&self) -> Result<Vec<ModuleState>, StateError> {
        let rows = sqlx::query_as::<_, ModuleState>(
            "SELECT module, enabled, status, last_error, updated_at FROM module_state ORDER BY module",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    // Key-value store operations

    /// Get a value from the key-value store.
    pub async fn kv_get(&self, key: &str) -> Result<Option<String>, StateError> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT value FROM kv_store WHERE key = ?")
                .bind(key)
                .fetch_optional(&self.pool)
                .await?;

        Ok(row.map(|(v,)| v))
    }

    /// Set a value in the key-value store.
    pub async fn kv_set(&self, key: &str, value: &str) -> Result<(), StateError> {
        sqlx::query(
            r#"
            INSERT INTO kv_store (key, value, updated_at)
            VALUES (?, ?, datetime('now'))
            ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                updated_at = datetime('now')
            "#,
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Delete a key from the key-value store.
    pub async fn kv_delete(&self, key: &str) -> Result<bool, StateError> {
        let result = sqlx::query("DELETE FROM kv_store WHERE key = ?")
            .bind(key)
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected() > 0)
    }

    // Audit log operations

    /// Log an audit event.
    pub async fn audit_log(
        &self,
        module: &str,
        action: &str,
        details: Option<&str>,
        user_id: Option<&str>,
    ) -> Result<(), StateError> {
        sqlx::query(
            "INSERT INTO audit_log (module, action, details, user_id) VALUES (?, ?, ?, ?)",
        )
        .bind(module)
        .bind(action)
        .bind(details)
        .bind(user_id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Get recent audit log entries.
    pub async fn get_audit_log(&self, limit: i64) -> Result<Vec<AuditEntry>, StateError> {
        let rows = sqlx::query_as::<_, AuditEntry>(
            "SELECT id, timestamp, module, action, details, user_id FROM audit_log ORDER BY id DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    /// Get the pool for advanced operations.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

/// Module state record.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ModuleState {
    pub module: String,
    pub enabled: bool,
    pub status: String,
    pub last_error: Option<String>,
    pub updated_at: String,
}

impl ModuleState {
    pub fn new(module: impl Into<String>) -> Self {
        Self {
            module: module.into(),
            enabled: true,
            status: "stopped".to_string(),
            last_error: None,
            updated_at: Utc::now().to_rfc3339(),
        }
    }

    pub fn with_status(mut self, status: impl Into<String>) -> Self {
        self.status = status.into();
        self
    }

    pub fn with_error(mut self, error: impl Into<String>) -> Self {
        self.last_error = Some(error.into());
        self
    }

    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }
}

/// Audit log entry.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AuditEntry {
    pub id: i64,
    pub timestamp: String,
    pub module: String,
    pub action: String,
    pub details: Option<String>,
    pub user_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_module_state() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");

        let state = StateManager::new(&db_path).await.unwrap();

        let module = ModuleState::new("test-module").with_status("running");
        state.set_module_state(&module).await.unwrap();

        let loaded = state.get_module_state("test-module").await.unwrap().unwrap();
        assert_eq!(loaded.status, "running");
        assert!(loaded.enabled);
    }

    #[tokio::test]
    async fn test_kv_store() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");

        let state = StateManager::new(&db_path).await.unwrap();

        state.kv_set("key1", "value1").await.unwrap();
        let value = state.kv_get("key1").await.unwrap();
        assert_eq!(value, Some("value1".to_string()));

        state.kv_delete("key1").await.unwrap();
        let value = state.kv_get("key1").await.unwrap();
        assert!(value.is_none());
    }

    #[tokio::test]
    async fn test_audit_log() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");

        let state = StateManager::new(&db_path).await.unwrap();

        state
            .audit_log("test", "started", Some("test details"), None)
            .await
            .unwrap();

        let entries = state.get_audit_log(10).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].action, "started");
    }
}
