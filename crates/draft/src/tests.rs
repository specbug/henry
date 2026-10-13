//! Draft against real git (a bare repo on disk), a fake builder, a scripted
//! planner, and releases marked live or failed by hand in Proof's place.

use std::collections::VecDeque;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use canon::JobState;
use hush::Key;
use press::build::{BuildRequest, Recipe};

use super::*;

#[derive(Default)]
struct FakeBuilder {
    built: Mutex<Vec<String>>,
    /// Railpack builds fail, as when Railpack cannot detect the app.
    railpack_fails: bool,
}

impl Builder for &'static FakeBuilder {
    async fn has(&self, tag: &str) -> press::Result<bool> {
        Ok(self.built.lock().unwrap().iter().any(|t| t == tag))
    }

    async fn build(&self, req: &BuildRequest<'_>) -> press::Result<()> {
        std::fs::create_dir_all(req.log.parent().unwrap()).unwrap();
        match req.recipe {
            Recipe::Railpack { .. } if self.railpack_fails => {
                std::fs::write(req.log, "railpack: no start command could be found\n").unwrap();
                return Err(press::Error::Build("the build exited with exit status: 1".into()));
            }
            Recipe::Dockerfile { dockerfile, .. } => {
                let text = std::fs::read_to_string(dockerfile).unwrap();
                if text.contains("BROKEN") {
                    std::fs::write(req.log, "COPY failed: no such file\n").unwrap();
                    return Err(press::Error::Build("the build exited with exit status: 1".into()));
                }
            }
            _ => {}
        }
        self.built.lock().unwrap().push(req.tag.to_owned());
        Ok(())
    }
}

/// Plans handed out in order, and every brief it was given.
#[derive(Default)]
struct Script {
    plans: Mutex<VecDeque<Result<Plan, ghost::Error>>>,
    seen: Mutex<Vec<Vec<String>>>,
}

impl Planner for &'static Script {
    fn name(&self) -> String {
        "meta/scripted".into()
    }

    async fn propose(&self, brief: &Brief<'_>, _: &Workspace, _: u64, _: &ghost::Settings) -> ghost::Outcome {
        self.seen.lock().unwrap().push(brief.attempts.iter().map(|a| a.failure.clone()).collect());
        let plan =
            self.plans.lock().unwrap().pop_front().unwrap_or(Err(ghost::Error::Exhausted("script ran out".into())));
        ghost::Outcome { plan, usage: ghost::Usage { input: 1000, output: 100 } }
    }
}

struct World {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    work: std::path::PathBuf,
    origin: String,
    canon: Arc<Canon>,
    hush: Arc<Hush>,
    builder: &'static FakeBuilder,
    script: &'static Script,
}

fn git(dir: &Path, args: &[&str]) {
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
}

fn world(files: &[(&str, &str)], railpack_fails: bool, plans: Vec<Result<Plan, ghost::Error>>) -> World {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let bare = root.join("origin.git");
    let work = root.join("work");
    std::fs::create_dir_all(&bare).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    git(&bare, &["init", "--quiet", "--bare", "--initial-branch=trunk"]);
    git(&work, &["init", "--quiet", "--initial-branch=trunk"]);
    let origin = format!("file://{}", bare.display());
    git(&work, &["remote", "add", "origin", &origin]);
    let canon = Arc::new(Canon::open_in_memory().unwrap());
    let hush = Arc::new(Hush::new(canon.clone(), &Key::generate().unwrap()));
    let builder = Box::leak(Box::new(FakeBuilder { railpack_fails, ..FakeBuilder::default() }));
    let script = Box::leak(Box::new(Script { plans: Mutex::new(plans.into()), ..Script::default() }));
    let w = World { _dir: dir, root, work, origin, canon, hush, builder, script };
    w.push(files);
    w
}

impl World {
    fn push(&self, files: &[(&str, &str)]) {
        for (path, text) in files {
            let p = self.work.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        git(&self.work, &["add", "-A"]);
        git(&self.work, &["commit", "--quiet", "--allow-empty", "-m", "change"]);
        git(&self.work, &["push", "--quiet", "origin", "trunk"]);
    }

    fn draft(&self, with_ai: bool, max_rounds: i64) -> Draft<&'static FakeBuilder, &'static Script> {
        let settings = press::Settings {
            checkouts: self.root.join("checkouts"),
            logs: self.root.join("logs"),
            limits: lease::Limits::default(),
            build_retries: 0,
            retry_pause: Duration::ZERO,
        };
        let press = Press::new(self.canon.clone(), Git::default(), self.builder, settings);
        let settings = Settings { checkouts: self.root.join("checkouts"), max_rounds, ..Settings::default() };
        let planner = with_ai.then_some(self.script);
        Draft::new(self.canon.clone(), self.hush.clone(), press, Git::default(), planner, settings)
    }

    fn start(&self) {
        start(&self.canon, "draw", &self.origin, None, Some("draw.example.com")).unwrap();
    }

    /// Run the queued draft job, as the daemon's worker would.
    async fn work(&self, with_ai: bool, max_rounds: i64) -> Result<String> {
        let job = self.canon.claim().unwrap().expect("a draft job is queued");
        assert_eq!(job.kind, JOB);
        let out = self.draft(with_ai, max_rounds).run(&job).await;
        match &out {
            Ok(_) => self.canon.finish(job.id).unwrap(),
            Err(e) => self.canon.fail(job.id, &e.to_string(), None).unwrap(),
        }
        out
    }

    fn stage(&self) -> (DraftStage, Option<String>) {
        let d = self.canon.draft("draw").unwrap().unwrap();
        (d.stage, d.detail)
    }

    fn release(&self) -> i64 {
        self.canon.draft("draw").unwrap().unwrap().release.expect("a release on trial")
    }

    fn secret(&self, name: &str) -> Option<String> {
        self.hush.get("draw", name).unwrap().map(|s| s.expose().to_owned())
    }
}

fn ghost_plan(dockerfile: &str) -> Result<Plan, ghost::Error> {
    Ok(Plan {
        lease: "name = \"draw\"\nbranch = \"trunk\"\n[[service]]\nname = \"web\"\nport = 3000\nbuild = { dockerfile = \".henry/Dockerfile\" }\nhealth = { http = \"/health\" }\npublic = { hostname = \"draw.example.com\" }\nsecrets = [\"SESSION_KEY\"]\n".into(),
        files: vec![(".henry/Dockerfile".into(), dockerfile.into())],
        generate: vec![Generate { name: "SESSION_KEY".into(), template: None }],
        notes: "node on 3000".into(),
    })
}

const REPO_LEASE: &str = "name = \"draw\"\nbranch = \"trunk\"\n[[service]]\nname = \"web\"\nport = 80\nbuild = { dockerfile = \"Dockerfile\" }\nhealth = { http = \"/\" }\npublic = { hostname = \"draw.example.com\" }\n";

#[tokio::test]
async fn a_repo_with_its_own_lease_goes_to_trial_without_ai_and_then_live() {
    let w = world(&[("lease.toml", REPO_LEASE), ("Dockerfile", "FROM scratch\n")], false, vec![]);
    w.start();
    let out = w.work(true, 5).await.unwrap();
    assert!(out.contains("on trial"), "{out}");
    assert_eq!(w.stage().0, DraftStage::Trial);
    assert!(w.script.seen.lock().unwrap().is_empty(), "the repo's own lease needs no AI");
    let app = w.canon.app("draw").unwrap();
    assert_eq!(app.source, Source::Repo);
    let d = w.canon.draft("draw").unwrap().unwrap();
    assert_eq!((d.round, d.branch), (1, None), "the default branch was found, not assumed");

    w.canon.set_release_state(w.release(), ReleaseState::Live, Some("proven")).unwrap();
    let report = pass(&w.canon, 5).unwrap();
    assert_eq!(report.live, ["draw"]);
    assert_eq!(w.stage().0, DraftStage::Live);
    assert_eq!(w.canon.proposals("draw", 1).unwrap()[0].verdict.as_deref(), Some(ACCEPTED));
}

#[tokio::test]
async fn sound_drafts_a_plain_repo_and_gives_it_a_private_database() {
    let w = world(
        &[("Dockerfile", "FROM node:22\nEXPOSE 3000\n"), ("package.json", r#"{"dependencies": {"pg": "^8"}}"#)],
        false,
        vec![],
    );
    w.start();
    w.work(false, 5).await.unwrap();
    assert_eq!(w.stage().0, DraftStage::Trial);
    let app = w.canon.app("draw").unwrap();
    assert_eq!(app.source, Source::Sound);
    let lease = app.lease().unwrap();
    assert_eq!(lease.branch, "trunk");
    assert!(lease.service("db").is_some());

    let password = w.secret("POSTGRES_PASSWORD").unwrap();
    assert_eq!(password.len(), 64, "32 random bytes, in hex");
    assert_eq!(w.secret("DATABASE_URL").unwrap(), format!("postgres://app:{password}@db:5432/app"));
    let proposal = &w.canon.proposals("draw", 1).unwrap()[0];
    assert_eq!(proposal.source, "sound");
    assert!(!proposal.generate.to_string().contains(&password), "a proposal names secrets, never holds them");

    // It fails its trial, and there is no AI to revise it.
    w.canon.set_release_state(w.release(), ReleaseState::Failed, Some("failed proof: `web` never held")).unwrap();
    assert_eq!(pass(&w.canon, 5).unwrap().retried, ["draw"]);
    w.work(false, 5).await.unwrap();
    let (stage, detail) = w.stage();
    assert_eq!(stage, DraftStage::Stuck);
    assert!(detail.as_deref().unwrap().contains("no AI provider"), "{detail:?}");
    assert_eq!(w.canon.app("draw").unwrap().status, Status::Stuck);
}

#[tokio::test]
async fn a_failed_build_goes_back_to_ghost_with_its_log() {
    let w = world(
        &[("package.json", r#"{"scripts": {"start": "node server.js"}}"#), ("server.js", "")],
        true,
        vec![ghost_plan("FROM node:22\nBROKEN\n"), ghost_plan("FROM node:22\nCOPY . .\n")],
    );
    w.start();
    w.work(true, 5).await.unwrap();
    assert_eq!(w.stage().0, DraftStage::Trial);

    let seen = w.script.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "Sound's Railpack plan went first, then Ghost twice");
    assert!(seen[0][0].contains("no start command could be found"), "Railpack's log reached Ghost: {seen:?}");
    assert!(seen[1].iter().any(|f| f.contains("COPY failed")), "{seen:?}");

    let proposals = w.canon.proposals("draw", 10).unwrap();
    let sources: Vec<&str> = proposals.iter().rev().map(|p| p.source.as_str()).collect();
    assert_eq!(sources, ["sound", "ghost", "ghost"]);
    assert_eq!(proposals[0].verdict, None, "the newest is on trial");
    assert_eq!(proposals[1].model.as_deref(), Some("meta/scripted"));
    assert_eq!(w.canon.overlays("draw").unwrap()[0].1, "FROM node:22\nCOPY . .\n");
    assert!(w.secret("SESSION_KEY").is_some());
    assert_eq!(w.canon.draft("draw").unwrap().unwrap().spent, 2200);
    assert_eq!(w.canon.draft("draw").unwrap().unwrap().round, 3);
}

#[tokio::test]
async fn a_failed_proof_starts_a_round_that_knows_why() {
    let w =
        world(&[("server.js", "")], false, vec![ghost_plan("FROM node:22\n"), ghost_plan("FROM node:22\nENV X=1\n")]);
    w.start();
    w.work(true, 5).await.unwrap();
    let first = w.release();
    w.canon
        .set_release_state(first, ReleaseState::Failed, Some("failed proof: `web` logged a crash: panic: boom"))
        .unwrap();
    assert_eq!(pass(&w.canon, 5).unwrap().retried, ["draw"]);
    assert_eq!(w.stage().0, DraftStage::Plan);

    w.work(true, 5).await.unwrap();
    assert_eq!(w.stage().0, DraftStage::Trial);
    assert_ne!(w.release(), first);
    let seen = w.script.seen.lock().unwrap().clone();
    assert!(seen[1][0].contains("panic: boom"), "{seen:?}");
}

#[tokio::test]
async fn the_repair_budget_ends_in_stuck() {
    let broken = || ghost_plan("FROM node:22\nBROKEN\n");
    let w = world(&[("server.js", "")], false, vec![broken(), broken(), broken()]);
    w.start();
    w.work(true, 2).await.unwrap();
    let (stage, detail) = w.stage();
    assert_eq!(stage, DraftStage::Stuck);
    assert!(detail.as_deref().unwrap().contains("out of repair rounds after 2"), "{detail:?}");
    assert_eq!(w.script.seen.lock().unwrap().len(), 2);
    assert_eq!(w.canon.app("draw").unwrap().status, Status::Stuck);
}

#[tokio::test]
async fn a_rate_limited_provider_spends_no_round() {
    let w = world(&[("server.js", "")], false, vec![Err(ghost::Error::Transient("429".into()))]);
    w.start();
    let err = w.work(true, 5).await.unwrap_err();
    assert!(err.is_transient());
    let d = w.canon.draft("draw").unwrap().unwrap();
    assert_eq!(d.round, 0);
    assert_eq!(d.stage, DraftStage::Plan);
}

#[tokio::test]
async fn a_random_secret_is_kept_and_a_template_is_remade() {
    let w = world(&[], false, vec![]);
    let set = |template: &str| {
        let wanted = vec![
            Generate { name: "PW".into(), template: None },
            Generate { name: "URL".into(), template: Some(template.into()) },
            Generate { name: "BOTH".into(), template: Some("{URL}#{PW}".into()) },
        ];
        generate(&w.hush, "draw", &wanted).unwrap();
    };
    set("pg://u:{PW}@db/x");
    let pw = w.secret("PW").unwrap();
    assert_eq!(w.secret("BOTH").unwrap(), format!("pg://u:{pw}@db/x#{pw}"));
    set("pg://u:{PW}@postgres/x");
    assert_eq!(w.secret("PW").unwrap(), pw, "the password the database was made with stays");
    assert_eq!(w.secret("URL").unwrap(), format!("pg://u:{pw}@postgres/x"));
}

#[tokio::test]
async fn a_crash_mid_build_resumes_the_build_not_the_planning() {
    let w = world(&[("server.js", "")], false, vec![ghost_plan("FROM node:22\n")]);
    w.start();
    w.work(true, 5).await.unwrap();
    // Pretend the process died after accepting the plan, before the build.
    w.canon.set_draft_stage("draw", DraftStage::Build, Some("round 1: building")).unwrap();
    w.canon.enqueue(JOB, Some("draw"), &serde_json::json!({})).unwrap();
    w.work(true, 5).await.unwrap();
    assert_eq!(w.stage().0, DraftStage::Trial);
    assert_eq!(w.script.seen.lock().unwrap().len(), 1, "no new plan was asked for");
    assert_eq!(w.canon.draft("draw").unwrap().unwrap().round, 1);
}

#[tokio::test]
async fn a_draft_waiting_on_you_says_for_what() {
    let w = world(&[("lease.toml", REPO_LEASE), ("Dockerfile", "FROM scratch\n")], false, vec![]);
    w.start();
    w.work(false, 5).await.unwrap();
    w.canon.set_status("draw", Status::NeedsYou, Some("waiting for secrets: STRIPE_KEY")).unwrap();
    pass(&w.canon, 5).unwrap();
    assert_eq!(w.stage(), (DraftStage::Trial, Some("waiting for secrets: STRIPE_KEY".into())));
    let jobs: Vec<_> = (0..3).filter_map(|_| w.canon.claim().unwrap()).collect();
    assert!(jobs.iter().all(|j| j.state == JobState::Running && j.kind != JOB), "no new round while waiting");
}

#[tokio::test]
async fn a_model_that_never_proposes_spends_rounds_and_says_so() {
    let w = world(&[("README.md", "nothing to build")], false, vec![]);
    w.start();
    w.work(true, 2).await.unwrap();
    let (stage, detail) = w.stage();
    assert_eq!(stage, DraftStage::Stuck);
    assert!(detail.as_deref().unwrap().ends_with("script ran out"), "the last reason is named: {detail:?}");
}

#[test]
fn a_repo_url_suggests_a_name() {
    assert_eq!(name_for("https://github.com/me/Draw_Board.git").as_deref(), Some("draw-board"));
    assert_eq!(name_for("git@github.com:me/site/").as_deref(), Some("site"));
    assert_eq!(name_for("https://github.com/me/---"), None);
}
