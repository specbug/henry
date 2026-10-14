//! Cloudflare Workers: the code each release carries for them, and what
//! Hinge has deployed. A Worker runs on Cloudflare, not the Mac, so Canon
//! keeps what was deployed where, to change or remove only Henry's own.

use rusqlite::params;

use crate::{Canon, Result, now_ms, record};

/// A release's code for one Worker: its modules and config, as Press found
/// them in the commit.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerBundle {
    pub worker: String,
    /// Names the bundle, so a deploy happens only when it changed.
    pub hash: String,
    pub bundle: serde_json::Value,
}

/// A Worker Henry deployed, or tried to.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerDeploy {
    pub app: String,
    pub worker: String,
    /// Its script name on Cloudflare.
    pub script: String,
    pub url: Option<String>,
    /// What is deployed: the bundle's hash and the secrets' version.
    pub deployed: Option<String>,
    pub release: Option<i64>,
    pub deployed_at: Option<i64>,
    pub error: Option<String>,
    pub failed_at: Option<i64>,
}

impl Canon {
    pub fn put_worker_bundle(&self, release: i64, worker: &str, hash: &str, bundle: &serde_json::Value) -> Result<()> {
        self.conn()?.execute(
            "INSERT INTO worker_bundles (release, worker, hash, bundle) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (release, worker) DO UPDATE SET hash = excluded.hash, bundle = excluded.bundle",
            params![release, worker, hash, bundle.to_string()],
        )?;
        Ok(())
    }

    pub fn worker_bundles(&self, release: i64) -> Result<Vec<WorkerBundle>> {
        let conn = self.conn()?;
        let mut stmt =
            conn.prepare("SELECT worker, hash, bundle FROM worker_bundles WHERE release = ?1 ORDER BY worker")?;
        let rows = stmt.query_map([release], |r| {
            Ok(WorkerBundle { worker: r.get(0)?, hash: r.get(1)?, bundle: crate::json(r.get::<_, String>(2)?) })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A release made from another's commit (a refresh) carries its Workers.
    pub fn copy_worker_bundles(&self, from: i64, to: i64) -> Result<()> {
        self.conn()?.execute(
            "INSERT OR REPLACE INTO worker_bundles (release, worker, hash, bundle)
             SELECT ?2, worker, hash, bundle FROM worker_bundles WHERE release = ?1",
            params![from, to],
        )?;
        Ok(())
    }

    /// Every Worker Henry has deployed or tried to, apps gone included.
    pub fn worker_deploys(&self) -> Result<Vec<WorkerDeploy>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT app, worker, script, url, deployed, release, deployed_at, error, failed_at
             FROM workers ORDER BY app, worker",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(WorkerDeploy {
                app: r.get(0)?,
                worker: r.get(1)?,
                script: r.get(2)?,
                url: r.get(3)?,
                deployed: r.get(4)?,
                release: r.get(5)?,
                deployed_at: r.get(6)?,
                error: r.get(7)?,
                failed_at: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// About to deploy `script`: noted first, so a crash mid-deploy still
    /// leaves Henry knowing the script is its own.
    pub fn claim_worker(&self, app: &str, worker: &str, script: &str) -> Result<()> {
        self.conn()?.execute(
            "INSERT INTO workers (app, worker, script) VALUES (?1, ?2, ?3)
             ON CONFLICT (app, worker) DO UPDATE SET script = excluded.script",
            params![app, worker, script],
        )?;
        Ok(())
    }

    pub fn set_worker_deployed(&self, app: &str, worker: &str, url: &str, deployed: &str, release: i64) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE workers SET url = ?3, deployed = ?4, release = ?5, deployed_at = ?6, error = NULL, failed_at = NULL
             WHERE app = ?1 AND worker = ?2",
            params![app, worker, url, deployed, release, now_ms()],
        )?;
        record(
            &conn,
            Some(app),
            "worker.deployed",
            &format!("Worker `{worker}` deployed from release {release} at {url}"),
            &serde_json::json!({ "worker": worker, "release": release, "url": url }),
        )
    }

    /// A deploy failed. Recorded once per distinct error, so a failure
    /// retried every tick does not fill the event log.
    pub fn set_worker_failed(&self, app: &str, worker: &str, error: &str) -> Result<()> {
        let conn = self.conn()?;
        let changed = conn.execute(
            "UPDATE workers SET error = ?3, failed_at = ?4 WHERE app = ?1 AND worker = ?2 AND error IS NOT ?3",
            params![app, worker, error, now_ms()],
        )?;
        if changed > 0 {
            record(
                &conn,
                Some(app),
                "worker.failed",
                &format!("Worker `{worker}` not deployed: {error}"),
                &serde_json::json!({ "worker": worker }),
            )?;
        }
        Ok(())
    }

    /// The script is gone from Cloudflare.
    pub fn forget_worker(&self, app: &str, worker: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.execute("DELETE FROM workers WHERE app = ?1 AND worker = ?2", params![app, worker])?;
        record(
            &conn,
            None,
            "worker.removed",
            &format!("Worker `{worker}` of `{app}` removed from Cloudflare"),
            &serde_json::json!({ "app": app, "worker": worker }),
        )
    }
}
