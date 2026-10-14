//! Press against real git (a bare repo on disk) and a fake builder.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use canon::{JobState, Status};

use super::*;
use crate::build::Recipe;

const LEASE: &str = r#"
name = "draw"

[[service]]
name = "web"
port = 8080
build = { dockerfile = "Dockerfile" }
health = { http = "/" }

[[service]]
name = "cache"
port = 6379
build = { image = "redis:7-alpine" }
health = { redis = true }
"#;

#[derive(Default)]
struct FakeBuilder {
    built: Mutex<Vec<String>>,
    /// The base image's version: an image built on it gets an id from it.
    base: Mutex<u32>,
    ids: Mutex<std::collections::BTreeMap<String, String>>,
    pulled: Mutex<Vec<bool>>,
    removed: Mutex<Vec<String>>,
    recipes: Mutex<Vec<String>>,
    /// How many of the next builds fail.
    failures: Mutex<u32>,
}

impl Builder for FakeBuilder {
    async fn has(&self, tag: &str) -> Result<bool> {
        Ok(self.built.lock().unwrap().iter().any(|t| t == tag))
    }

    async fn build(&self, req: &BuildRequest<'_>) -> Result<()> {
        match req.recipe {
            Recipe::Dockerfile { dockerfile, .. } => {
                assert!(dockerfile.exists(), "the Dockerfile is checked out: {}", dockerfile.display());
            }
            Recipe::Railpack { dir } => assert!(dir.is_dir(), "Railpack gets the source: {}", dir.display()),
        }
        self.recipes.lock().unwrap().push(format!("{:?}", req.recipe));
        self.pulled.lock().unwrap().push(req.pull);
        std::fs::create_dir_all(req.log.parent().unwrap()).unwrap();
        let mut failures = self.failures.lock().unwrap();
        if *failures > 0 {
            *failures -= 1;
            std::fs::write(req.log, "step 3/5: RUN make\nmake: *** [all] Error 2\n").unwrap();
            return Err(Error::Build("the build exited with exit status: 2".into()));
        }
        self.built.lock().unwrap().push(req.tag.to_owned());
        let id = format!("sha256:base{}", self.base.lock().unwrap());
        self.ids.lock().unwrap().insert(req.tag.to_owned(), id);
        Ok(())
    }

    async fn image_id(&self, tag: &str) -> Result<Option<String>> {
        Ok(self.ids.lock().unwrap().get(tag).cloned())
    }

    async fn remove(&self, tag: &str) -> Result<()> {
        self.removed.lock().unwrap().push(tag.to_owned());
        Ok(())
    }
}

struct World {
    _dir: tempfile::TempDir,
    root: PathBuf,
    work: PathBuf,
    origin: String,
    canon: Arc<Canon>,
    press: Press<Arc<FakeBuilder>>,
    builder: Arc<FakeBuilder>,
}

impl Builder for Arc<FakeBuilder> {
    async fn has(&self, tag: &str) -> Result<bool> {
        (**self).has(tag).await
    }
    async fn build(&self, req: &BuildRequest<'_>) -> Result<()> {
        (**self).build(req).await
    }
    async fn image_id(&self, tag: &str) -> Result<Option<String>> {
        (**self).image_id(tag).await
    }
    async fn remove(&self, tag: &str) -> Result<()> {
        (**self).remove(tag).await
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let bare = root.join("origin.git");
    let work = root.join("work");
    std::fs::create_dir_all(&bare).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    git(&bare, &["init", "--quiet", "--bare", "--initial-branch=main"]);
    git(&work, &["init", "--quiet", "--initial-branch=main"]);
    let origin = format!("file://{}", bare.display());
    git(&work, &["remote", "add", "origin", &origin]);

    let canon = Arc::new(Canon::open_in_memory().unwrap());
    canon.insert_app(&Lease::parse(LEASE).unwrap(), LEASE, &origin, Source::Operator).unwrap();
    let builder = Arc::new(FakeBuilder::default());
    let settings = Settings {
        checkouts: root.join("checkouts"),
        logs: root.join("logs"),
        limits: lease::Limits::default(),
        build_retries: 1,
        retry_pause: Duration::ZERO,
    };
    let press = Press::new(canon.clone(), Git::default(), builder.clone(), settings);
    World { _dir: dir, root, work, origin, canon, press, builder }
}

impl World {
    /// Commit these files and push. Returns the commit.
    fn push(&self, files: &[(&str, &str)]) -> String {
        for (path, text) in files {
            let p = self.work.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        git(&self.work, &["add", "-A"]);
        git(&self.work, &["commit", "--quiet", "--allow-empty", "-m", "change"]);
        git(&self.work, &["push", "--quiet", "origin", "main"]);
        git(&self.work, &["rev-parse", "HEAD"])
    }

    fn job(&self, commit: Option<&str>, force: bool) -> Job {
        let job = request(&self.canon, "draw", commit, force).unwrap();
        self.canon.claim().unwrap().unwrap();
        job
    }

    async fn deploy(&self) -> Outcome {
        let job = self.job(None, false);
        let out = self.press.run(&job).await.unwrap();
        self.canon.finish(job.id).unwrap();
        out
    }
}

#[tokio::test]
async fn a_push_becomes_a_release_on_trial() {
    let w = world();
    let sha = w.push(&[("Dockerfile", "FROM scratch\n")]);
    let Outcome::Released(id) = w.deploy().await else { panic!("expected a release") };

    let r = w.canon.release(id).unwrap();
    assert_eq!(r.state, ReleaseState::Trial);
    assert_eq!(r.commit_sha, sha);
    let tag = r.images["web"].as_str().unwrap().to_owned();
    assert!(tag.starts_with(&format!("henry/draw-web:{}-", &sha[..12])), "{tag}");
    assert_eq!(r.images["cache"], "redis:7-alpine", "published images pass through untouched");
    assert_eq!(*w.builder.built.lock().unwrap(), [tag]);
    assert_eq!(w.canon.wanted_releases("draw").unwrap().candidate.map(|r| r.id), Some(id));
}

#[tokio::test]
async fn polling_queues_each_commit_once() {
    let w = world();
    w.push(&[("Dockerfile", "FROM scratch\n")]);
    assert_eq!(w.press.poll().await.unwrap(), ["draw"]);
    let job = w.canon.claim().unwrap().unwrap();
    w.press.run(&job).await.unwrap();
    w.canon.finish(job.id).unwrap();
    assert!(w.press.poll().await.unwrap().is_empty(), "this commit is released");

    w.push(&[("README", "more")]);
    assert_eq!(w.press.poll().await.unwrap(), ["draw"]);
}

#[tokio::test]
async fn a_stopped_or_manual_app_is_not_polled() {
    let w = world();
    w.push(&[("Dockerfile", "FROM scratch\n")]);
    w.canon.set_desired("draw", Desired::Stopped).unwrap();
    assert!(w.press.poll().await.unwrap().is_empty());

    w.canon.set_desired("draw", Desired::Running).unwrap();
    let manual = format!("auto_deploy = false\n{LEASE}");
    w.canon.update_lease(&Lease::parse(&manual).unwrap(), &manual, Source::Operator).unwrap();
    assert!(w.press.poll().await.unwrap().is_empty());
}

#[tokio::test]
async fn the_same_commit_is_released_once_unless_forced() {
    let w = world();
    w.push(&[("Dockerfile", "FROM scratch\n")]);
    let Outcome::Released(first) = w.deploy().await else { panic!() };
    assert_eq!(w.deploy().await, Outcome::Already(first));

    let job = w.job(None, true);
    let Outcome::Released(again) = w.press.run(&job).await.unwrap() else { panic!() };
    assert_ne!(again, first);
    assert_eq!(w.builder.built.lock().unwrap().len(), 1, "the image for this commit is reused, not rebuilt");
    assert_eq!(w.canon.release(first).unwrap().state, ReleaseState::Superseded);
}

#[tokio::test]
async fn a_newer_push_supersedes_the_older_trial() {
    let w = world();
    w.push(&[("Dockerfile", "FROM scratch\n")]);
    let Outcome::Released(a) = w.deploy().await else { panic!() };
    w.push(&[("Dockerfile", "FROM scratch\nLABEL v=2\n")]);
    let Outcome::Released(b) = w.deploy().await else { panic!() };
    assert_eq!(w.canon.release(a).unwrap().state, ReleaseState::Superseded);
    assert_eq!(w.canon.wanted_releases("draw").unwrap().candidate.map(|r| r.id), Some(b));
}

#[tokio::test]
async fn the_repos_own_lease_wins() {
    let w = world();
    let repo_lease = LEASE.replace("port = 8080", "port = 9090");
    w.push(&[("Dockerfile", "FROM scratch\n"), ("lease.toml", &repo_lease)]);
    let Outcome::Released(id) = w.deploy().await else { panic!() };
    let app = w.canon.app("draw").unwrap();
    assert_eq!(app.source, Source::Repo);
    assert_eq!(app.lease().unwrap().service("web").unwrap().port, Some(9090));
    assert_eq!(w.canon.release(id).unwrap().lease, repo_lease);
}

#[tokio::test]
async fn a_bad_lease_in_the_repo_fails_the_release_and_is_not_retried() {
    let w = world();
    let bad = LEASE.replace("health = { http = \"/\" }", "privileged = true");
    w.push(&[("Dockerfile", "FROM scratch\n"), ("lease.toml", &bad)]);
    let Outcome::Failed(id, why) = w.deploy().await else { panic!() };
    assert!(why.contains("lease.toml"), "{why}");
    assert_eq!(w.canon.release(id).unwrap().state, ReleaseState::Failed);
    assert_eq!(w.canon.app("draw").unwrap().source, Source::Operator, "the stored lease is untouched");
    assert!(w.press.poll().await.unwrap().is_empty(), "a failed commit waits for a new push");
    assert!(w.builder.built.lock().unwrap().is_empty(), "nothing was built");

    let renamed = LEASE.replace("name = \"draw\"", "name = \"other\"");
    w.push(&[("lease.toml", &renamed)]);
    let Outcome::Failed(_, why) = w.deploy().await else { panic!() };
    assert!(why.contains("names the app `other`"), "{why}");
}

#[tokio::test]
async fn a_build_gets_one_retry() {
    let w = world();
    w.push(&[("Dockerfile", "FROM scratch\n")]);
    *w.builder.failures.lock().unwrap() = 1;
    assert!(matches!(w.deploy().await, Outcome::Released(_)), "the second attempt succeeds");

    w.push(&[("Dockerfile", "FROM scratch\nLABEL v=2\n")]);
    *w.builder.failures.lock().unwrap() = 2;
    let Outcome::Failed(id, why) = w.deploy().await else { panic!() };
    assert!(why.contains("`web` did not build"), "{why}");
    assert!(why.contains("make: *** [all] Error 2"), "the log's tail is in the reason: {why}");
    assert_eq!(w.canon.release(id).unwrap().state, ReleaseState::Failed);
    assert_eq!(w.canon.app("draw").unwrap().status, Status::Onboarding, "a failed build changes no app status");
}

#[tokio::test]
async fn a_service_without_a_dockerfile_is_built_by_railpack() {
    let w = world();
    let lease = LEASE.replace("build = { dockerfile = \"Dockerfile\" }", "build = { railpack = \"app\" }");
    w.push(&[("lease.toml", &lease), ("app/package.json", "{}")]);
    let Outcome::Released(id) = w.deploy().await else { panic!() };
    let recipes = w.builder.recipes.lock().unwrap().clone();
    assert_eq!(recipes.len(), 1);
    assert!(recipes[0].starts_with("Railpack") && recipes[0].contains("checkouts/draw/app"), "{recipes:?}");
    assert!(w.canon.release(id).unwrap().images["web"].as_str().unwrap().starts_with("henry/draw-web:"));
}

#[tokio::test]
async fn overlays_are_written_into_the_checkout_and_name_the_image() {
    let w = world();
    let lease =
        LEASE.replace("build = { dockerfile = \"Dockerfile\" }", "build = { dockerfile = \".henry/Dockerfile\" }");
    w.canon.update_lease(&Lease::parse(&lease).unwrap(), &lease, Source::Ghost).unwrap();
    w.canon.set_overlays("draw", &[(".henry/Dockerfile".into(), "FROM scratch\n".into())]).unwrap();
    let sha = w.push(&[("README.md", "hi")]);
    let Outcome::Released(first) = w.deploy().await else { panic!() };
    let checkout = w.root.join("checkouts/draw");
    assert_eq!(std::fs::read_to_string(checkout.join(".henry/Dockerfile")).unwrap(), "FROM scratch\n");

    // The same commit with a different Dockerfile is a different image.
    w.canon.set_overlays("draw", &[(".henry/Dockerfile".into(), "FROM scratch\nLABEL v=2\n".into())]).unwrap();
    let job = w.job(Some(&sha), true);
    let Outcome::Released(second) = w.press.run(&job).await.unwrap() else { panic!() };
    let image = |id| w.canon.release(id).unwrap().images["web"].as_str().unwrap().to_owned();
    assert_ne!(image(first), image(second));
    assert_eq!(w.builder.built.lock().unwrap().len(), 2, "built again, not reused");
}

#[tokio::test]
async fn the_checkout_is_exact_and_clean() {
    let w = world();
    let first = w.push(&[("Dockerfile", "FROM scratch\n"), ("old.txt", "x")]);
    w.deploy().await;
    let checkout = w.root.join("checkouts/draw");
    std::fs::write(checkout.join("junk"), "left behind").unwrap();
    std::fs::remove_file(w.work.join("old.txt")).unwrap();
    w.push(&[]);
    w.deploy().await;
    assert!(!checkout.join("junk").exists(), "untracked files are cleaned");
    assert!(!checkout.join("old.txt").exists(), "deleted files are gone");
    assert_ne!(git(&checkout, &["rev-parse", "HEAD"]), first);
}

#[tokio::test]
async fn an_interrupted_build_resumes_its_release() {
    let w = world();
    let sha = w.push(&[("Dockerfile", "FROM scratch\n")]);
    let half = w.canon.create_release("draw", &sha, LEASE).unwrap();
    w.canon.set_release_state(half.id, ReleaseState::Building, None).unwrap();
    assert_eq!(w.deploy().await, Outcome::Released(half.id));
}

#[tokio::test]
async fn an_explicit_commit_is_released_even_if_not_the_head() {
    let w = world();
    let old = w.push(&[("Dockerfile", "FROM scratch\n")]);
    w.push(&[("Dockerfile", "FROM scratch\nLABEL v=2\n")]);
    let job = w.job(Some(&old), false);
    let Outcome::Released(id) = w.press.run(&job).await.unwrap() else { panic!() };
    assert_eq!(w.canon.release(id).unwrap().commit_sha, old);
}

#[tokio::test]
async fn a_missing_branch_is_an_error_not_a_release() {
    let w = world();
    let job = w.job(None, false);
    let err = w.press.run(&job).await.unwrap_err();
    assert!(err.to_string().contains("no branch"), "{err}");
    assert!(w.canon.releases("draw", 5).unwrap().is_empty());
    assert_eq!(w.canon.job(job.id).unwrap().state, JobState::Running);
    let _ = &w.origin;
}

#[tokio::test]
async fn a_refresh_releases_only_when_a_base_image_moved() {
    let w = world();
    assert_eq!(w.press.refresh("draw").await.unwrap(), Refresh::NothingLive);
    w.push(&[("Dockerfile", "FROM scratch\n")]);
    let Outcome::Released(first) = w.deploy().await else { panic!() };
    w.canon.set_release_state(first, ReleaseState::Live, None).unwrap();

    // Same base: rebuilt with a pull, found identical, and dropped.
    assert_eq!(w.press.refresh("draw").await.unwrap(), Refresh::Unchanged);
    assert_eq!(*w.builder.pulled.lock().unwrap(), [false, true], "a refresh pulls; a push does not");
    assert_eq!(w.builder.removed.lock().unwrap().len(), 1);

    // A patched base: a new release of the same commit goes on trial.
    *w.builder.base.lock().unwrap() += 1;
    let Refresh::Released(next, changed) = w.press.refresh("draw").await.unwrap() else { panic!() };
    assert_eq!(changed, ["web"]);
    let r = w.canon.release(next).unwrap();
    assert_eq!(r.state, ReleaseState::Trial);
    assert_eq!(r.commit_sha, w.canon.release(first).unwrap().commit_sha);
    assert_eq!(r.images["cache"], "redis:7-alpine", "a published image is left as it is");
    assert_ne!(r.images["web"], w.canon.release(first).unwrap().images["web"]);
    assert!(r.reason.unwrap().contains("base images refreshed: web"));

    assert_eq!(w.press.refresh("draw").await.unwrap(), Refresh::Busy, "one trial at a time");
}

#[tokio::test]
async fn a_release_carries_its_workers_code_or_fails_saying_why() {
    let w = world();
    let lease = format!("{LEASE}\n[[worker]]\nname = \"sync\"\ndir = \"worker\"\n");
    w.push(&[
        ("Dockerfile", "FROM scratch\n"),
        ("lease.toml", &lease),
        ("worker/wrangler.toml", "name = \"draw-sync\"\nmain = \"index.js\"\n"),
        ("worker/index.js", "export default {}\n"),
    ]);
    let Outcome::Released(id) = w.deploy().await else { panic!("expected a release") };
    let bundles = w.canon.worker_bundles(id).unwrap();
    assert_eq!((bundles.len(), bundles[0].worker.as_str()), (1, "sync"));
    assert_eq!(bundles[0].bundle["modules"]["index.js"], "export default {}\n");

    w.push(&[("worker/index.js", "import { Hono } from 'hono';\nexport default new Hono();\n")]);
    let Outcome::Failed(id, why) = w.deploy().await else { panic!("expected a failure") };
    assert!(why.contains("imports hono"), "{why}");
    assert_eq!(w.canon.release(id).unwrap().state, ReleaseState::Failed);
}
