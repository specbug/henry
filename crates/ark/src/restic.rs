//! restic, through its CLI. The repository's password and the storage
//! credentials travel in the environment, never in argv.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use hush::Secret;
use serde_json::Value;
use tokio::process::Command;

use crate::{Error, Result};

/// restic's exit code for "there is no repository here yet".
const NO_REPOSITORY: i32 = 10;
/// restic's exit code for a wrong password.
const WRONG_PASSWORD: i32 = 12;

pub struct Restic {
    pub program: PathBuf,
    /// e.g. `s3:https://<account>.r2.cloudflarestorage.com/<bucket>/henry`,
    /// `rclone:gdrive:henry`, or a local path.
    pub repository: String,
    pub password: Secret,
    /// Storage credentials, e.g. `AWS_ACCESS_KEY_ID` for R2.
    pub env: Vec<(String, Secret)>,
    pub cache: Option<PathBuf>,
    /// The longest any one restic call may take.
    pub timeout: Duration,
}

/// What a backup made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub id: String,
    pub bytes: u64,
}

/// How many snapshots of each target to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    pub daily: u32,
    pub weekly: u32,
    pub monthly: u32,
}

impl Default for Retention {
    fn default() -> Self {
        Retention { daily: 7, weekly: 4, monthly: 6 }
    }
}

impl Restic {
    /// A restic command for this repository, with nothing on stdin.
    pub fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.env("RESTIC_REPOSITORY", &self.repository)
            .env("RESTIC_PASSWORD", self.password.expose())
            .env_remove("RESTIC_PASSWORD_FILE")
            .env_remove("RESTIC_PASSWORD_COMMAND")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        for (name, value) in &self.env {
            cmd.env(name, value.expose());
        }
        match &self.cache {
            Some(dir) => cmd.env("RESTIC_CACHE_DIR", dir),
            None => cmd.arg("--no-cache"),
        };
        cmd
    }

    async fn run(&self, args: &[&str]) -> Result<std::process::Output> {
        let mut cmd = self.command();
        cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
        tokio::time::timeout(self.timeout, cmd.output())
            .await
            .map_err(|_| Error::Transient(format!("restic {} ran past {:?}", args[0], self.timeout)))?
            .map_err(|e| Error::Restic(format!("{}: {e}", self.program.display())))
    }

    /// Make sure the repository exists, creating it the first time. Returns
    /// whether it was created now. A wrong password is never "no repository".
    pub async fn ensure(&self) -> Result<bool> {
        let out = self.run(&["cat", "config"]).await?;
        match out.status.code() {
            Some(0) => Ok(false),
            Some(NO_REPOSITORY) => {
                let init = self.run(&["init"]).await?;
                if init.status.success() { Ok(true) } else { Err(failure("restic init", &init)) }
            }
            Some(WRONG_PASSWORD) => {
                Err(Error::Restic(format!("{}: wrong password (henry/RESTIC_PASSWORD)", self.repository)))
            }
            // Anything else, typically the network, is worth trying again.
            _ => Err(Error::Transient(failure_text(&format!("opening {}", self.repository), &out))),
        }
    }

    /// Back up what `command` prints, as one file named `filename`.
    pub async fn backup_from(&self, command: &[String], filename: &str, tags: &[String]) -> Result<Snapshot> {
        let mut args = vec!["backup", "--json", "--host", "henry", "--stdin-filename", filename];
        for t in tags {
            args.extend(["--tag", t.as_str()]);
        }
        args.push("--stdin-from-command");
        args.push("--");
        args.extend(command.iter().map(String::as_str));
        let out = self.run(&args).await?;
        parse_backup(&out)
    }

    /// Drop snapshots of one target beyond the retention. Pruning, which
    /// rewrites the repository, happens once a day in Henry's own job.
    pub async fn forget(&self, tags: &[String], keep: &Retention) -> Result<()> {
        let tag = tags.join(",");
        let (d, w, m) = (keep.daily.to_string(), keep.weekly.to_string(), keep.monthly.to_string());
        let args =
            ["forget", "--host", "henry", "--tag", &tag, "--keep-daily", &d, "--keep-weekly", &w, "--keep-monthly", &m];
        let out = self.run(&args).await?;
        if out.status.success() { Ok(()) } else { Err(failure("restic forget", &out)) }
    }

    pub async fn prune(&self) -> Result<()> {
        let out = self.run(&["prune"]).await?;
        if out.status.success() { Ok(()) } else { Err(failure("restic prune", &out)) }
    }

    /// The newest snapshot carrying every one of `tags`.
    pub async fn latest(&self, tags: &[String]) -> Result<Option<Snapshot>> {
        let tag = tags.join(",");
        let out = self.run(&["snapshots", "--json", "--host", "henry", "--tag", &tag, "--latest", "1"]).await?;
        if !out.status.success() {
            return Err(failure("restic snapshots", &out));
        }
        let list: Value = serde_json::from_slice(&out.stdout)
            .map_err(|e| Error::Restic(format!("restic snapshots: unreadable output: {e}")))?;
        let newest = list.as_array().and_then(|l| l.iter().max_by_key(|s| s["time"].as_str().map(str::to_owned)));
        Ok(newest.and_then(|s| {
            Some(Snapshot {
                id: s["id"].as_str()?.to_owned(),
                bytes: s["summary"]["total_bytes_processed"].as_u64().unwrap_or(0),
            })
        }))
    }

    /// A command that writes one file of a snapshot to stdout.
    pub fn dump(&self, snapshot: &str, filename: &str) -> Command {
        let mut cmd = self.command();
        cmd.args(["dump", snapshot, filename]);
        cmd
    }

    /// Bring a mirror up to date with `primary`: created from it the first
    /// time (sharing its chunking, so copies deduplicate), then copied.
    pub async fn mirror_from(&self, primary: &Restic) -> Result<()> {
        let with_primary = |args: &[&str]| {
            let mut cmd = self.command();
            cmd.env("RESTIC_FROM_REPOSITORY", &primary.repository)
                .env("RESTIC_FROM_PASSWORD", primary.password.expose())
                .args(args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            cmd
        };
        let exists = self.run(&["cat", "config"]).await?;
        if exists.status.code() == Some(NO_REPOSITORY) {
            let out = with_primary(&["init", "--copy-chunker-params"])
                .output()
                .await
                .map_err(|e| Error::Restic(e.to_string()))?;
            if !out.status.success() {
                return Err(failure("restic init (mirror)", &out));
            }
        }
        let copy = with_primary(&["copy", "--host", "henry"]).output();
        let out = tokio::time::timeout(self.timeout, copy)
            .await
            .map_err(|_| Error::Transient(format!("restic copy to {} ran past {:?}", self.repository, self.timeout)))?
            .map_err(|e| Error::Restic(e.to_string()))?;
        if out.status.success() { Ok(()) } else { Err(failure("restic copy", &out)) }
    }
}

/// The summary line of `backup --json`, or why there is none.
pub(crate) fn parse_backup(out: &std::process::Output) -> Result<Snapshot> {
    let mut error = None;
    for line in String::from_utf8_lossy(&out.stdout).lines().chain(String::from_utf8_lossy(&out.stderr).lines()) {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        match v["message_type"].as_str() {
            Some("summary") if out.status.success() => {
                if let Some(id) = v["snapshot_id"].as_str() {
                    let bytes = v["total_bytes_processed"].as_u64().unwrap_or(0);
                    return Ok(Snapshot { id: id.to_owned(), bytes });
                }
            }
            Some("exit_error") | Some("error") => {
                let m = v["message"].as_str().or_else(|| v["error"]["message"].as_str()).unwrap_or_default();
                error = Some(m.to_owned());
            }
            _ => {}
        }
    }
    Err(Error::Restic(error.unwrap_or_else(|| failure_text("restic backup", out))))
}

fn failure(what: &str, out: &std::process::Output) -> Error {
    Error::Restic(failure_text(what, out))
}

fn failure_text(what: &str, out: &std::process::Output) -> String {
    let err = String::from_utf8_lossy(&out.stderr);
    let last = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output");
    format!("{what} exited with {}: {last}", out.status)
}
