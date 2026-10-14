//! Cloudflare Workers, deployed from Canon. A Worker follows its app's
//! release on trial, else the live one: the code under test runs with the
//! Worker it was built with, and a failed trial puts the live one back.
//!
//! Henry deploys a Worker as `henry-<app>-<name>` on its account's workers.dev
//! subdomain, with its secrets from Hush. Its R2 bindings are Henry's own
//! bucket, scoped (`scope.js`) to `apps/<app>/<worker>/<binding>/`, so its code
//! sees only its own corner. Henry removes only scripts Canon says it made.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use canon::{Canon, WorkerBundle, WorkerDeploy, serde_json};
use hush::{Hush, Secret};
use lease::Lease;
use tracing::{info, warn};

use crate::Result;

/// The module Henry puts in front of a Worker that uses R2.
pub const ENTRY: &str = "henry-entry.js";

/// Where a Worker's binding keeps its objects in Henry's bucket.
pub fn prefix(app: &str, worker: &str, binding: &str) -> String {
    format!("apps/{app}/{worker}/{binding}/")
}

/// The entry module that scopes each R2 binding to its prefix, then hands
/// every request to the Worker's own `main`.
pub fn entry(main: &str, prefixes: &BTreeMap<String, String>) -> String {
    let spec = serde_json::Value::String(format!("./{main}")).to_string();
    let spec = spec.trim_matches('"');
    include_str!("scope.js")
        .replace("__MAIN__", spec)
        .replace("__PREFIXES__", &serde_json::to_string(prefixes).expect("a map of strings serialises"))
}

/// One binding a Worker is deployed with.
pub enum Binding {
    R2 { name: String, bucket: String },
    Plain { name: String, text: String },
    Secret { name: String, text: Secret },
}

impl fmt::Debug for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Binding::R2 { name, bucket } => write!(f, "{name}=r2:{bucket}"),
            Binding::Plain { name, text } => write!(f, "{name}={text}"),
            Binding::Secret { name, .. } => write!(f, "{name}=***"),
        }
    }
}

/// A Worker as Cloudflare's upload takes it.
#[derive(Debug)]
pub struct Upload {
    pub main_module: String,
    pub compatibility_date: String,
    pub compatibility_flags: Vec<String>,
    pub bindings: Vec<Binding>,
    /// `(name, source)`, every module the Worker imports included.
    pub modules: Vec<(String, String)>,
}

impl Upload {
    /// The upload's `metadata` part. It holds secret values: send it, never log it.
    pub fn metadata(&self) -> serde_json::Value {
        let bindings: Vec<serde_json::Value> = self
            .bindings
            .iter()
            .map(|b| match b {
                Binding::R2 { name, bucket } => {
                    serde_json::json!({ "type": "r2_bucket", "name": name, "bucket_name": bucket })
                }
                Binding::Plain { name, text } => {
                    serde_json::json!({ "type": "plain_text", "name": name, "text": text })
                }
                Binding::Secret { name, text } => {
                    serde_json::json!({ "type": "secret_text", "name": name, "text": text.expose() })
                }
            })
            .collect();
        serde_json::json!({
            "main_module": self.main_module,
            "compatibility_date": self.compatibility_date,
            "compatibility_flags": self.compatibility_flags,
            "bindings": bindings,
        })
    }
}

/// The Cloudflare calls deploying a Worker takes.
pub trait Edge: Send + Sync {
    /// The account's workers.dev subdomain: `me` in `x.me.workers.dev`.
    fn subdomain(&self) -> impl Future<Output = Result<String>> + Send;
    /// Create or replace the script.
    fn upload(&self, script: &str, upload: &Upload) -> impl Future<Output = Result<()>> + Send;
    /// Serve it at `<script>.<subdomain>.workers.dev`.
    fn enable_subdomain(&self, script: &str) -> impl Future<Output = Result<()>> + Send;
    /// Remove the script. One already gone is not an error.
    fn delete(&self, script: &str) -> impl Future<Output = Result<()>> + Send;
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct WorkerReport {
    pub deployed: Vec<String>,
    pub removed: Vec<String>,
    /// `(script, why)`.
    pub failed: Vec<(String, String)>,
}

pub struct Workers<E> {
    canon: Arc<Canon>,
    hush: Arc<Hush>,
    edge: E,
    /// Henry's R2 bucket, from `[ark]`. Workers that use R2 wait for it.
    bucket: Option<String>,
    /// How long a failed deploy waits before it is tried again.
    retry: Duration,
    subdomain: Mutex<Option<String>>,
}

impl<E: Edge> Workers<E> {
    pub fn new(canon: Arc<Canon>, hush: Arc<Hush>, edge: E, bucket: Option<String>, retry: Duration) -> Self {
        Workers { canon, hush, edge, bucket, retry, subdomain: Mutex::new(None) }
    }

    /// Deploy what changed, remove what is no longer wanted.
    pub async fn pass(&self) -> Result<WorkerReport> {
        let mut report = WorkerReport::default();
        let deploys = self.canon.worker_deploys()?;
        let mut wanted = BTreeSet::new();
        for app in self.canon.apps()? {
            let w = self.canon.wanted_releases(&app.name)?;
            let Some(release) = w.candidate.or(w.live) else { continue };
            let Ok(lease) = Lease::parse(&release.lease) else { continue };
            let bundles = self.canon.worker_bundles(release.id)?;
            for worker in &lease.workers {
                wanted.insert((app.name.clone(), worker.name.clone()));
                let Some(bundle) = bundles.iter().find(|b| b.worker == worker.name) else { continue };
                let current = deploys.iter().find(|d| d.app == app.name && d.worker == worker.name);
                let script = worker.script(&app.name);
                match self.deploy(&app.name, worker, release.id, bundle, current).await {
                    Ok(true) => report.deployed.push(script),
                    Ok(false) => {}
                    Err(why) => {
                        warn!(app = %app.name, worker = %worker.name, "Worker not deployed: {why}");
                        self.canon.claim_worker(&app.name, &worker.name, &script)?;
                        self.canon.set_worker_failed(&app.name, &worker.name, &why)?;
                        report.failed.push((script, why));
                    }
                }
            }
        }
        for d in deploys.iter().filter(|d| !wanted.contains(&(d.app.clone(), d.worker.clone()))) {
            match self.edge.delete(&d.script).await {
                Ok(()) => {
                    info!(script = %d.script, "Worker removed");
                    self.canon.forget_worker(&d.app, &d.worker)?;
                    report.removed.push(d.script.clone());
                }
                Err(e) => warn!(script = %d.script, "Worker not removed: {e}"),
            }
        }
        Ok(report)
    }

    /// Deploy one Worker if what should run differs from what does. `Ok(true)`
    /// when it deployed; `Err` says why it could not.
    async fn deploy(
        &self,
        app: &str,
        worker: &lease::Worker,
        release: i64,
        bundle: &WorkerBundle,
        current: Option<&WorkerDeploy>,
    ) -> Result<bool, String> {
        let script = worker.script(app);
        // What is deployed names the code, the bucket and each secret's
        // version: a secret set again redeploys, its value never stored.
        let versions = self.canon.sealed_versions(app).map_err(|e| e.to_string())?;
        let mut stamp = vec![bundle.hash.clone(), format!("bucket={}", self.bucket.as_deref().unwrap_or_default())];
        for (name, from) in &worker.secrets {
            let v = versions.get(from).ok_or_else(|| format!("waiting for the app's secret {from}"))?;
            stamp.push(format!("{name}={from}@{v}"));
        }
        let stamp = stamp.join(",");
        if let Some(d) = current {
            if d.deployed.as_deref() == Some(stamp.as_str()) {
                if let Some(url) = &d.url {
                    self.set_url(app, worker, url).map_err(|e| e.to_string())?;
                }
                return Ok(false);
            }
            let recent = d.failed_at.is_some_and(|at| canon::now_ms() - at < self.retry.as_millis() as i64);
            if d.error.is_some() && recent {
                return Ok(false);
            }
        }

        let b = &bundle.bundle;
        let main = b["main"].as_str().unwrap_or_default().to_owned();
        let mut modules: Vec<(String, String)> = b["modules"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(name, src)| (name.clone(), src.as_str().unwrap_or_default().to_owned()))
            .collect();
        let mut bindings = Vec::new();
        let mut main_module = main.clone();
        if !worker.buckets.is_empty() {
            let bucket = self
                .bucket
                .clone()
                .ok_or("it stores in Henry's R2 bucket, and there is none yet: set up backups ([ark]) first")?;
            if modules.iter().any(|(n, _)| n == ENTRY) {
                return Err(format!("it has a module named {ENTRY}, which is Henry's"));
            }
            let prefixes: BTreeMap<String, String> =
                worker.buckets.iter().map(|n| (n.clone(), prefix(app, &worker.name, n))).collect();
            modules.push((ENTRY.to_owned(), entry(&main, &prefixes)));
            main_module = ENTRY.to_owned();
            bindings.extend(worker.buckets.iter().map(|n| Binding::R2 { name: n.clone(), bucket: bucket.clone() }));
        }
        for (name, from) in &worker.secrets {
            let text = self
                .hush
                .get(app, from)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("waiting for the app's secret {from}"))?;
            bindings.push(Binding::Secret { name: name.clone(), text });
        }
        for (name, text) in b["vars"].as_object().into_iter().flatten() {
            bindings.push(Binding::Plain { name: name.clone(), text: text.as_str().unwrap_or_default().to_owned() });
        }
        let upload = Upload {
            main_module,
            compatibility_date: b["compatibility_date"].as_str().unwrap_or_default().to_owned(),
            compatibility_flags: b["compatibility_flags"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|f| f.as_str().map(str::to_owned))
                .collect(),
            bindings,
            modules,
        };

        // Noted before the upload: a crash part-way still leaves the script
        // known to be Henry's.
        self.canon.claim_worker(app, &worker.name, &script).map_err(|e| e.to_string())?;
        let subdomain = self.subdomain().await.map_err(|e| e.to_string())?;
        self.edge.upload(&script, &upload).await.map_err(|e| e.to_string())?;
        self.edge.enable_subdomain(&script).await.map_err(|e| e.to_string())?;
        let url = format!("https://{script}.{subdomain}.workers.dev");
        self.set_url(app, worker, &url).map_err(|e| e.to_string())?;
        self.canon.set_worker_deployed(app, &worker.name, &url, &stamp, release).map_err(|e| e.to_string())?;
        info!(app, worker = %worker.name, %url, "Worker deployed");
        Ok(true)
    }

    /// Give the app the Worker's URL, as the secret its lease names.
    fn set_url(&self, app: &str, worker: &lease::Worker, url: &str) -> hush::Result<()> {
        let Some(name) = &worker.url else { return Ok(()) };
        if self.hush.get(app, name)?.is_some_and(|s| s.expose() == url) {
            return Ok(());
        }
        self.hush.set(app, name, &Secret::new(url))
    }

    async fn subdomain(&self) -> Result<String> {
        if let Some(s) = self.subdomain.lock().unwrap().clone() {
            return Ok(s);
        }
        let s = self.edge.subdomain().await?;
        *self.subdomain.lock().unwrap() = Some(s.clone());
        Ok(s)
    }
}

/// What an import copied, and what it found already there and kept.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Imported {
    pub copied: u64,
    pub kept: u64,
}

/// How long workers.dev may take to know a new script.
const PROPAGATION: Duration = Duration::from_secs(90);

/// Copy every object of the R2 bucket `source` into Henry's `bucket` under
/// `prefix`, through a Worker that lives only as long as the copy: Henry's
/// own R2 keys reach only its bucket, a Worker can bind both. Objects already
/// under `prefix` are kept, so an import never overwrites what an app wrote.
/// `url` overrides where the copier answers, for tests.
pub async fn import<E: Edge>(
    edge: &E,
    source: &str,
    bucket: &str,
    prefix: &str,
    url: Option<String>,
) -> Result<Imported, String> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|e| e.to_string())?;
    let token = hex::encode(random);
    let script = format!("henry-import-{}", &token[..12]);
    let upload = Upload {
        main_module: "copy.js".into(),
        compatibility_date: "2025-01-01".into(),
        compatibility_flags: vec![],
        bindings: vec![
            Binding::R2 { name: "FROM".into(), bucket: source.into() },
            Binding::R2 { name: "TO".into(), bucket: bucket.into() },
            Binding::Plain { name: "PREFIX".into(), text: prefix.into() },
            Binding::Secret { name: "TOKEN".into(), text: Secret::new(token.clone()) },
        ],
        modules: vec![("copy.js".into(), include_str!("copy.js").into())],
    };
    edge.upload(&script, &upload).await.map_err(|e| e.to_string())?;
    let copy = async {
        edge.enable_subdomain(&script).await.map_err(|e| e.to_string())?;
        let url = match url {
            Some(u) => u,
            None => format!("https://{script}.{}.workers.dev", edge.subdomain().await.map_err(|e| e.to_string())?),
        };
        let client =
            reqwest::Client::builder().timeout(Duration::from_secs(60)).build().expect("an HTTPS client always builds");
        let mut total = Imported::default();
        let mut cursor: Option<String> = None;
        let mut patience = PROPAGATION;
        loop {
            let batch = copy_batch(&client, &url, &token, cursor.as_deref(), patience).await?;
            total.copied += batch["copied"].as_u64().unwrap_or(0);
            total.kept += batch["kept"].as_u64().unwrap_or(0);
            cursor = batch["cursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                return Ok(total);
            }
            patience = Duration::from_secs(10);
        }
    };
    let result = copy.await;
    // The copier goes whatever happened: it can read and write both buckets.
    if let Err(e) = edge.delete(&script).await {
        warn!(%script, "the import's Worker was not removed: {e}; delete it in the dashboard");
    }
    result
}

/// One batch, asked again while the copier is not yet reachable.
async fn copy_batch(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    cursor: Option<&str>,
    patience: Duration,
) -> Result<serde_json::Value, String> {
    let deadline = std::time::Instant::now() + patience;
    loop {
        let sent = client.post(url).bearer_auth(token).json(&serde_json::json!({ "cursor": cursor })).send().await;
        let why = match sent {
            Ok(res) if res.status().is_success() => return res.json().await.map_err(|e| e.to_string()),
            Ok(res) if res.status() == reqwest::StatusCode::UNAUTHORIZED => {
                return Err("the copier refused Henry's token".into());
            }
            Ok(res) => format!("the copier answered {}", res.status()),
            Err(e) => e.to_string(),
        };
        if std::time::Instant::now() > deadline {
            return Err(why);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use canon::{ReleaseState, Source};

    use super::*;

    #[derive(Default)]
    struct FakeEdge {
        uploads: Mutex<Vec<(String, serde_json::Value, Vec<String>)>>,
        deleted: Mutex<Vec<String>>,
        refuse: Mutex<Option<String>>,
    }

    impl Edge for Arc<FakeEdge> {
        async fn subdomain(&self) -> Result<String> {
            Ok("me".into())
        }
        async fn upload(&self, script: &str, upload: &Upload) -> Result<()> {
            if let Some(why) = self.refuse.lock().unwrap().clone() {
                return Err(crate::Error::Refused("Cloudflare", why));
            }
            let names = upload.modules.iter().map(|(n, _)| n.clone()).collect();
            self.uploads.lock().unwrap().push((script.to_owned(), upload.metadata(), names));
            Ok(())
        }
        async fn enable_subdomain(&self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn delete(&self, script: &str) -> Result<()> {
            self.deleted.lock().unwrap().push(script.to_owned());
            Ok(())
        }
    }

    const LEASE: &str = r#"
name = "cairn"
[[service]]
name = "web"
port = 8080
build = { image = "x" }
health = { http = "/" }
secrets = ["CAIRN_SYNC_TOKEN", "CAIRN_SYNC_URL"]
[[worker]]
name = "sync"
dir = "worker"
buckets = ["BUCKET"]
secrets = { SYNC_TOKEN = "CAIRN_SYNC_TOKEN" }
url = "CAIRN_SYNC_URL"
"#;

    struct World {
        canon: Arc<Canon>,
        hush: Arc<Hush>,
        edge: Arc<FakeEdge>,
        workers: Workers<Arc<FakeEdge>>,
    }

    fn world(bucket: Option<&str>) -> World {
        let canon = Arc::new(Canon::open_in_memory().unwrap());
        let hush = Arc::new(Hush::new(canon.clone(), &hush::Key::generate().unwrap()));
        canon.insert_app(&Lease::parse(LEASE).unwrap(), LEASE, "repo", Source::Operator).unwrap();
        let edge = Arc::new(FakeEdge::default());
        let workers =
            Workers::new(canon.clone(), hush.clone(), edge.clone(), bucket.map(str::to_owned), Duration::from_secs(60));
        World { canon, hush, edge, workers }
    }

    impl World {
        /// A release on trial carrying this Worker code.
        fn trial(&self, code: &str) -> i64 {
            let r = self.canon.create_release("cairn", "abc", LEASE).unwrap();
            let bundle = serde_json::json!({
                "main": "src/worker.js", "compatibility_date": "2026-05-01", "compatibility_flags": [],
                "vars": { "MODE": "prod" }, "modules": { "src/worker.js": code },
            });
            self.canon.put_worker_bundle(r.id, "sync", &format!("h-{code}"), &bundle).unwrap();
            self.canon.set_release_state(r.id, ReleaseState::Trial, None).unwrap();
            r.id
        }
    }

    #[tokio::test]
    async fn a_worker_goes_out_with_its_secret_into_its_own_corner_of_henrys_bucket() {
        let w = world(Some("henry-r2"));
        w.hush.set("cairn", "CAIRN_SYNC_TOKEN", &Secret::new("tok")).unwrap();
        w.trial("export default {}");
        let report = w.workers.pass().await.unwrap();
        assert_eq!(report.deployed, ["henry-cairn-sync"]);

        let (script, meta, modules) = w.edge.uploads.lock().unwrap()[0].clone();
        assert_eq!(script, "henry-cairn-sync");
        assert_eq!(meta["main_module"], ENTRY, "R2 goes through Henry's scope");
        assert_eq!(modules, ["src/worker.js", ENTRY]);
        let bindings = meta["bindings"].as_array().unwrap();
        assert!(
            bindings.contains(&serde_json::json!({ "type": "r2_bucket", "name": "BUCKET", "bucket_name": "henry-r2" }))
        );
        assert!(bindings.contains(&serde_json::json!({ "type": "secret_text", "name": "SYNC_TOKEN", "text": "tok" })));
        assert!(bindings.contains(&serde_json::json!({ "type": "plain_text", "name": "MODE", "text": "prod" })));
        assert_eq!(
            w.hush.get("cairn", "CAIRN_SYNC_URL").unwrap().unwrap().expose(),
            "https://henry-cairn-sync.me.workers.dev",
            "the app learns its Worker's URL"
        );

        // Nothing changed: nothing is uploaded.
        assert!(w.workers.pass().await.unwrap().deployed.is_empty());
        // The secret set again: deployed again, with it.
        std::thread::sleep(std::time::Duration::from_millis(2));
        w.hush.set("cairn", "CAIRN_SYNC_TOKEN", &Secret::new("tok2")).unwrap();
        assert_eq!(w.workers.pass().await.unwrap().deployed, ["henry-cairn-sync"]);
        assert_eq!(w.edge.uploads.lock().unwrap().len(), 2);
        // New code on trial: deployed again.
        w.trial("export default { fetch() {} }");
        assert_eq!(w.workers.pass().await.unwrap().deployed, ["henry-cairn-sync"]);
    }

    #[tokio::test]
    async fn a_worker_waits_for_what_it_needs_and_says_why() {
        let w = world(None);
        w.trial("export default {}");
        let r = w.workers.pass().await.unwrap();
        assert!(r.failed[0].1.contains("waiting for the app's secret CAIRN_SYNC_TOKEN"), "{r:?}");
        w.hush.set("cairn", "CAIRN_SYNC_TOKEN", &Secret::new("tok")).unwrap();
        // Failed a moment ago: not tried again until the retry has passed.
        assert!(w.workers.pass().await.unwrap().failed.is_empty());
        let w = world(None);
        w.hush.set("cairn", "CAIRN_SYNC_TOKEN", &Secret::new("tok")).unwrap();
        w.trial("export default {}");
        let r = w.workers.pass().await.unwrap();
        assert!(r.failed[0].1.contains("set up backups"), "no bucket, no R2: {r:?}");
        assert!(w.edge.uploads.lock().unwrap().is_empty());
        let d = &w.canon.worker_deploys().unwrap()[0];
        assert!(d.error.as_deref().unwrap().contains("set up backups"));
    }

    #[tokio::test]
    async fn only_workers_henry_deployed_are_removed_and_only_once_their_app_is_gone() {
        let w = world(Some("b"));
        w.hush.set("cairn", "CAIRN_SYNC_TOKEN", &Secret::new("tok")).unwrap();
        w.trial("export default {}");
        w.workers.pass().await.unwrap();
        assert!(w.workers.pass().await.unwrap().removed.is_empty());
        w.canon.remove_app("cairn").unwrap();
        let r = w.workers.pass().await.unwrap();
        assert_eq!(r.removed, ["henry-cairn-sync"]);
        assert_eq!(*w.edge.deleted.lock().unwrap(), ["henry-cairn-sync"]);
        assert!(w.canon.worker_deploys().unwrap().is_empty());
        assert!(w.workers.pass().await.unwrap().removed.is_empty(), "removed once");
    }

    #[tokio::test]
    async fn a_refused_upload_is_recorded_once_and_tried_again_later() {
        let w = world(Some("b"));
        w.hush.set("cairn", "CAIRN_SYNC_TOKEN", &Secret::new("tok")).unwrap();
        w.trial("export default {}");
        *w.edge.refuse.lock().unwrap() = Some("403 on /workers/scripts: Authentication error".into());
        let r = w.workers.pass().await.unwrap();
        assert!(r.failed[0].1.contains("Authentication error"));
        assert!(w.hush.get("cairn", "CAIRN_SYNC_URL").unwrap().is_none(), "no URL before a deploy");
        let events = w.canon.events(Some("cairn"), 50).unwrap();
        assert_eq!(events.iter().filter(|e| e.kind == "worker.failed").count(), 1);
    }

    #[test]
    fn the_scope_keeps_a_worker_in_its_own_corner_in_a_real_runtime() {
        if Command::new("node").arg("--version").output().is_err() {
            eprintln!("skipped: no node to run the scope in");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("package.json"), "{\"type\": \"module\"}").unwrap();
        std::fs::write(d.join("src/worker.js"), include_str!("testdata/worker.js")).unwrap();
        std::fs::write(d.join("scope_test.mjs"), include_str!("testdata/scope_test.mjs")).unwrap();
        let prefixes = BTreeMap::from([("BUCKET".to_owned(), prefix("cairn", "sync", "BUCKET"))]);
        std::fs::write(d.join(ENTRY), entry("src/worker.js", &prefixes)).unwrap();
        let out = Command::new("node").arg("scope_test.mjs").current_dir(d).output().unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success() && text.contains("scope ok"), "{text}");
    }

    #[test]
    fn a_secret_never_shows_in_an_uploads_debug() {
        let u = Upload {
            main_module: "w.js".into(),
            compatibility_date: "2026-05-01".into(),
            compatibility_flags: vec![],
            bindings: vec![Binding::Secret { name: "T".into(), text: Secret::new("hunter2") }],
            modules: vec![],
        };
        assert!(!format!("{u:?}").contains("hunter2"));
    }
}

#[cfg(test)]
mod import_tests {
    use std::process::Command;

    use axum::Router;
    use axum::routing::post;

    use super::*;

    #[derive(Default)]
    struct Recorder {
        uploads: Mutex<Vec<(String, serde_json::Value)>>,
        deleted: Mutex<Vec<String>>,
    }

    impl Edge for Arc<Recorder> {
        async fn subdomain(&self) -> Result<String> {
            Ok("me".into())
        }
        async fn upload(&self, script: &str, upload: &Upload) -> Result<()> {
            self.uploads.lock().unwrap().push((script.to_owned(), upload.metadata()));
            Ok(())
        }
        async fn enable_subdomain(&self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn delete(&self, script: &str) -> Result<()> {
            self.deleted.lock().unwrap().push(script.to_owned());
            Ok(())
        }
    }

    /// A stand-in for the copier: two batches, then done; or a refusal.
    async fn copier(refuse: bool) -> String {
        let app = Router::new().route(
            "/",
            post(move |body: axum::Json<serde_json::Value>| async move {
                if refuse {
                    return (axum::http::StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({})));
                }
                let r = match body["cursor"].as_str() {
                    None => serde_json::json!({ "copied": 50, "kept": 0, "cursor": "50" }),
                    Some(_) => serde_json::json!({ "copied": 10, "kept": 1, "cursor": null }),
                };
                (axum::http::StatusCode::OK, axum::Json(r))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    }

    #[tokio::test]
    async fn an_import_copies_through_a_worker_that_is_gone_afterwards() {
        let edge = Arc::new(Recorder::default());
        let url = copier(false).await;
        let got = import(&edge, "cairn-state", "henry", "apps/cairn/sync/BUCKET/", Some(url)).await.unwrap();
        assert_eq!(got, Imported { copied: 60, kept: 1 });
        let (script, meta) = edge.uploads.lock().unwrap()[0].clone();
        assert!(script.starts_with("henry-import-"));
        let bindings = meta["bindings"].as_array().unwrap();
        assert!(
            bindings
                .contains(&serde_json::json!({ "type": "r2_bucket", "name": "FROM", "bucket_name": "cairn-state" }))
        );
        assert!(bindings.contains(&serde_json::json!({ "type": "r2_bucket", "name": "TO", "bucket_name": "henry" })));
        assert_eq!(*edge.deleted.lock().unwrap(), [script]);
    }

    #[tokio::test]
    async fn a_failed_import_still_removes_its_worker() {
        let edge = Arc::new(Recorder::default());
        let url = copier(true).await;
        let err = import(&edge, "cairn-state", "henry", "p/", Some(url)).await.unwrap_err();
        assert!(err.contains("refused"), "{err}");
        assert_eq!(edge.deleted.lock().unwrap().len(), 1);
    }

    #[test]
    fn the_copier_copies_everything_and_overwrites_nothing_in_a_real_runtime() {
        if Command::new("node").arg("--version").output().is_err() {
            eprintln!("skipped: no node to run the copier in");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(d.join("package.json"), "{\"type\": \"module\"}").unwrap();
        std::fs::write(d.join("copy.js"), include_str!("copy.js")).unwrap();
        std::fs::write(d.join("copy_test.mjs"), include_str!("testdata/copy_test.mjs")).unwrap();
        let out = Command::new("node").arg("copy_test.mjs").current_dir(d).output().unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success() && text.contains("copy ok"), "{text}");
    }
}
