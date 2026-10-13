//! Git, through the `git` CLI, in Henry's own checkouts.
//!
//! Every call has a timeout and runs with prompts disabled: hostd once hung
//! forever on a credential dialog that never appears for a background agent.
//! A GitHub token, when Hush has one, reaches git through its config
//! environment variables, never through argv, so it stays out of `ps`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use tokio::process::Command;

use crate::{Error, Result};

const QUICK: Duration = Duration::from_secs(60);
const FETCH: Duration = Duration::from_secs(600);

#[derive(Clone)]
pub struct Git {
    pub program: PathBuf,
    /// Sent as basic auth to github.com, for private repos.
    pub token: Option<String>,
}

impl Default for Git {
    fn default() -> Self {
        Git { program: "git".into(), token: None }
    }
}

impl Git {
    async fn run(&self, cwd: Option<&Path>, args: &[&str], timeout: Duration) -> Result<String> {
        let mut cmd = Command::new(&self.program);
        if let Some(dir) = cwd {
            cmd.arg("-C").arg(dir);
        }
        cmd.args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "true")
            .env("SSH_ASKPASS", "true")
            .env("GCM_INTERACTIVE", "never")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        if let Some(token) = &self.token {
            let basic = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
            cmd.env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "http.https://github.com/.extraheader")
                .env("GIT_CONFIG_VALUE_0", format!("AUTHORIZATION: basic {basic}"));
        }
        let what = format!("git {}", args.first().copied().unwrap_or_default());
        let out = tokio::time::timeout(timeout, cmd.output())
            .await
            .map_err(|_| Error::Transient(format!("{what} timed out")))?
            .map_err(|e| Error::Transient(format!("{what}: {e}")))?;
        if out.status.success() {
            // Untrimmed: `show` returns file contents, which must be exact.
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(Error::Git(format!("{what}: {}", String::from_utf8_lossy(&out.stderr).trim())))
        }
    }

    /// The commit a branch points at on the remote, or `None` if there is no
    /// such branch.
    pub async fn head(&self, url: &str, branch: &str) -> Result<Option<String>> {
        let refname = format!("refs/heads/{branch}");
        let out = self.run(None, &["ls-remote", url, &refname], QUICK).await.map_err(Error::transient)?;
        Ok(out.lines().find_map(|l| {
            let (sha, name) = l.split_once('\t')?;
            (name == refname).then(|| sha.to_owned())
        }))
    }

    /// Make `dir` an exact copy of `commit`: fetched, checked out, and
    /// cleaned of anything else. Henry owns the directory, so force is safe.
    pub async fn checkout(&self, dir: &Path, url: &str, branch: &str, commit: &str) -> Result<()> {
        if !dir.join(".git").exists() {
            std::fs::create_dir_all(dir).map_err(|e| Error::Io(format!("{}: {e}", dir.display())))?;
            self.run(Some(dir), &["init", "--quiet"], QUICK).await?;
            self.run(Some(dir), &["remote", "add", "origin", url], QUICK).await?;
        } else {
            // The repo may have moved; follow it.
            self.run(Some(dir), &["remote", "set-url", "origin", url], QUICK).await?;
        }
        // Fetch the exact commit. Servers that refuse a bare commit get the
        // branch instead, which contains it if it is still on the branch.
        if self.run(Some(dir), &["fetch", "--quiet", "--depth", "1", "origin", commit], FETCH).await.is_err() {
            self.run(Some(dir), &["fetch", "--quiet", "--depth", "50", "origin", branch], FETCH)
                .await
                .map_err(Error::transient)?;
        }
        self.run(
            Some(dir),
            &["-c", "advice.detachedHead=false", "checkout", "--quiet", "--force", "--detach", commit],
            QUICK,
        )
        .await?;
        self.run(Some(dir), &["clean", "--quiet", "-ffdx"], QUICK).await?;
        let at = self.run(Some(dir), &["rev-parse", "HEAD"], QUICK).await?;
        let at = at.trim();
        if at != commit {
            return Err(Error::Git(format!("checked out {at}, wanted {commit}")));
        }
        Ok(())
    }

    /// A file's contents at the checked-out commit, or `None` if absent.
    pub async fn show(&self, dir: &Path, commit: &str, path: &str) -> Result<Option<String>> {
        match self.run(Some(dir), &["show", &format!("{commit}:{path}")], QUICK).await {
            Ok(text) => Ok(Some(text)),
            Err(Error::Git(e)) if e.contains("does not exist") || e.contains("exists on disk, but not in") => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// `owner/repo`, lowercased, from any of the ways a GitHub repo is written.
/// Used to match webhook deliveries to apps.
pub fn repo_slug(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = rest.split('/');
    let (owner, repo) = (parts.next()?, parts.next()?);
    (parts.next().is_none() && !owner.is_empty() && !repo.is_empty())
        .then(|| format!("{}/{}", owner.to_lowercase(), repo.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_match_every_spelling() {
        for url in [
            "https://github.com/SpecBug/Henry",
            "https://github.com/specbug/henry.git",
            "https://github.com/specbug/henry/",
            "git@github.com:specbug/henry.git",
            "ssh://git@github.com/specbug/henry",
        ] {
            assert_eq!(repo_slug(url).as_deref(), Some("specbug/henry"), "{url}");
        }
        assert_eq!(repo_slug("https://gitlab.com/a/b"), None);
        assert_eq!(repo_slug("https://github.com/a/b/tree/main"), None);
        assert_eq!(repo_slug("https://github.com/a"), None);
    }
}
