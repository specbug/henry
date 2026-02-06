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

            -- Reactive anomalies table
            CREATE TABLE IF NOT EXISTS anomalies (
                id TEXT PRIMARY KEY,
                event_type TEXT NOT NULL,
                event_data TEXT NOT NULL,
                severity TEXT NOT NULL,
                detected_at TEXT NOT NULL,
                resolved_at TEXT,
                resolved_by TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_anomalies_detected ON anomalies(detected_at);
            CREATE INDEX IF NOT EXISTS idx_anomalies_severity ON anomalies(severity);

            -- Reactive actions table
            CREATE TABLE IF NOT EXISTS actions (
                id TEXT PRIMARY KEY,
                anomaly_id TEXT NOT NULL,
                action_type TEXT NOT NULL,
                action_data TEXT NOT NULL,
                created_at TEXT NOT NULL,
                requires_approval INTEGER NOT NULL DEFAULT 0,
                approved_by TEXT,
                approved_at TEXT,
                executed_at TEXT,
                status TEXT NOT NULL DEFAULT 'pending',
                result TEXT,
                retries INTEGER NOT NULL DEFAULT 0,
                FOREIGN KEY (anomaly_id) REFERENCES anomalies(id)
            );

            CREATE INDEX IF NOT EXISTS idx_actions_anomaly ON actions(anomaly_id);
            CREATE INDEX IF NOT EXISTS idx_actions_status ON actions(status);
            CREATE INDEX IF NOT EXISTS idx_actions_created ON actions(created_at);

            -- Agent tasks table
            CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                description TEXT,
                created_at TEXT NOT NULL,
                created_by TEXT,
                priority TEXT NOT NULL DEFAULT 'normal',
                status TEXT NOT NULL DEFAULT 'queued',
                steps TEXT,
                current_step_index INTEGER DEFAULT 0,
                context TEXT,
                result TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);
            CREATE INDEX IF NOT EXISTS idx_tasks_created ON tasks(created_at);

            -- Agent task steps table
            CREATE TABLE IF NOT EXISTS task_steps (
                id TEXT PRIMARY KEY,
                task_id TEXT NOT NULL,
                step_index INTEGER NOT NULL,
                description TEXT NOT NULL,
                step_type TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                iterations TEXT,
                FOREIGN KEY (task_id) REFERENCES tasks(id)
            );

            CREATE INDEX IF NOT EXISTS idx_task_steps_task ON task_steps(task_id);

            -- Apps registry
            CREATE TABLE IF NOT EXISTS apps (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                repo_url TEXT NOT NULL,
                branch TEXT DEFAULT 'main',
                domain TEXT NOT NULL,
                path_prefix TEXT DEFAULT '/',
                container_name TEXT NOT NULL,
                image_name TEXT NOT NULL,
                port INTEGER DEFAULT 8080,
                env_vars TEXT,
                status TEXT DEFAULT 'pending',
                error_message TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                last_deployed TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_apps_name ON apps(name);
            CREATE INDEX IF NOT EXISTS idx_apps_status ON apps(status);

            -- Proxy routes
            CREATE TABLE IF NOT EXISTS proxy_routes (
                id TEXT PRIMARY KEY,
                domain TEXT NOT NULL,
                path_prefix TEXT DEFAULT '/',
                upstream TEXT NOT NULL,
                strip_prefix INTEGER DEFAULT 1,
                tls_enabled INTEGER DEFAULT 1,
                app_id TEXT,
                created_at TEXT NOT NULL,
                UNIQUE(domain, path_prefix)
            );

            CREATE INDEX IF NOT EXISTS idx_proxy_routes_domain ON proxy_routes(domain);
            CREATE INDEX IF NOT EXISTS idx_proxy_routes_app ON proxy_routes(app_id);

            -- Tunnel routes
            CREATE TABLE IF NOT EXISTS tunnel_routes (
                id TEXT PRIMARY KEY,
                hostname TEXT NOT NULL,
                path TEXT,
                service TEXT NOT NULL,
                created_at TEXT NOT NULL,
                UNIQUE(hostname, path)
            );

            CREATE INDEX IF NOT EXISTS idx_tunnel_routes_hostname ON tunnel_routes(hostname);

            -- App services (multi-service support)
            CREATE TABLE IF NOT EXISTS app_services (
                id TEXT PRIMARY KEY,
                app_id TEXT NOT NULL,
                name TEXT NOT NULL,
                service_type TEXT DEFAULT 'other',
                build_context TEXT,
                container_name TEXT NOT NULL,
                image_name TEXT NOT NULL,
                port INTEGER DEFAULT 8080,
                env_vars TEXT,
                status TEXT DEFAULT 'pending',
                error_message TEXT,
                path_prefix TEXT DEFAULT '/',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                UNIQUE(app_id, name)
            );

            CREATE INDEX IF NOT EXISTS idx_app_services_app ON app_services(app_id);
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

    // Anomaly operations

    /// Save an anomaly.
    pub async fn save_anomaly(&self, anomaly: &AnomalyRecord) -> Result<(), StateError> {
        sqlx::query(
            r#"
            INSERT INTO anomalies (id, event_type, event_data, severity, detected_at, resolved_at, resolved_by)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                resolved_at = excluded.resolved_at,
                resolved_by = excluded.resolved_by
            "#,
        )
        .bind(&anomaly.id)
        .bind(&anomaly.event_type)
        .bind(&anomaly.event_data)
        .bind(&anomaly.severity)
        .bind(&anomaly.detected_at)
        .bind(&anomaly.resolved_at)
        .bind(&anomaly.resolved_by)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Get an anomaly by ID.
    pub async fn get_anomaly(&self, id: &str) -> Result<Option<AnomalyRecord>, StateError> {
        let row = sqlx::query_as::<_, AnomalyRecord>(
            "SELECT id, event_type, event_data, severity, detected_at, resolved_at, resolved_by FROM anomalies WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row)
    }

    /// Get recent anomalies.
    pub async fn get_anomalies(&self, limit: i64, include_resolved: bool) -> Result<Vec<AnomalyRecord>, StateError> {
        let query = if include_resolved {
            "SELECT id, event_type, event_data, severity, detected_at, resolved_at, resolved_by FROM anomalies ORDER BY detected_at DESC LIMIT ?"
        } else {
            "SELECT id, event_type, event_data, severity, detected_at, resolved_at, resolved_by FROM anomalies WHERE resolved_at IS NULL ORDER BY detected_at DESC LIMIT ?"
        };

        let rows = sqlx::query_as::<_, AnomalyRecord>(query)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;

        Ok(rows)
    }

    // Action operations

    /// Save an action.
    pub async fn save_action(&self, action: &ActionRecord) -> Result<(), StateError> {
        sqlx::query(
            r#"
            INSERT INTO actions (id, anomaly_id, action_type, action_data, created_at, requires_approval, approved_by, approved_at, executed_at, status, result, retries)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                approved_by = excluded.approved_by,
                approved_at = excluded.approved_at,
                executed_at = excluded.executed_at,
                status = excluded.status,
                result = excluded.result,
                retries = excluded.retries
            "#,
        )
        .bind(&action.id)
        .bind(&action.anomaly_id)
        .bind(&action.action_type)
        .bind(&action.action_data)
        .bind(&action.created_at)
        .bind(action.requires_approval)
        .bind(&action.approved_by)
        .bind(&action.approved_at)
        .bind(&action.executed_at)
        .bind(&action.status)
        .bind(&action.result)
        .bind(action.retries)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Get an action by ID.
    pub async fn get_action(&self, id: &str) -> Result<Option<ActionRecord>, StateError> {
        let row = sqlx::query_as::<_, ActionRecord>(
            "SELECT id, anomaly_id, action_type, action_data, created_at, requires_approval, approved_by, approved_at, executed_at, status, result, retries FROM actions WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row)
    }

    /// Get recent actions.
    pub async fn get_actions(&self, limit: i64) -> Result<Vec<ActionRecord>, StateError> {
        let rows = sqlx::query_as::<_, ActionRecord>(
            "SELECT id, anomaly_id, action_type, action_data, created_at, requires_approval, approved_by, approved_at, executed_at, status, result, retries FROM actions ORDER BY created_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    /// Get pending actions (awaiting approval).
    pub async fn get_pending_actions(&self) -> Result<Vec<ActionRecord>, StateError> {
        let rows = sqlx::query_as::<_, ActionRecord>(
            "SELECT id, anomaly_id, action_type, action_data, created_at, requires_approval, approved_by, approved_at, executed_at, status, result, retries FROM actions WHERE status = 'awaiting_approval' ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    // Task operations

    /// Save a task.
    pub async fn save_task(&self, task: &TaskRecord) -> Result<(), StateError> {
        sqlx::query(
            r#"
            INSERT INTO tasks (id, title, description, created_at, created_by, priority, status, steps, current_step_index, context, result)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                status = excluded.status,
                steps = excluded.steps,
                current_step_index = excluded.current_step_index,
                context = excluded.context,
                result = excluded.result
            "#,
        )
        .bind(&task.id)
        .bind(&task.title)
        .bind(&task.description)
        .bind(&task.created_at)
        .bind(&task.created_by)
        .bind(&task.priority)
        .bind(&task.status)
        .bind(&task.steps)
        .bind(task.current_step_index)
        .bind(&task.context)
        .bind(&task.result)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Get a task by ID.
    pub async fn get_task(&self, id: &str) -> Result<Option<TaskRecord>, StateError> {
        let row = sqlx::query_as::<_, TaskRecord>(
            "SELECT id, title, description, created_at, created_by, priority, status, steps, current_step_index, context, result FROM tasks WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row)
    }

    /// Get all tasks.
    pub async fn get_tasks(&self, limit: i64) -> Result<Vec<TaskRecord>, StateError> {
        let rows = sqlx::query_as::<_, TaskRecord>(
            "SELECT id, title, description, created_at, created_by, priority, status, steps, current_step_index, context, result FROM tasks ORDER BY created_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    /// Get tasks by status.
    pub async fn get_tasks_by_status(&self, status: &str) -> Result<Vec<TaskRecord>, StateError> {
        let rows = sqlx::query_as::<_, TaskRecord>(
            "SELECT id, title, description, created_at, created_by, priority, status, steps, current_step_index, context, result FROM tasks WHERE status = ? ORDER BY created_at ASC",
        )
        .bind(status)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    /// Save a task step.
    pub async fn save_task_step(&self, step: &TaskStepRecord) -> Result<(), StateError> {
        sqlx::query(
            r#"
            INSERT INTO task_steps (id, task_id, step_index, description, step_type, status, iterations)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                status = excluded.status,
                iterations = excluded.iterations
            "#,
        )
        .bind(&step.id)
        .bind(&step.task_id)
        .bind(step.step_index)
        .bind(&step.description)
        .bind(&step.step_type)
        .bind(&step.status)
        .bind(&step.iterations)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Get task steps.
    pub async fn get_task_steps(&self, task_id: &str) -> Result<Vec<TaskStepRecord>, StateError> {
        let rows = sqlx::query_as::<_, TaskStepRecord>(
            "SELECT id, task_id, step_index, description, step_type, status, iterations FROM task_steps WHERE task_id = ? ORDER BY step_index ASC",
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    // ===== App operations =====

    /// Save an app.
    pub async fn save_app(&self, app: &AppRecord) -> Result<(), StateError> {
        sqlx::query(
            r#"
            INSERT INTO apps (id, name, repo_url, branch, domain, path_prefix, container_name, image_name, port, env_vars, status, error_message, created_at, updated_at, last_deployed)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                status = excluded.status,
                error_message = excluded.error_message,
                updated_at = excluded.updated_at,
                last_deployed = excluded.last_deployed
            "#,
        )
        .bind(&app.id)
        .bind(&app.name)
        .bind(&app.repo_url)
        .bind(&app.branch)
        .bind(&app.domain)
        .bind(&app.path_prefix)
        .bind(&app.container_name)
        .bind(&app.image_name)
        .bind(app.port)
        .bind(&app.env_vars)
        .bind(&app.status)
        .bind(&app.error_message)
        .bind(&app.created_at)
        .bind(&app.updated_at)
        .bind(&app.last_deployed)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Get an app by ID.
    pub async fn get_app(&self, id: &str) -> Result<Option<AppRecord>, StateError> {
        let row = sqlx::query_as::<_, AppRecord>(
            "SELECT id, name, repo_url, branch, domain, path_prefix, container_name, image_name, port, env_vars, status, error_message, created_at, updated_at, last_deployed FROM apps WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row)
    }

    /// Get an app by name.
    pub async fn get_app_by_name(&self, name: &str) -> Result<Option<AppRecord>, StateError> {
        let row = sqlx::query_as::<_, AppRecord>(
            "SELECT id, name, repo_url, branch, domain, path_prefix, container_name, image_name, port, env_vars, status, error_message, created_at, updated_at, last_deployed FROM apps WHERE name = ?",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row)
    }

    /// Get all apps.
    pub async fn get_apps(&self) -> Result<Vec<AppRecord>, StateError> {
        let rows = sqlx::query_as::<_, AppRecord>(
            "SELECT id, name, repo_url, branch, domain, path_prefix, container_name, image_name, port, env_vars, status, error_message, created_at, updated_at, last_deployed FROM apps ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    /// Delete an app.
    pub async fn delete_app(&self, id: &str) -> Result<bool, StateError> {
        let result = sqlx::query("DELETE FROM apps WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected() > 0)
    }

    // ===== Proxy route operations =====

    /// Save a proxy route.
    pub async fn save_proxy_route(&self, route: &ProxyRouteRecord) -> Result<(), StateError> {
        sqlx::query(
            r#"
            INSERT INTO proxy_routes (id, domain, path_prefix, upstream, strip_prefix, tls_enabled, app_id, created_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                upstream = excluded.upstream,
                strip_prefix = excluded.strip_prefix,
                tls_enabled = excluded.tls_enabled
            "#,
        )
        .bind(&route.id)
        .bind(&route.domain)
        .bind(&route.path_prefix)
        .bind(&route.upstream)
        .bind(route.strip_prefix)
        .bind(route.tls_enabled)
        .bind(&route.app_id)
        .bind(&route.created_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Get a proxy route by ID.
    pub async fn get_proxy_route(&self, id: &str) -> Result<Option<ProxyRouteRecord>, StateError> {
        let row = sqlx::query_as::<_, ProxyRouteRecord>(
            "SELECT id, domain, path_prefix, upstream, strip_prefix, tls_enabled, app_id, created_at FROM proxy_routes WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row)
    }

    /// Get all proxy routes.
    pub async fn get_proxy_routes(&self) -> Result<Vec<ProxyRouteRecord>, StateError> {
        let rows = sqlx::query_as::<_, ProxyRouteRecord>(
            "SELECT id, domain, path_prefix, upstream, strip_prefix, tls_enabled, app_id, created_at FROM proxy_routes ORDER BY domain, path_prefix",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    /// Delete a proxy route.
    pub async fn delete_proxy_route(&self, id: &str) -> Result<bool, StateError> {
        let result = sqlx::query("DELETE FROM proxy_routes WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected() > 0)
    }

    // ===== App service operations =====

    /// Save an app service.
    pub async fn save_app_service(&self, svc: &AppServiceRecord) -> Result<(), StateError> {
        sqlx::query(
            r#"
            INSERT INTO app_services (id, app_id, name, service_type, build_context, container_name, image_name, port, env_vars, status, error_message, path_prefix, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                status = excluded.status,
                error_message = excluded.error_message,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(&svc.id)
        .bind(&svc.app_id)
        .bind(&svc.name)
        .bind(&svc.service_type)
        .bind(&svc.build_context)
        .bind(&svc.container_name)
        .bind(&svc.image_name)
        .bind(svc.port)
        .bind(&svc.env_vars)
        .bind(&svc.status)
        .bind(&svc.error_message)
        .bind(&svc.path_prefix)
        .bind(&svc.created_at)
        .bind(&svc.updated_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Get all services for an app.
    pub async fn get_app_services(&self, app_id: &str) -> Result<Vec<AppServiceRecord>, StateError> {
        let rows = sqlx::query_as::<_, AppServiceRecord>(
            "SELECT id, app_id, name, service_type, build_context, container_name, image_name, port, env_vars, status, error_message, path_prefix, created_at, updated_at FROM app_services WHERE app_id = ? ORDER BY name",
        )
        .bind(app_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    /// Delete all services for an app.
    pub async fn delete_app_services(&self, app_id: &str) -> Result<usize, StateError> {
        let result = sqlx::query("DELETE FROM app_services WHERE app_id = ?")
            .bind(app_id)
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected() as usize)
    }

    /// Delete proxy routes by app ID.
    pub async fn delete_proxy_routes_by_app(&self, app_id: &str) -> Result<usize, StateError> {
        let result = sqlx::query("DELETE FROM proxy_routes WHERE app_id = ?")
            .bind(app_id)
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected() as usize)
    }

    // ===== Tunnel route operations =====

    /// Save a tunnel route.
    pub async fn save_tunnel_route(&self, route: &TunnelRouteRecord) -> Result<(), StateError> {
        sqlx::query(
            r#"
            INSERT INTO tunnel_routes (id, hostname, path, service, created_at)
            VALUES (?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                service = excluded.service
            "#,
        )
        .bind(&route.id)
        .bind(&route.hostname)
        .bind(&route.path)
        .bind(&route.service)
        .bind(&route.created_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Get all tunnel routes.
    pub async fn get_tunnel_routes(&self) -> Result<Vec<TunnelRouteRecord>, StateError> {
        let rows = sqlx::query_as::<_, TunnelRouteRecord>(
            "SELECT id, hostname, path, service, created_at FROM tunnel_routes ORDER BY hostname, path",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    /// Delete a tunnel route.
    pub async fn delete_tunnel_route(&self, id: &str) -> Result<bool, StateError> {
        let result = sqlx::query("DELETE FROM tunnel_routes WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected() > 0)
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

/// Anomaly record for persistence.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AnomalyRecord {
    pub id: String,
    pub event_type: String,
    pub event_data: String,
    pub severity: String,
    pub detected_at: String,
    pub resolved_at: Option<String>,
    pub resolved_by: Option<String>,
}

impl AnomalyRecord {
    pub fn new(id: impl Into<String>, event_type: impl Into<String>, event_data: impl Into<String>, severity: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            event_type: event_type.into(),
            event_data: event_data.into(),
            severity: severity.into(),
            detected_at: Utc::now().to_rfc3339(),
            resolved_at: None,
            resolved_by: None,
        }
    }

    pub fn resolve(&mut self, resolved_by: impl Into<String>) {
        self.resolved_at = Some(Utc::now().to_rfc3339());
        self.resolved_by = Some(resolved_by.into());
    }
}

/// Action record for persistence.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ActionRecord {
    pub id: String,
    pub anomaly_id: String,
    pub action_type: String,
    pub action_data: String,
    pub created_at: String,
    pub requires_approval: bool,
    pub approved_by: Option<String>,
    pub approved_at: Option<String>,
    pub executed_at: Option<String>,
    pub status: String,
    pub result: Option<String>,
    pub retries: i32,
}

impl ActionRecord {
    pub fn new(id: impl Into<String>, anomaly_id: impl Into<String>, action_type: impl Into<String>, action_data: impl Into<String>, requires_approval: bool) -> Self {
        Self {
            id: id.into(),
            anomaly_id: anomaly_id.into(),
            action_type: action_type.into(),
            action_data: action_data.into(),
            created_at: Utc::now().to_rfc3339(),
            requires_approval,
            approved_by: None,
            approved_at: None,
            executed_at: None,
            status: if requires_approval { "awaiting_approval" } else { "pending" }.to_string(),
            result: None,
            retries: 0,
        }
    }
}

/// Task record for persistence.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TaskRecord {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub created_at: String,
    pub created_by: Option<String>,
    pub priority: String,
    pub status: String,
    pub steps: Option<String>,
    pub current_step_index: i32,
    pub context: Option<String>,
    pub result: Option<String>,
}

impl TaskRecord {
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            description: None,
            created_at: Utc::now().to_rfc3339(),
            created_by: None,
            priority: "normal".to_string(),
            status: "queued".to_string(),
            steps: None,
            current_step_index: 0,
            context: None,
            result: None,
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn with_priority(mut self, priority: impl Into<String>) -> Self {
        self.priority = priority.into();
        self
    }

    pub fn with_created_by(mut self, created_by: impl Into<String>) -> Self {
        self.created_by = Some(created_by.into());
        self
    }
}

/// Task step record for persistence.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TaskStepRecord {
    pub id: String,
    pub task_id: String,
    pub step_index: i32,
    pub description: String,
    pub step_type: String,
    pub status: String,
    pub iterations: Option<String>,
}

impl TaskStepRecord {
    pub fn new(id: impl Into<String>, task_id: impl Into<String>, step_index: i32, description: impl Into<String>, step_type: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            task_id: task_id.into(),
            step_index,
            description: description.into(),
            step_type: step_type.into(),
            status: "pending".to_string(),
            iterations: None,
        }
    }
}

/// App record for persistence.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AppRecord {
    pub id: String,
    pub name: String,
    pub repo_url: String,
    pub branch: String,
    pub domain: String,
    pub path_prefix: String,
    pub container_name: String,
    pub image_name: String,
    pub port: i32,
    pub env_vars: Option<String>,
    pub status: String,
    pub error_message: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub last_deployed: Option<String>,
}

impl AppRecord {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        repo_url: impl Into<String>,
        domain: impl Into<String>,
        path_prefix: impl Into<String>,
        container_name: impl Into<String>,
        image_name: impl Into<String>,
    ) -> Self {
        let now = Utc::now().to_rfc3339();
        Self {
            id: id.into(),
            name: name.into(),
            repo_url: repo_url.into(),
            branch: "main".to_string(),
            domain: domain.into(),
            path_prefix: path_prefix.into(),
            container_name: container_name.into(),
            image_name: image_name.into(),
            port: 8080,
            env_vars: None,
            status: "pending".to_string(),
            error_message: None,
            created_at: now.clone(),
            updated_at: now,
            last_deployed: None,
        }
    }

    pub fn with_branch(mut self, branch: impl Into<String>) -> Self {
        self.branch = branch.into();
        self
    }

    pub fn with_port(mut self, port: i32) -> Self {
        self.port = port;
        self
    }

    pub fn with_env_vars(mut self, env_vars: impl Into<String>) -> Self {
        self.env_vars = Some(env_vars.into());
        self
    }
}

/// App service record for persistence.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AppServiceRecord {
    pub id: String,
    pub app_id: String,
    pub name: String,
    pub service_type: String,
    pub build_context: Option<String>,
    pub container_name: String,
    pub image_name: String,
    pub port: i32,
    pub env_vars: Option<String>,
    pub status: String,
    pub error_message: Option<String>,
    pub path_prefix: String,
    pub created_at: String,
    pub updated_at: String,
}

impl AppServiceRecord {
    pub fn new(
        id: impl Into<String>,
        app_id: impl Into<String>,
        name: impl Into<String>,
        container_name: impl Into<String>,
        image_name: impl Into<String>,
    ) -> Self {
        let now = Utc::now().to_rfc3339();
        Self {
            id: id.into(),
            app_id: app_id.into(),
            name: name.into(),
            service_type: "other".to_string(),
            build_context: None,
            container_name: container_name.into(),
            image_name: image_name.into(),
            port: 8080,
            env_vars: None,
            status: "pending".to_string(),
            error_message: None,
            path_prefix: "/".to_string(),
            created_at: now.clone(),
            updated_at: now,
        }
    }

    pub fn with_service_type(mut self, service_type: impl Into<String>) -> Self {
        self.service_type = service_type.into();
        self
    }

    pub fn with_port(mut self, port: i32) -> Self {
        self.port = port;
        self
    }

    pub fn with_path_prefix(mut self, path_prefix: impl Into<String>) -> Self {
        self.path_prefix = path_prefix.into();
        self
    }
}

/// Proxy route record for persistence.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProxyRouteRecord {
    pub id: String,
    pub domain: String,
    pub path_prefix: String,
    pub upstream: String,
    pub strip_prefix: bool,
    pub tls_enabled: bool,
    pub app_id: Option<String>,
    pub created_at: String,
}

impl ProxyRouteRecord {
    pub fn new(
        id: impl Into<String>,
        domain: impl Into<String>,
        path_prefix: impl Into<String>,
        upstream: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            domain: domain.into(),
            path_prefix: path_prefix.into(),
            upstream: upstream.into(),
            strip_prefix: true,
            tls_enabled: true,
            app_id: None,
            created_at: Utc::now().to_rfc3339(),
        }
    }

    pub fn with_app(mut self, app_id: impl Into<String>) -> Self {
        self.app_id = Some(app_id.into());
        self
    }
}

/// Tunnel route record for persistence.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TunnelRouteRecord {
    pub id: String,
    pub hostname: String,
    pub path: Option<String>,
    pub service: String,
    pub created_at: String,
}

impl TunnelRouteRecord {
    pub fn new(
        id: impl Into<String>,
        hostname: impl Into<String>,
        service: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            hostname: hostname.into(),
            path: None,
            service: service.into(),
            created_at: Utc::now().to_rfc3339(),
        }
    }

    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }
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
