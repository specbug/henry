//! Ark: backups, and the drills that prove them.
//!
//! What survives the flood is what was put in the ark, and a promise of
//! safety is a covenant only if it is kept. Every day Ark takes an
//! engine-aware snapshot of each volume of each live app (SQLite `.backup`,
//! `pg_dumpall`, Redis `BGSAVE`, or plain files with their containers paused)
//! and streams it into a restic repository, normally on R2. Restic encrypts,
//! deduplicates and keeps 7 daily, 4 weekly and 6 monthly snapshots.
//!
//! A backup counts only once it has been restored: a drill restores the
//! newest snapshot into a scratch volume (and, for a database, a scratch
//! server) and checks it, every month. Henry's own Canon is backed up the
//! same way, and mirrors (Google Drive through rclone, say) get a copy.

pub mod engine;
pub mod restic;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use canon::{Canon, Desired, Job, now_ms};
use hush::Secret;
use lease::{Backup, Lease};
use tracing::{info, warn};

use crate::engine::{Engine, pipe};
pub use crate::restic::{Restic, Retention, Snapshot};

/// The Canon job kind Ark runs. Its app is the app to back up, or
/// [`HENRY`] for Henry's own state.
pub const JOB: &str = "ark";
/// The name Henry's own backups go under; no app may take it.
pub const HENRY: &str = "henry";
const CANON: &str = "canon";
const CONFIG: &str = "config";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Worth trying again later: the network, the repository unreachable.
    #[error("{0}")]
    Transient(String),
    #[error("{0}")]
    Restic(String),
    #[error("{0}")]
    Engine(String),
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Io(String),
    #[error(transparent)]
    Canon(#[from] canon::Error),
}

impl Error {
    pub fn is_transient(&self) -> bool {
        matches!(self, Error::Transient(_))
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone)]
pub struct Settings {
    /// How often each target is backed up.
    pub every: Duration,
    /// How soon a failed backup or drill is tried again.
    pub retry: Duration,
    /// How often each target's newest snapshot is drilled.
    pub drill_every: Duration,
    pub keep: Retention,
    /// Where Canon's snapshot is written before it is backed up.
    pub scratch: PathBuf,
    /// `henry.toml`, backed up with Canon.
    pub config: Option<PathBuf>,
    /// How long a scratch server may take to come up.
    pub patience: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            every: Duration::from_secs(24 * 3600),
            retry: Duration::from_secs(3600),
            drill_every: Duration::from_secs(30 * 24 * 3600),
            keep: Retention::default(),
            scratch: std::env::temp_dir(),
            config: None,
            patience: Duration::from_secs(120),
        }
    }
}

/// One volume to back up.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub app: String,
    pub service: String,
    pub volume: String,
    pub kind: Backup,
    /// The engine's name for the volume, `henry-<app>-<volume>`.
    pub engine_volume: String,
    /// Where the service mounts it.
    pub mount: String,
    /// The service's one container, `<app>-<service>`.
    pub container: String,
    /// The image the live release runs, for scratch servers in drills.
    pub image: String,
    pub env: BTreeMap<String, String>,
}

impl Target {
    /// `service/volume`, as Canon records it.
    pub fn name(&self) -> String {
        format!("{}/{}", self.service, self.volume)
    }

    /// The file the snapshot holds.
    pub fn filename(&self) -> String {
        let ext = match self.kind {
            Backup::Files | Backup::Sqlite { .. } => "tar",
            Backup::Postgres => "sql",
            Backup::Redis => "rdb",
        };
        format!("{}/{}/{}.{ext}", self.app, self.service, self.volume)
    }

    pub fn tags(&self) -> Vec<String> {
        tags(&self.app, &self.name())
    }

    fn pg_user(&self) -> &str {
        self.env.get("POSTGRES_USER").map_or("postgres", String::as_str)
    }

    fn scratch(&self) -> String {
        format!("henry-drill-{}-{}", self.app, self.service)
    }
}

fn tags(app: &str, target: &str) -> Vec<String> {
    vec![format!("app={app}"), format!("target={target}")]
}

/// Every volume of an app's live release. An app with nothing live has
/// nothing worth keeping yet.
pub fn targets(canon: &Canon, app: &str) -> Result<Vec<Target>> {
    let a = canon.app(app)?;
    let Some(live) = a.live_release else { return Ok(vec![]) };
    let release = canon.release(live)?;
    let lease = Lease::parse(&release.lease).map_err(|e| Error::Refused(format!("the live lease: {e}")))?;
    let mut out = Vec::new();
    for s in &lease.services {
        let image = release.images.get(&s.name).and_then(|i| i.as_str()).unwrap_or_default().to_owned();
        for v in &s.volumes {
            out.push(Target {
                app: app.to_owned(),
                service: s.name.clone(),
                volume: v.name.clone(),
                kind: v.backup.clone().unwrap_or(Backup::Files),
                engine_volume: plumb::plan::volume_name(app, &v.name),
                mount: v.mount.clone(),
                container: plumb::plan::container_name(app, &s.name, None),
                image: image.clone(),
                env: s.env.clone(),
            });
        }
    }
    Ok(out)
}

/// Whether a target is due: never backed up, last good one too old, or a
/// failure old enough to try again.
fn due(canon: &Canon, app: &str, target: &str, settings: &Settings, now: i64) -> Result<bool> {
    Ok(match canon.last_backup(app, target)? {
        None => true,
        Some(b) if b.ok => now - b.at >= settings.every.as_millis() as i64,
        Some(b) => now - b.at >= settings.retry.as_millis() as i64,
    })
}

/// Queue a backup for every running app with a target due, and for Henry
/// itself. Returns what was queued.
pub fn schedule(canon: &Canon, settings: &Settings) -> Result<Vec<String>> {
    let now = now_ms();
    let mut queued = Vec::new();
    for app in canon.apps()? {
        if app.desired == Desired::Stopped || app.live_release.is_none() {
            continue;
        }
        let mut any = false;
        for t in targets(canon, &app.name)? {
            any |= due(canon, &app.name, &t.name(), settings, now)?;
        }
        if any {
            canon.enqueue(JOB, Some(&app.name), &serde_json::json!({}))?;
            queued.push(app.name);
        }
    }
    if due(canon, HENRY, CANON, settings, now)? {
        canon.enqueue(JOB, Some(HENRY), &serde_json::json!({}))?;
        queued.push(HENRY.to_owned());
    }
    Ok(queued)
}

pub struct Ark {
    canon: Arc<Canon>,
    restic: Restic,
    mirrors: Vec<Restic>,
    engine: Engine,
    settings: Settings,
}

impl Ark {
    pub fn new(canon: Arc<Canon>, restic: Restic, mirrors: Vec<Restic>, engine: Engine, settings: Settings) -> Self {
        Ark { canon, restic, mirrors, engine, settings }
    }

    /// Back up an app's due targets (every target, with `force` in the
    /// payload), then drill those whose drill is due.
    pub async fn run(&self, job: &Job) -> Result<String> {
        let app = job.app.as_deref().unwrap_or(HENRY);
        let force = job.payload.get("force").and_then(|f| f.as_bool()).unwrap_or(false);
        let now = now_ms();
        let names: Vec<String> = if app == HENRY {
            vec![CANON.to_owned()]
        } else {
            targets(&self.canon, app)?.iter().map(Target::name).collect()
        };
        // A repository or helper that is not there yet fails every due
        // target, so the schedule waits its retry interval rather than
        // queueing the same failure every tick.
        let ready = async {
            self.restic.ensure().await?;
            if app != HENRY {
                unpause_all(&self.engine).await;
                self.engine.ensure_helper().await?;
            }
            Ok::<_, Error>(())
        }
        .await;
        if let Err(e) = ready {
            for name in &names {
                self.canon.record_backup(app, name, None, None, Some(&e.to_string()))?;
            }
            return Err(e);
        }
        if app == HENRY {
            return self.henry().await;
        }
        let mut report = Vec::new();
        for t in targets(&self.canon, app)? {
            let name = t.name();
            if !force && !due(&self.canon, app, &name, &self.settings, now)? {
                continue;
            }
            match self.back_up(&t).await {
                Ok(s) => {
                    self.canon.record_backup(app, &name, Some(&s.id), Some(s.bytes as i64), None)?;
                    if let Err(e) = self.restic.forget(&t.tags(), &self.settings.keep).await {
                        warn!(app, target = %name, "retention not applied: {e}");
                    }
                    info!(app, target = %name, snapshot = %short(&s.id), bytes = s.bytes, "backed up");
                    report.push(format!("{name}: {}", short(&s.id)));
                }
                Err(e) => {
                    self.canon.record_backup(app, &name, None, None, Some(&e.to_string()))?;
                    warn!(app, target = %name, "backup failed: {e}");
                    report.push(format!("{name}: failed"));
                }
            }
            if self.drill_due(app, &name, now)?
                && let Some(b) = self.canon.last_good_backup(app, &name)?
                && let Some(snapshot) = b.snapshot
            {
                let (ok, detail) = match self.drill(&t, &snapshot).await {
                    Ok(d) => (true, d),
                    Err(e) => (false, e.to_string()),
                };
                self.canon.record_drill(app, &name, &snapshot, ok, &detail)?;
                report.push(format!("{name}: drill {}", if ok { "passed" } else { "FAILED" }));
            }
        }
        Ok(if report.is_empty() { format!("{app}: nothing due") } else { format!("{app}: {}", report.join("; ")) })
    }

    /// A drill is due when no drill ever passed, or the last pass is old; a
    /// failed drill waits for the retry interval.
    fn drill_due(&self, app: &str, target: &str, now: i64) -> Result<bool> {
        let every = self.settings.drill_every.as_millis() as i64;
        Ok(match self.canon.last_drill(app, target)? {
            None => true,
            Some(d) if d.ok => now - d.at >= every,
            Some(d) => now - d.at >= self.settings.retry.as_millis() as i64,
        })
    }

    /// Take one target's snapshot.
    pub async fn back_up(&self, t: &Target) -> Result<Snapshot> {
        let command = match &t.kind {
            Backup::Files => {
                // Containers writing to the volume are frozen for the copy.
                let users = self.engine.users(&t.engine_volume).await?;
                for id in &users {
                    self.engine.run(&["pause", id]).await?;
                }
                let args = self.engine.helper_args(&t.engine_volume, true, &[], "tar -C /v -cf - .");
                let result = self.restic.backup_from(&args, &t.filename(), &t.tags()).await;
                for id in &users {
                    if let Err(e) = self.engine.run(&["unpause", id]).await {
                        warn!(container = %id, "could not unpause after the backup: {e}");
                    }
                }
                return result;
            }
            Backup::Sqlite { path } => {
                let path = safe_path(path)?;
                self.engine.helper_args(&t.engine_volume, false, &[("P", path)], engine::SQLITE_BACKUP)
            }
            Backup::Postgres => {
                let program = self.engine.program.to_string_lossy().into_owned();
                [program.as_str(), "exec", &t.container, "pg_dumpall", "-U", t.pg_user(), "--clean", "--if-exists"]
                    .map(String::from)
                    .to_vec()
            }
            Backup::Redis => {
                let program = self.engine.program.to_string_lossy().into_owned();
                [program.as_str(), "exec", &t.container, "sh", "-c", engine::REDIS_BACKUP].map(String::from).to_vec()
            }
        };
        self.restic.backup_from(&command, &t.filename(), &t.tags()).await
    }

    /// Restore a snapshot into a scratch copy and check it. Returns what was
    /// found, e.g. `restored 42 files` or `3 databases`.
    pub async fn drill(&self, t: &Target, snapshot: &str) -> Result<String> {
        let scratch = t.scratch();
        self.engine.remove_container(&scratch).await;
        self.engine.remove_volume(&scratch).await;
        let result = self.drill_into(t, snapshot, &scratch).await;
        self.engine.remove_container(&scratch).await;
        self.engine.remove_volume(&scratch).await;
        result
    }

    async fn drill_into(&self, t: &Target, snapshot: &str, scratch: &str) -> Result<String> {
        self.engine.run(&["volume", "create", scratch]).await?;
        let password = Secret::new(random_hex()?);
        let server = self.load(t, snapshot, scratch, &password).await?;
        match &t.kind {
            Backup::Files => {
                let count = self.engine.run(&helper_cmd(&self.engine, scratch, &[], engine::COUNT)).await?;
                Ok(format!("restored {count} files"))
            }
            Backup::Sqlite { path } => {
                let env = [("P", safe_path(path)?)];
                match self.engine.run(&helper_cmd(&self.engine, scratch, &env, engine::SQLITE_CHECK)).await?.as_str() {
                    "ok" => Ok(format!("{path} passes its integrity check")),
                    "absent" => Ok(format!("no {path} yet; the volume's other files restored")),
                    other => Err(Error::Refused(format!("{path} fails its integrity check: {other}"))),
                }
            }
            Backup::Postgres => {
                let name = server.expect("a Postgres load starts a server");
                let sql = "select count(*) from pg_database where not datistemplate";
                let n =
                    self.engine.run(&["exec", &name, "psql", "-U", t.pg_user(), "-d", "postgres", "-tAc", sql]).await?;
                Ok(format!("{n} database(s) restored and answering"))
            }
            Backup::Redis => {
                let name = server.expect("a Redis load starts a server");
                let n = self.engine.run(&["exec", &name, "redis-cli", "DBSIZE"]).await?;
                Ok(format!("{n} key(s) restored and answering"))
            }
        }
    }

    /// Put a snapshot into `volume`. A database comes back through a server
    /// running on that volume, which is left running (and named) for the
    /// caller to check and remove.
    async fn load(&self, t: &Target, snapshot: &str, volume: &str, password: &Secret) -> Result<Option<String>> {
        let dump = self.restic.dump(snapshot, &t.filename());
        let timeout = self.restic.timeout;
        match &t.kind {
            Backup::Files | Backup::Sqlite { .. } => {
                pipe(dump, self.engine.helper(volume, false, &[], engine::EXTRACT), timeout).await?;
                Ok(None)
            }
            Backup::Redis => {
                pipe(dump, self.engine.helper(volume, false, &[("F", "dump.rdb")], engine::WRITE_FILE), timeout)
                    .await?;
                let name = t.scratch();
                let mount = format!("{volume}:{}", t.mount);
                self.engine.run(&["run", "-d", "--name", &name, "--network", "none", "-v", &mount, &t.image]).await?;
                self.engine
                    .wait_for(&["exec", &name, "redis-cli", "PING"], Some("PONG"), self.settings.patience)
                    .await?;
                Ok(Some(name))
            }
            Backup::Postgres => {
                self.engine.run(&helper_cmd(&self.engine, volume, &[], "find /v -mindepth 1 -delete")).await?;
                let name = t.scratch();
                let mount = format!("{volume}:{}", t.mount);
                let mut run = self.engine.command();
                run.args(["run", "-d", "--name", &name, "--network", "none", "-v", &mount])
                    .args(["-e", "POSTGRES_PASSWORD", "-e", &format!("POSTGRES_USER={}", t.pg_user())])
                    .arg(&t.image)
                    // The password reaches the container through the CLI's
                    // environment, never its argv.
                    .env("POSTGRES_PASSWORD", password.expose())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::piped());
                let out = run.output().await.map_err(|e| Error::Engine(e.to_string()))?;
                if !out.status.success() {
                    return Err(Error::Engine(format!(
                        "starting a scratch Postgres: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    )));
                }
                // The image's first-boot server listens on no TCP port; this
                // waits for the real one.
                let ready = ["exec", &name, "pg_isready", "-h", "127.0.0.1", "-U", t.pg_user()];
                self.engine.wait_for(&ready, None, self.settings.patience).await?;
                let mut psql = self.engine.command();
                psql.args(["exec", "-i", &name, "psql", "-U", t.pg_user(), "-d", "postgres", "-q", "-f", "-"]);
                pipe(dump, psql, timeout).await?;
                Ok(Some(name))
            }
        }
    }

    /// Restore a snapshot over the live volume. The app must be stopped:
    /// nothing may be using the volume. A database is loaded by a temporary
    /// server with the app's own password, which is then removed.
    pub async fn restore(&self, t: &Target, snapshot: &str, password: Option<&Secret>) -> Result<String> {
        let users = self.engine.users(&t.engine_volume).await?;
        if !users.is_empty() {
            return Err(Error::Refused(format!("{} is in use; `henry stop {}` first", t.engine_volume, t.app)));
        }
        self.engine.ensure_helper().await?;
        let generated;
        let password = match password {
            Some(p) => p,
            None => {
                generated = Secret::new(random_hex()?);
                &generated
            }
        };
        let server = self.load(t, snapshot, &t.engine_volume, password).await;
        if let Ok(Some(name)) = &server {
            let _ = self.engine.run(&["stop", name]).await;
            self.engine.remove_container(name).await;
        }
        server?;
        Ok(format!("restored {} from {} into {}", t.name(), short(snapshot), t.engine_volume))
    }

    /// Henry's own state: Canon and henry.toml, then the daily repository
    /// upkeep (prune, mirrors) and Canon's drill.
    async fn henry(&self) -> Result<String> {
        let now = now_ms();
        let mut report = Vec::new();
        let snap = self.settings.scratch.join(format!("canon-{now}.db"));
        let result = async {
            self.canon.snapshot_to(&snap)?;
            let cat = vec!["cat".to_owned(), snap.to_string_lossy().into_owned()];
            self.restic.backup_from(&cat, "henry/canon.db", &tags(HENRY, CANON)).await
        }
        .await;
        let _ = std::fs::remove_file(&snap);
        match result {
            Ok(s) => {
                self.canon.record_backup(HENRY, CANON, Some(&s.id), Some(s.bytes as i64), None)?;
                report.push(format!("canon: {}", short(&s.id)));
            }
            Err(e) => {
                self.canon.record_backup(HENRY, CANON, None, None, Some(&e.to_string()))?;
                report.push("canon: failed".into());
            }
        }
        if let Some(config) = self.settings.config.as_ref().filter(|c| c.exists()) {
            let cat = vec!["cat".to_owned(), config.to_string_lossy().into_owned()];
            match self.restic.backup_from(&cat, "henry/henry.toml", &tags(HENRY, CONFIG)).await {
                Ok(s) => drop(self.canon.record_backup(HENRY, CONFIG, Some(&s.id), Some(s.bytes as i64), None)?),
                Err(e) => drop(self.canon.record_backup(HENRY, CONFIG, None, None, Some(&e.to_string()))?),
            }
        }
        for target in [CANON, CONFIG] {
            if let Err(e) = self.restic.forget(&tags(HENRY, target), &self.settings.keep).await {
                warn!(target, "retention not applied: {e}");
            }
        }
        match self.restic.prune().await {
            Ok(()) => report.push("pruned".into()),
            Err(e) => warn!("prune failed: {e}"),
        }
        for m in &self.mirrors {
            match m.mirror_from(&self.restic).await {
                Ok(()) => report.push(format!("copied to {}", m.repository)),
                Err(e) => {
                    warn!(mirror = %m.repository, "mirror copy failed: {e}");
                    self.canon.record(
                        Some(HENRY),
                        "ark.mirror_failed",
                        &format!("copy to {} failed: {e}", m.repository),
                        &serde_json::Value::Null,
                    )?;
                    report.push(format!("{}: failed", m.repository));
                }
            }
        }
        if self.drill_due(HENRY, CANON, now)?
            && let Some(snapshot) = self.canon.last_good_backup(HENRY, CANON)?.and_then(|b| b.snapshot)
        {
            let (ok, detail) = match self.drill_canon(&snapshot).await {
                Ok(d) => (true, d),
                Err(e) => (false, e.to_string()),
            };
            self.canon.record_drill(HENRY, CANON, &snapshot, ok, &detail)?;
            report.push(format!("canon drill {}", if ok { "passed" } else { "FAILED" }));
        }
        Ok(format!("henry: {}", report.join("; ")))
    }

    /// Restore Canon's snapshot to a scratch file and open it.
    async fn drill_canon(&self, snapshot: &str) -> Result<String> {
        let path = self.settings.scratch.join(format!("canon-drill-{}.db", now_ms()));
        let result = async {
            self.restore_file(snapshot, "henry/canon.db", &path).await?;
            let restored = Canon::open(&path)?;
            let apps = restored.apps()?.len();
            Ok(format!("Canon opens at schema {} with {apps} app(s)", restored.schema_version()?))
        }
        .await;
        let _ = std::fs::remove_file(&path);
        result
    }

    /// Write one file of a snapshot to `dest`, e.g. Canon when rebuilding.
    pub async fn restore_file(&self, snapshot: &str, filename: &str, dest: &std::path::Path) -> Result<()> {
        let out = self
            .restic
            .dump(snapshot, filename)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output();
        let out = tokio::time::timeout(self.restic.timeout, out)
            .await
            .map_err(|_| Error::Transient("restic dump timed out".into()))?
            .map_err(|e| Error::Restic(e.to_string()))?;
        if !out.status.success() {
            return Err(Error::Restic(format!("restic dump: {}", String::from_utf8_lossy(&out.stderr).trim())));
        }
        std::fs::write(dest, &out.stdout).map_err(|e| Error::Io(format!("{}: {e}", dest.display())))
    }

    /// The newest snapshot of a target in the repository.
    pub async fn latest(&self, app: &str, target: &str) -> Result<Option<Snapshot>> {
        self.restic.latest(&tags(app, target)).await
    }
}

/// Copy everything in volume `from` into `to`, for an app moving onto Henry
/// from elsewhere (hostd). `to` is created if missing, labelled as `app`'s,
/// and emptied first, so running this again gives the same result. Anything
/// running on `from` is paused for the copy, so it is consistent, and always
/// unpaused after. Refuses while anything runs on `to`. Returns how many
/// files `to` then holds.
pub async fn seed(engine: &Engine, from: &str, to: &str, app: &str) -> Result<u64> {
    if engine.run(&["volume", "inspect", from]).await.is_err() {
        return Err(Error::Refused(format!("there is no volume `{from}` to copy from")));
    }
    if !engine.users(to).await?.is_empty() {
        return Err(Error::Refused(format!("`{to}` is in use; stop what runs on it first")));
    }
    if engine.run(&["volume", "inspect", to]).await.is_err() {
        let app_label = format!("dev.henry.app={app}");
        engine.run(&["volume", "create", "--label", "dev.henry.managed=1", "--label", &app_label, to]).await?;
    }
    engine.ensure_helper().await?;
    let users = engine.users(from).await?;
    for id in &users {
        engine.run(&["pause", id]).await?;
    }
    let (from_mount, to_mount) = (format!("{from}:/from:ro"), format!("{to}:/v"));
    let copied = engine
        .run(&[
            "run",
            "--rm",
            "--network",
            "none",
            "-v",
            &from_mount,
            "-v",
            &to_mount,
            &engine.helper,
            "sh",
            "-c",
            "set -eu\nfind /v -mindepth 1 -delete\ntar -C /from -cf - . | tar -C /v -xf -\nfind /v -type f | wc -l",
        ])
        .await;
    for id in &users {
        if let Err(e) = engine.run(&["unpause", id]).await {
            warn!(container = %id, "could not unpause after copying its volume: {e}");
        }
    }
    let out = copied?;
    out.trim().parse().map_err(|_| Error::Engine(format!("the copy reported {out:?}")))
}

/// Unpause every Henry container left paused, e.g. by a crash mid-backup.
pub async fn unpause_all(engine: &Engine) {
    let Ok(out) = engine.run(&["ps", "-q", "--filter", "status=paused", "--filter", "label=dev.henry.managed=1"]).await
    else {
        return;
    };
    for id in out.lines().filter(|l| !l.is_empty()) {
        match engine.run(&["unpause", id]).await {
            Ok(_) => warn!(container = id, "unpaused a container a backup left paused"),
            Err(e) => warn!(container = id, "could not unpause: {e}"),
        }
    }
}

/// A helper run, as arguments for [`Engine::run`] (which adds the program).
fn helper_cmd(engine: &Engine, volume: &str, env: &[(&str, &str)], script: &str) -> Vec<String> {
    let mut args = engine.helper_args(volume, false, env, script);
    args.remove(0);
    args
}

/// A SQLite path for the helper's script: relative, no `..`, plain characters.
fn safe_path(path: &str) -> Result<&str> {
    let plain = path.chars().all(|c| c.is_ascii_alphanumeric() || "._-/".contains(c));
    if plain && !path.starts_with('/') && !path.split('/').any(|p| p == ".." || p.is_empty()) {
        Ok(path)
    } else {
        Err(Error::Refused(format!("`{path}` is not a path Ark will put in a script")))
    }
}

fn random_hex() -> Result<String> {
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).map_err(|e| Error::Io(format!("no randomness: {e}")))?;
    Ok(hex::encode(bytes))
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

#[cfg(test)]
mod tests;
