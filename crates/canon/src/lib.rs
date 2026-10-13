//! Canon: what is true, by definition, about what should be running.
//!
//! One SQLite file holds every app and its lease, the routes it claims,
//! each release, the job queue, the event log and the sealed secrets. Plumb
//! reads it to decide what to do. Nothing else is the source of truth.
//!
//! Henry is crash-only. A step is written here before it is taken, so after a
//! power cut [`Canon::requeue_interrupted`] puts every half-done job back
//! on the queue and the daemon simply carries on.

mod adopt;
mod ark;
mod draft;
mod mend;
mod schema;

use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use lease::Lease;
use rusqlite::{Connection, OptionalExtension, Row, params};

pub use adopt::Adoption;
pub use ark::{Backup, Drill};
pub use draft::{Draft, NewProposal, Proposal};
pub use mend::{BenchRun, Tool, Upkeep};
pub use rusqlite;
pub use schema::LATEST as SCHEMA_VERSION;
pub use serde_json;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("an app named `{0}` already exists")]
    AppExists(String),
    #[error("no app named `{0}`")]
    NoSuchApp(String),
    #[error("{hostname}{path} is already routed to `{app}`")]
    RouteTaken { hostname: String, path: String, app: String },
    #[error("no {kind} with id {id}")]
    NotFound { kind: &'static str, id: i64 },
    #[error("stored {0} is unreadable: {1}")]
    Corrupt(&'static str, String),
    #[error("no free host port left in the range")]
    PortsExhausted,
    #[error("the database lock was poisoned by a panic")]
    Poisoned,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Implements `as_str` / `parse` for the small closed sets stored as text.
macro_rules! text_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident = $text:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name { $($variant),+ }

        impl $name {
            pub fn as_str(self) -> &'static str {
                match self { $($name::$variant => $text),+ }
            }

            pub fn parse(s: &str) -> Option<$name> {
                match s { $($text => Some($name::$variant),)+ _ => None }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

text_enum! {
    /// Who wrote the lease Henry is running.
    Source { Repo = "repo", Sound = "sound", Ghost = "ghost", Operator = "operator" }
}

text_enum! {
    /// What you want for an app. The Plumb closes the gap to this.
    Desired { Running = "running", Stopped = "stopped" }
}

text_enum! {
    /// What the app is, as the UI says it.
    Status {
        Onboarding = "onboarding",
        NeedsYou = "needs_you",
        Live = "live",
        Degraded = "degraded",
        Down = "down",
        Stopped = "stopped",
        Stuck = "stuck",
    }
}

text_enum! {
    ReleaseState {
        Pending = "pending",
        Building = "building",
        Trial = "trial",
        Proof = "proof",
        Live = "live",
        Failed = "failed",
        Superseded = "superseded",
    }
}

text_enum! {
    /// Where an onboarding stands. Build and Trial watch Press and Proof;
    /// the others are Draft's own steps.
    DraftStage {
        Survey = "survey",
        Plan = "plan",
        Build = "build",
        Trial = "trial",
        Live = "live",
        Stuck = "stuck",
    }
}

text_enum! {
    JobState { Queued = "queued", Running = "running", Done = "done", Failed = "failed" }
}

#[derive(Debug, Clone, PartialEq)]
pub struct App {
    pub name: String,
    pub repo: String,
    pub lease: String,
    pub source: Source,
    pub desired: Desired,
    pub status: Status,
    pub status_detail: Option<String>,
    pub live_release: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl App {
    /// The stored lease, parsed. It was checked when written, so failure
    /// here means the limits tightened since, which is worth surfacing.
    pub fn lease(&self) -> Result<Lease> {
        Lease::parse(&self.lease).map_err(|e| Error::Corrupt("lease", e.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub id: i64,
    pub app: String,
    pub commit_sha: String,
    pub lease: String,
    pub state: ReleaseState,
    pub images: serde_json::Value,
    pub reason: Option<String>,
    pub created_at: i64,
    pub finished_at: Option<i64>,
}

/// What Plumb should run for one app.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Wanted {
    pub live: Option<Release>,
    /// A newer release being tried beside the live one.
    pub candidate: Option<Release>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub id: i64,
    pub kind: String,
    pub app: Option<String>,
    pub state: JobState,
    pub attempts: i64,
    pub payload: serde_json::Value,
    pub error: Option<String>,
    pub not_before: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub id: i64,
    pub at: i64,
    pub app: Option<String>,
    pub kind: String,
    pub message: String,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub hostname: String,
    pub path: String,
    pub app: String,
    pub service: String,
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

pub struct Canon {
    conn: Mutex<Connection>,
}

impl Canon {
    /// Open (creating if needed) the record at `path`, readable only by you.
    pub fn open(path: &Path) -> Result<Canon> {
        let conn = Connection::open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Canon::init(conn)
    }

    pub fn open_in_memory() -> Result<Canon> {
        Canon::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Canon> {
        // FULL, not NORMAL: the Mac can lose power, and a step must not be
        // forgotten after Henry was told it was written.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )?;
        schema::migrate(&conn)?;
        Ok(Canon { conn: Mutex::new(conn) })
    }

    fn conn(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn.lock().map_err(|_| Error::Poisoned)
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self.conn()?.query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    /// Write a consistent copy of the whole record to `dest`, for Ark.
    pub fn snapshot_to(&self, dest: &Path) -> Result<()> {
        let dest = dest.to_str().ok_or_else(|| Error::Corrupt("path", dest.display().to_string()))?;
        self.conn()?.execute("VACUUM INTO ?1", [dest])?;
        Ok(())
    }

    // --- apps ------------------------------------------------------------

    /// Register an app with a checked lease. Its routes are claimed in the
    /// same transaction, so two apps can never hold one hostname.
    pub fn insert_app(&self, lease: &Lease, text: &str, repo: &str, source: Source) -> Result<App> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let now = now_ms();
        let inserted = tx.execute(
            "INSERT INTO apps (name, repo, lease, source, desired, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'running', 'onboarding', ?5, ?5)
             ON CONFLICT (name) DO NOTHING",
            params![lease.name, repo, text, source.as_str(), now],
        )?;
        if inserted == 0 {
            return Err(Error::AppExists(lease.name.clone()));
        }
        claim_routes(&tx, lease)?;
        record(
            &tx,
            Some(&lease.name),
            "app.registered",
            &format!("registered from {repo}"),
            &serde_json::json!({ "source": source.as_str() }),
        )?;
        let app = get_app(&tx, &lease.name)?;
        tx.commit()?;
        Ok(app)
    }

    /// Replace an app's lease. Routes are re-claimed to match it.
    pub fn update_lease(&self, lease: &Lease, text: &str, source: Source) -> Result<App> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "UPDATE apps SET lease = ?2, source = ?3, updated_at = ?4 WHERE name = ?1",
            params![lease.name, text, source.as_str(), now_ms()],
        )?;
        if changed == 0 {
            return Err(Error::NoSuchApp(lease.name.clone()));
        }
        tx.execute("DELETE FROM routes WHERE app = ?1", [&lease.name])?;
        claim_routes(&tx, lease)?;
        record(
            &tx,
            Some(&lease.name),
            "app.lease",
            "lease updated",
            &serde_json::json!({ "source": source.as_str() }),
        )?;
        let app = get_app(&tx, &lease.name)?;
        tx.commit()?;
        Ok(app)
    }

    pub fn app(&self, name: &str) -> Result<App> {
        get_app(&*self.conn()?, name)
    }

    pub fn apps(&self) -> Result<Vec<App>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(&format!("SELECT {APP_COLS} FROM apps ORDER BY name"))?;
        let rows = stmt.query_map([], app_row)?;
        rows.map(|r| r?).collect()
    }

    pub fn set_desired(&self, name: &str, desired: Desired) -> Result<()> {
        let conn = self.conn()?;
        let changed = conn.execute(
            "UPDATE apps SET desired = ?2, updated_at = ?3 WHERE name = ?1",
            params![name, desired.as_str(), now_ms()],
        )?;
        if changed == 0 {
            return Err(Error::NoSuchApp(name.to_owned()));
        }
        record(&conn, Some(name), "app.desired", &format!("desired state is now {desired}"), &serde_json::Value::Null)?;
        Ok(())
    }

    /// Record a new status. An unchanged status writes nothing, so callers
    /// may report on every pass without flooding the event log.
    pub fn set_status(&self, name: &str, status: Status, detail: Option<&str>) -> Result<bool> {
        let conn = self.conn()?;
        let changed = conn.execute(
            "UPDATE apps SET status = ?2, status_detail = ?3, updated_at = ?4
             WHERE name = ?1 AND (status IS NOT ?2 OR status_detail IS NOT ?3)",
            params![name, status.as_str(), detail, now_ms()],
        )?;
        if changed == 0 {
            get_app(&conn, name)?;
            return Ok(false);
        }
        let message = match detail {
            Some(d) => format!("{status}: {d}"),
            None => status.to_string(),
        };
        record(&conn, Some(name), "app.status", &message, &serde_json::json!({ "status": status.as_str() }))?;
        Ok(true)
    }

    /// Every status an app has had since `since`, oldest first, led by the
    /// one it already had then (if any was recorded).
    pub fn status_history(&self, app: &str, since: i64) -> Result<Vec<(i64, Status)>> {
        let conn = self.conn()?;
        let row = |r: &Row| -> rusqlite::Result<(i64, String, String)> { Ok((r.get(0)?, r.get(1)?, r.get(2)?)) };
        let before = conn
            .query_row(
                "SELECT at, message, data FROM events WHERE app = ?1 AND kind = 'app.status' AND at < ?2
                 ORDER BY id DESC LIMIT 1",
                params![app, since],
                row,
            )
            .optional()?;
        let mut stmt = conn.prepare(
            "SELECT at, message, data FROM events WHERE app = ?1 AND kind = 'app.status' AND at >= ?2 ORDER BY id",
        )?;
        let after = stmt.query_map(params![app, since], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(before
            .into_iter()
            .chain(after)
            .filter_map(|(at, message, data)| {
                // Older events carry the status only in their message.
                let data = json(data);
                let text = data["status"].as_str().map(str::to_owned);
                let text = text.unwrap_or_else(|| message.split(':').next().unwrap_or_default().to_owned());
                Status::parse(&text).map(|s| (at, s))
            })
            .collect())
    }

    /// Forget an app, its routes, releases and secrets. Jobs and events stay
    /// as history.
    pub fn remove_app(&self, name: &str) -> Result<()> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM secrets WHERE scope = ?1", [name])?;
        tx.execute("DELETE FROM drafts WHERE app = ?1", [name])?;
        tx.execute("DELETE FROM proposals WHERE app = ?1", [name])?;
        if tx.execute("DELETE FROM apps WHERE name = ?1", [name])? == 0 {
            return Err(Error::NoSuchApp(name.to_owned()));
        }
        record(&tx, Some(name), "app.removed", "removed", &serde_json::Value::Null)?;
        tx.commit()?;
        Ok(())
    }

    pub fn routes(&self) -> Result<Vec<Route>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare("SELECT hostname, path, app, service FROM routes ORDER BY hostname, path")?;
        let rows = stmt.query_map([], |r| {
            Ok(Route { hostname: r.get(0)?, path: r.get(1)?, app: r.get(2)?, service: r.get(3)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // --- releases --------------------------------------------------------

    pub fn create_release(&self, app: &str, commit_sha: &str, lease: &str) -> Result<Release> {
        let conn = self.conn()?;
        get_app(&conn, app)?;
        conn.execute(
            "INSERT INTO releases (app, commit_sha, lease, state, created_at) VALUES (?1, ?2, ?3, 'pending', ?4)",
            params![app, commit_sha, lease, now_ms()],
        )?;
        let id = conn.last_insert_rowid();
        record(
            &conn,
            Some(app),
            "release.created",
            &format!("release {id} for {}", short(commit_sha)),
            &serde_json::json!({ "release": id }),
        )?;
        get_release(&conn, id)
    }

    /// Move a release forward. Reaching `live` makes it the app's live
    /// release and supersedes the one before it, in one transaction.
    pub fn set_release_state(&self, id: i64, state: ReleaseState, reason: Option<&str>) -> Result<Release> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let release = get_release(&tx, id)?;
        let finished =
            matches!(state, ReleaseState::Live | ReleaseState::Failed | ReleaseState::Superseded).then(now_ms);
        tx.execute(
            "UPDATE releases SET state = ?2, reason = COALESCE(?3, reason), finished_at = COALESCE(?4, finished_at) WHERE id = ?1",
            params![id, state.as_str(), reason, finished],
        )?;
        if state == ReleaseState::Live {
            tx.execute(
                "UPDATE releases SET state = 'superseded'
                 WHERE id = (SELECT live_release FROM apps WHERE name = ?1) AND id != ?2",
                params![release.app, id],
            )?;
            tx.execute(
                "UPDATE apps SET live_release = ?2, updated_at = ?3 WHERE name = ?1",
                params![release.app, id, now_ms()],
            )?;
        }
        let message = match reason {
            Some(r) => format!("release {id} {state}: {r}"),
            None => format!("release {id} {state}"),
        };
        record(&tx, Some(&release.app), "release.state", &message, &serde_json::json!({ "release": id }))?;
        let out = get_release(&tx, id)?;
        tx.commit()?;
        Ok(out)
    }

    pub fn set_release_images(&self, id: i64, images: &serde_json::Value) -> Result<()> {
        let conn = self.conn()?;
        if conn.execute("UPDATE releases SET images = ?2 WHERE id = ?1", params![id, images.to_string()])? == 0 {
            return Err(Error::NotFound { kind: "release", id });
        }
        Ok(())
    }

    pub fn release(&self, id: i64) -> Result<Release> {
        get_release(&*self.conn()?, id)
    }

    /// Newest first.
    pub fn releases(&self, app: &str, limit: usize) -> Result<Vec<Release>> {
        let conn = self.conn()?;
        let mut stmt =
            conn.prepare(&format!("SELECT {RELEASE_COLS} FROM releases WHERE app = ?1 ORDER BY id DESC LIMIT ?2"))?;
        let rows = stmt.query_map(params![app, limit as i64], release_row)?;
        rows.map(|r| r?).collect()
    }

    /// The newest release of `commit` for an app, whatever its state.
    pub fn release_for_commit(&self, app: &str, commit: &str) -> Result<Option<Release>> {
        let conn = self.conn()?;
        conn.query_row(
            &format!("SELECT {RELEASE_COLS} FROM releases WHERE app = ?1 AND commit_sha = ?2 ORDER BY id DESC LIMIT 1"),
            params![app, commit],
            release_row,
        )
        .optional()?
        .transpose()
    }

    /// Retire every release of `app` still on trial or in proof, other than
    /// `keep`: a newer push replaces them.
    pub fn supersede_trials(&self, app: &str, keep: i64) -> Result<usize> {
        let conn = self.conn()?;
        let n = conn.execute(
            "UPDATE releases SET state = 'superseded', finished_at = ?3
             WHERE app = ?1 AND id != ?2 AND state IN ('pending', 'building', 'trial', 'proof')",
            params![app, keep, now_ms()],
        )?;
        if n > 0 {
            record(
                &conn,
                Some(app),
                "release.superseded",
                &format!("{n} older release(s) superseded"),
                &serde_json::Value::Null,
            )?;
        }
        Ok(n)
    }

    /// The releases Plumb should be running for an app: the live one, and the
    /// newest release still being tried that is newer than it.
    pub fn wanted_releases(&self, app: &str) -> Result<Wanted> {
        let conn = self.conn()?;
        let a = get_app(&conn, app)?;
        let live = a.live_release.map(|id| get_release(&conn, id)).transpose()?;
        let candidate = conn
            .query_row(
                &format!(
                    "SELECT {RELEASE_COLS} FROM releases
                     WHERE app = ?1 AND state IN ('trial', 'proof') AND id > ?2
                     ORDER BY id DESC LIMIT 1"
                ),
                params![app, a.live_release.unwrap_or(0)],
                release_row,
            )
            .optional()?
            .transpose()?;
        Ok(Wanted { live, candidate })
    }

    // --- proofs ----------------------------------------------------------

    /// Proof's saved progress on a release, if it has started.
    pub fn proof(&self, release: i64) -> Result<Option<serde_json::Value>> {
        let conn = self.conn()?;
        let text: Option<String> =
            conn.query_row("SELECT state FROM proofs WHERE release = ?1", [release], |r| r.get(0)).optional()?;
        Ok(text.map(json))
    }

    pub fn put_proof(&self, release: i64, app: &str, state: &serde_json::Value) -> Result<()> {
        self.conn()?.execute(
            "INSERT INTO proofs (release, app, state, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (release) DO UPDATE SET state = excluded.state, updated_at = excluded.updated_at",
            params![release, app, state.to_string(), now_ms()],
        )?;
        Ok(())
    }

    // --- host ports ------------------------------------------------------

    /// The host port for one service slot, allocating the lowest free port in
    /// `range` the first time. The same slot always gets the same port.
    pub fn port_for(&self, app: &str, service: &str, slot: i64, range: std::ops::Range<u16>) -> Result<u16> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let existing: Option<i64> = tx
            .query_row(
                "SELECT host_port FROM ports WHERE app = ?1 AND service = ?2 AND slot = ?3",
                params![app, service, slot],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(p) = existing {
            return Ok(p as u16);
        }
        let used: std::collections::BTreeSet<i64> = {
            let mut stmt = tx.prepare("SELECT host_port FROM ports")?;
            stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        let port = range.clone().find(|p| !used.contains(&i64::from(*p))).ok_or(Error::PortsExhausted)?;
        tx.execute(
            "INSERT INTO ports (app, service, slot, host_port) VALUES (?1, ?2, ?3, ?4)",
            params![app, service, slot, port],
        )?;
        tx.commit()?;
        Ok(port)
    }

    /// The host port already given to a slot, without allocating one.
    pub fn existing_port(&self, app: &str, service: &str, slot: i64) -> Result<Option<u16>> {
        let conn = self.conn()?;
        let port: Option<i64> = conn
            .query_row(
                "SELECT host_port FROM ports WHERE app = ?1 AND service = ?2 AND slot = ?3",
                params![app, service, slot],
                |r| r.get(0),
            )
            .optional()?;
        Ok(port.map(|p| p as u16))
    }

    /// Release every port of `app` whose slot is not in `keep`.
    pub fn free_ports(&self, app: &str, keep: &[(String, i64)]) -> Result<usize> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare("SELECT service, slot FROM ports WHERE app = ?1")?;
        let held: Vec<(String, i64)> =
            stmt.query_map([app], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut freed = 0;
        for (service, slot) in held {
            if !keep.iter().any(|(s, k)| *s == service && *k == slot) {
                freed += conn.execute(
                    "DELETE FROM ports WHERE app = ?1 AND service = ?2 AND slot = ?3",
                    params![app, service, slot],
                )?;
            }
        }
        Ok(freed)
    }

    // --- jobs ------------------------------------------------------------

    /// Queue a job unless one of the same kind for the same app is already
    /// queued or running; then return that one. A webhook and a poll that
    /// notice the same push therefore start one deploy, not two.
    pub fn enqueue(&self, kind: &str, app: Option<&str>, payload: &serde_json::Value) -> Result<Job> {
        let conn = self.conn()?;
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM jobs WHERE kind = ?1 AND app IS ?2 AND state IN ('queued', 'running') ORDER BY id LIMIT 1",
                params![kind, app],
                |r| r.get(0),
            )
            .optional()?;
        let id = match existing {
            Some(id) => id,
            None => {
                let now = now_ms();
                conn.execute(
                    "INSERT INTO jobs (kind, app, state, payload, not_before, created_at, updated_at)
                     VALUES (?1, ?2, 'queued', ?3, ?4, ?4, ?4)",
                    params![kind, app, payload.to_string(), now],
                )?;
                conn.last_insert_rowid()
            }
        };
        get_job(&conn, id)
    }

    /// Take the oldest job that is due, marking it running.
    pub fn claim(&self) -> Result<Option<Job>> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let now = now_ms();
        let id: Option<i64> = tx
            .query_row(
                "SELECT id FROM jobs WHERE state = 'queued' AND not_before <= ?1 ORDER BY not_before, id LIMIT 1",
                [now],
                |r| r.get(0),
            )
            .optional()?;
        let Some(id) = id else { return Ok(None) };
        tx.execute(
            "UPDATE jobs SET state = 'running', attempts = attempts + 1, updated_at = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        let job = get_job(&tx, id)?;
        tx.commit()?;
        Ok(Some(job))
    }

    pub fn finish(&self, id: i64) -> Result<()> {
        self.settle(id, JobState::Done, None, None)
    }

    /// Record a failure. With `retry_at`, the job goes back on the queue for
    /// then; without, it stays failed.
    pub fn fail(&self, id: i64, error: &str, retry_at: Option<i64>) -> Result<()> {
        let state = if retry_at.is_some() { JobState::Queued } else { JobState::Failed };
        self.settle(id, state, Some(error), retry_at)
    }

    fn settle(&self, id: i64, state: JobState, error: Option<&str>, not_before: Option<i64>) -> Result<()> {
        let conn = self.conn()?;
        let changed = conn.execute(
            "UPDATE jobs SET state = ?2, error = ?3, not_before = COALESCE(?4, not_before), updated_at = ?5
             WHERE id = ?1 AND state = 'running'",
            params![id, state.as_str(), error, not_before, now_ms()],
        )?;
        if changed == 0 {
            return Err(Error::NotFound { kind: "running job", id });
        }
        Ok(())
    }

    /// After a crash or reboot: every job that was running is queued again.
    /// Jobs are written to be safe to run twice.
    pub fn requeue_interrupted(&self) -> Result<usize> {
        let conn = self.conn()?;
        let n =
            conn.execute("UPDATE jobs SET state = 'queued', updated_at = ?1 WHERE state = 'running'", [now_ms()])?;
        if n > 0 {
            record(
                &conn,
                None,
                "jobs.requeued",
                &format!("{n} interrupted job(s) queued again"),
                &serde_json::Value::Null,
            )?;
        }
        Ok(n)
    }

    pub fn job(&self, id: i64) -> Result<Job> {
        get_job(&*self.conn()?, id)
    }

    // --- events ----------------------------------------------------------

    pub fn record(&self, app: Option<&str>, kind: &str, message: &str, data: &serde_json::Value) -> Result<()> {
        record(&*self.conn()?, app, kind, message, data)
    }

    /// Newest first, for one app or for everything.
    pub fn events(&self, app: Option<&str>, limit: usize) -> Result<Vec<Event>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, at, app, kind, message, data FROM events
             WHERE ?1 IS NULL OR app = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![app, limit as i64], event_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Events newer than `after`, oldest first: what a watcher has not seen.
    pub fn events_after(&self, after: i64, app: Option<&str>, limit: usize) -> Result<Vec<Event>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, at, app, kind, message, data FROM events
             WHERE id > ?1 AND (?2 IS NULL OR app = ?2) ORDER BY id LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![after, app, limit as i64], event_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // --- sealed secrets --------------------------------------------------
    //
    // Canon stores only ciphertext. Sealing and opening is the
    // Hush's job; `scope` is an app name, or "" for Henry's own keys.

    pub fn put_sealed(&self, scope: &str, name: &str, sealed: &[u8]) -> Result<()> {
        self.conn()?.execute(
            "INSERT INTO secrets (scope, name, sealed, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (scope, name) DO UPDATE SET sealed = excluded.sealed, updated_at = excluded.updated_at",
            params![scope, name, sealed, now_ms()],
        )?;
        Ok(())
    }

    pub fn get_sealed(&self, scope: &str, name: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .conn()?
            .query_row("SELECT sealed FROM secrets WHERE scope = ?1 AND name = ?2", params![scope, name], |r| r.get(0))
            .optional()?)
    }

    pub fn sealed_names(&self, scope: &str) -> Result<Vec<String>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare("SELECT name FROM secrets WHERE scope = ?1 ORDER BY name")?;
        let rows = stmt.query_map([scope], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// When each secret in `scope` was last set. A change here is what makes
    /// Plumb recreate the containers that use it.
    pub fn sealed_versions(&self, scope: &str) -> Result<std::collections::BTreeMap<String, i64>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare("SELECT name, updated_at FROM secrets WHERE scope = ?1")?;
        let rows = stmt.query_map([scope], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn delete_sealed(&self, scope: &str, name: &str) -> Result<bool> {
        Ok(self.conn()?.execute("DELETE FROM secrets WHERE scope = ?1 AND name = ?2", params![scope, name])? > 0)
    }
}

// --- row helpers ----------------------------------------------------------

const APP_COLS: &str =
    "name, repo, lease, source, desired, status, status_detail, live_release, created_at, updated_at";
const RELEASE_COLS: &str = "id, app, commit_sha, lease, state, images, reason, created_at, finished_at";
const JOB_COLS: &str = "id, kind, app, state, attempts, payload, error, not_before";

fn claim_routes(conn: &Connection, lease: &Lease) -> Result<()> {
    for s in &lease.services {
        let Some(p) = &s.public else { continue };
        let owner: Option<String> = conn
            .query_row("SELECT app FROM routes WHERE hostname = ?1 AND path = ?2", params![p.hostname, p.path], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(app) = owner {
            return Err(Error::RouteTaken { hostname: p.hostname.clone(), path: p.path.clone(), app });
        }
        conn.execute(
            "INSERT INTO routes (hostname, path, app, service) VALUES (?1, ?2, ?3, ?4)",
            params![p.hostname, p.path, lease.name, s.name],
        )?;
    }
    Ok(())
}

fn record(conn: &Connection, app: Option<&str>, kind: &str, message: &str, data: &serde_json::Value) -> Result<()> {
    conn.execute(
        "INSERT INTO events (at, app, kind, message, data) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![now_ms(), app, kind, message, data.to_string()],
    )?;
    Ok(())
}

fn event_row(r: &Row) -> rusqlite::Result<Event> {
    Ok(Event {
        id: r.get(0)?,
        at: r.get(1)?,
        app: r.get(2)?,
        kind: r.get(3)?,
        message: r.get(4)?,
        data: json(r.get::<_, String>(5)?),
    })
}

fn get_app(conn: &Connection, name: &str) -> Result<App> {
    conn.query_row(&format!("SELECT {APP_COLS} FROM apps WHERE name = ?1"), [name], app_row)
        .optional()?
        .ok_or_else(|| Error::NoSuchApp(name.to_owned()))?
}

fn get_release(conn: &Connection, id: i64) -> Result<Release> {
    conn.query_row(&format!("SELECT {RELEASE_COLS} FROM releases WHERE id = ?1"), [id], release_row)
        .optional()?
        .ok_or(Error::NotFound { kind: "release", id })?
}

fn get_job(conn: &Connection, id: i64) -> Result<Job> {
    conn.query_row(&format!("SELECT {JOB_COLS} FROM jobs WHERE id = ?1"), [id], job_row)
        .optional()?
        .ok_or(Error::NotFound { kind: "job", id })?
}

fn app_row(r: &Row<'_>) -> rusqlite::Result<Result<App>> {
    let source: String = r.get(3)?;
    let desired: String = r.get(4)?;
    let status: String = r.get(5)?;
    Ok((|| {
        Ok(App {
            name: r.get(0)?,
            repo: r.get(1)?,
            lease: r.get(2)?,
            source: Source::parse(&source).ok_or(Error::Corrupt("source", source.clone()))?,
            desired: Desired::parse(&desired).ok_or(Error::Corrupt("desired", desired.clone()))?,
            status: Status::parse(&status).ok_or(Error::Corrupt("status", status.clone()))?,
            status_detail: r.get(6)?,
            live_release: r.get(7)?,
            created_at: r.get(8)?,
            updated_at: r.get(9)?,
        })
    })())
}

fn release_row(r: &Row<'_>) -> rusqlite::Result<Result<Release>> {
    let state: String = r.get(4)?;
    Ok((|| {
        Ok(Release {
            id: r.get(0)?,
            app: r.get(1)?,
            commit_sha: r.get(2)?,
            lease: r.get(3)?,
            state: ReleaseState::parse(&state).ok_or(Error::Corrupt("release state", state.clone()))?,
            images: json(r.get::<_, String>(5)?),
            reason: r.get(6)?,
            created_at: r.get(7)?,
            finished_at: r.get(8)?,
        })
    })())
}

fn job_row(r: &Row<'_>) -> rusqlite::Result<Result<Job>> {
    let state: String = r.get(3)?;
    Ok((|| {
        Ok(Job {
            id: r.get(0)?,
            kind: r.get(1)?,
            app: r.get(2)?,
            state: JobState::parse(&state).ok_or(Error::Corrupt("job state", state.clone()))?,
            attempts: r.get(4)?,
            payload: json(r.get::<_, String>(5)?),
            error: r.get(6)?,
            not_before: r.get(7)?,
        })
    })())
}

fn json(text: String) -> serde_json::Value {
    serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text))
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(10)]
}

#[cfg(test)]
mod tests;
