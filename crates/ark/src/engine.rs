//! The container engine's CLI, for what Ark needs beyond Hold's API: one-off
//! helper containers, `exec`, pausing, and streams in and out of volumes.
//! Every path and name Ark puts in a shell script arrives as an environment
//! variable, never spliced into the script.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::{Error, Result};

/// A small image with `sqlite3` and GNU `tar`, built once on first use.
const HELPER_DOCKERFILE: &str = "FROM alpine:3.20\nRUN apk add --no-cache sqlite tar\n";

pub struct Engine {
    /// `podman` or `docker`.
    pub program: PathBuf,
    /// The helper's tag, e.g. `henry/ark-helper:1`.
    pub helper: String,
    pub timeout: Duration,
}

impl Engine {
    pub fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.stdin(Stdio::null()).kill_on_drop(true);
        cmd
    }

    /// Run and return stdout, trimmed; fail with stderr's last line.
    pub async fn run<S: AsRef<str>>(&self, args: &[S]) -> Result<String> {
        let args: Vec<&str> = args.iter().map(AsRef::as_ref).collect();
        let mut cmd = self.command();
        cmd.args(&args).stdout(Stdio::piped()).stderr(Stdio::piped());
        let out = tokio::time::timeout(self.timeout, cmd.output())
            .await
            .map_err(|_| Error::Engine(format!("{} {} ran past {:?}", self.program.display(), args[0], self.timeout)))?
            .map_err(|e| Error::Engine(format!("{}: {e}", self.program.display())))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
        } else {
            let err = String::from_utf8_lossy(&out.stderr);
            let last = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output");
            Err(Error::Engine(format!("{} {}: {last}", self.program.display(), args.join(" "))))
        }
    }

    /// Build the helper image unless it exists.
    pub async fn ensure_helper(&self) -> Result<()> {
        if self.run(&["image", "inspect", &self.helper]).await.is_ok() {
            return Ok(());
        }
        let mut cmd = self.command();
        cmd.args(["build", "-t", &self.helper, "-"]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| Error::Engine(format!("{}: {e}", self.program.display())))?;
        let mut stdin = child.stdin.take().expect("stdin is piped");
        stdin.write_all(HELPER_DOCKERFILE.as_bytes()).await.map_err(|e| Error::Engine(e.to_string()))?;
        drop(stdin);
        let out = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| Error::Engine("building the backup helper timed out".into()))?
            .map_err(|e| Error::Engine(e.to_string()))?;
        if out.status.success() {
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&out.stderr);
            Err(Error::Engine(format!("building the backup helper: {}", err.lines().last().unwrap_or_default())))
        }
    }

    /// The running containers that mount `volume`.
    pub async fn users(&self, volume: &str) -> Result<Vec<String>> {
        let filter = format!("volume={volume}");
        let out = self.run(&["ps", "-q", "--filter", &filter]).await?;
        Ok(out.lines().map(str::to_owned).filter(|l| !l.is_empty()).collect())
    }

    pub async fn remove_container(&self, name: &str) {
        let _ = self.run(&["rm", "-f", name]).await;
    }

    pub async fn remove_volume(&self, name: &str) {
        let _ = self.run(&["volume", "rm", "-f", name]).await;
    }

    /// The argv of a helper container over `volume` (mounted at /v) running
    /// `script`, reading stdin and with no network.
    pub fn helper_args(&self, volume: &str, read_only: bool, env: &[(&str, &str)], script: &str) -> Vec<String> {
        let mut args = vec![self.program.to_string_lossy().into_owned()];
        let mount = format!("{volume}:/v{}", if read_only { ":ro" } else { "" });
        args.extend(["run", "--rm", "-i", "--network", "none", "-v"].map(String::from));
        args.push(mount);
        for (k, v) in env {
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
        args.push(self.helper.clone());
        args.extend(["sh".into(), "-c".into(), script.into()]);
        args
    }

    /// [`Engine::helper_args`] as a command, for piping into.
    pub fn helper(&self, volume: &str, read_only: bool, env: &[(&str, &str)], script: &str) -> Command {
        let args = self.helper_args(volume, read_only, env, script);
        let mut cmd = self.command();
        cmd.args(&args[1..]);
        cmd
    }

    /// Wait until `args` (an `exec` probe) succeeds and prints `want`, if set.
    pub async fn wait_for(&self, args: &[&str], want: Option<&str>, patience: Duration) -> Result<String> {
        let deadline = tokio::time::Instant::now() + patience;
        loop {
            let last = self.run(args).await;
            match &last {
                Ok(out) if want.is_none_or(|w| out.trim() == w) => return Ok(out.clone()),
                _ if tokio::time::Instant::now() >= deadline => {
                    let why = match last {
                        Ok(out) => format!("answered {out:?}"),
                        Err(e) => e.to_string(),
                    };
                    return Err(Error::Engine(format!("not ready after {patience:?}: {why}")));
                }
                _ => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        }
    }
}

/// Pipe `from`'s stdout into `into`'s stdin and wait for both. Either one
/// failing fails the pipe.
pub async fn pipe(mut from: Command, mut into: Command, timeout: Duration) -> Result<String> {
    from.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut source = from.spawn().map_err(|e| Error::Restic(format!("restic: {e}")))?;
    let stdout: Stdio = source
        .stdout
        .take()
        .expect("stdout is piped")
        .try_into()
        .map_err(|e: std::io::Error| Error::Engine(e.to_string()))?;
    into.stdin(stdout).stdout(Stdio::piped()).stderr(Stdio::piped());
    let sink = into.spawn().map_err(|e| Error::Engine(format!("engine: {e}")))?;
    let both = async { tokio::join!(source.wait_with_output(), sink.wait_with_output()) };
    let (src, snk) =
        tokio::time::timeout(timeout, both).await.map_err(|_| Error::Engine("restore timed out".into()))?;
    let src = src.map_err(|e| Error::Restic(e.to_string()))?;
    let snk = snk.map_err(|e| Error::Engine(e.to_string()))?;
    let tail =
        |b: &[u8]| String::from_utf8_lossy(b).lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").to_owned();
    if !src.status.success() {
        return Err(Error::Restic(format!("restic dump exited with {}: {}", src.status, tail(&src.stderr))));
    }
    if !snk.status.success() {
        return Err(Error::Engine(format!("restoring exited with {}: {}", snk.status, tail(&snk.stderr))));
    }
    Ok(String::from_utf8_lossy(&snk.stdout).trim().to_owned())
}

// --- scripts ---------------------------------------------------------------

/// Tar a SQLite volume with the database copied by `.backup`, so the copy is
/// consistent while the app keeps writing. `P` is the database's path in the
/// volume. Without a database yet, the volume is tarred as it is.
pub const SQLITE_BACKUP: &str = r#"set -euo pipefail
mkdir -p /out
tar -C /v -cf - --exclude="./$P" --exclude="./$P-wal" --exclude="./$P-shm" --exclude="./$P-journal" . | tar -C /out -xf -
if [ -f "/v/$P" ]; then
  mkdir -p "/out/$(dirname "$P")"
  sqlite3 "/v/$P" ".backup '/out/$P'"
fi
tar -C /out -cf - ."#;

/// Snapshot Redis with BGSAVE, wait for it, then print the dump file.
pub const REDIS_BACKUP: &str = r#"set -u
redis-cli BGSAVE >/dev/null
i=0
while redis-cli INFO persistence | grep -q 'rdb_bgsave_in_progress:1'; do
  i=$((i+1)); [ "$i" -gt 3000 ] && { echo "BGSAVE did not finish in 10 minutes" >&2; exit 1; }
  sleep 0.2
done
redis-cli INFO persistence | grep -q 'rdb_last_bgsave_status:ok' || { echo "BGSAVE failed" >&2; exit 1; }
dir=$(redis-cli CONFIG GET dir | sed -n 2p)
file=$(redis-cli CONFIG GET dbfilename | sed -n 2p)
cat "$dir/$file""#;

/// Empty the volume, then unpack the tar on stdin into it.
pub const EXTRACT: &str = "set -eu\nfind /v -mindepth 1 -delete\ntar -C /v -xf -";

/// Empty the volume, then write stdin to `$F` in it.
pub const WRITE_FILE: &str = "set -eu\nfind /v -mindepth 1 -delete\ncat > \"/v/$F\"";

/// Count the files in the volume.
pub const COUNT: &str = "find /v -type f | wc -l";

/// `ok` when the database at `$P` passes SQLite's integrity check, or a
/// note when there is none.
pub const SQLITE_CHECK: &str =
    r#"if [ -f "/v/$P" ]; then sqlite3 "/v/$P" 'PRAGMA integrity_check' | head -1; else echo absent; fi"#;
