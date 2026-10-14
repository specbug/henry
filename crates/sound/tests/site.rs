//! A static site, planned by Sound, built and served by a real engine: only
//! what Vercel would publish is served, and the build sees the app's env.
//! Runs only with HENRY_ENGINE_TESTS=1 and `docker` (or `podman`) on PATH.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use sound::{Ask, Survey};

const FILES: &[(&str, &str)] = &[
    ("index.html", "<!doctype html><script src=\"static/config.js\"></script>\n"),
    ("static/app.js", "console.log('app')\n"),
    ("vercel.json", r#"{"buildCommand": "node scripts/gen-config.js", "outputDirectory": ".", "framework": null}"#),
    (".vercelignore", "notes.md\nworker/\nPrivate Notes (1).md\n"),
    (
        "scripts/gen-config.js",
        "const fs = require('fs');\nconst token = process.env.SITE_SYNC_TOKEN || '';\nfs.writeFileSync('static/config.js', 'window.CONFIG = ' + JSON.stringify({ token }) + ';\\n');\n",
    ),
    ("notes.md", "private\n"),
    ("Private Notes (1).md", "private\n"),
    (".env", "SITE_SYNC_TOKEN=from-the-repo\n"),
    ("worker/wrangler.toml", "name = \"sync\"\n"),
    ("worker/worker.js", "export default {}\n"),
];

fn run(program: &str, args: &[&str], dir: &Path) -> String {
    let out = Command::new(program).args(args).current_dir(dir).output().unwrap();
    assert!(out.status.success(), "{program} {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `(status, body)` of a plain HTTP/1.0 GET.
fn get(port: u16, path: &str) -> Option<(u16, String)> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(s, "GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").ok()?;
    let mut text = String::new();
    s.read_to_string(&mut text).ok()?;
    let status = text.split_whitespace().nth(1)?.parse().ok()?;
    Some((status, text.split_once("\r\n\r\n").map_or("", |(_, b)| b).to_owned()))
}

/// A site built from `files` as Press would build it, running with `env`.
struct Served {
    program: &'static str,
    name: String,
    tag: String,
    port: u16,
}

impl Served {
    fn start(files: &[(&str, &str)], env: &[&str]) -> Option<Served> {
        if std::env::var("HENRY_ENGINE_TESTS").ok().as_deref() != Some("1") {
            eprintln!("skipped: set HENRY_ENGINE_TESTS=1 to build with a real engine");
            return None;
        }
        let program = if Command::new("podman").arg("--version").output().is_ok_and(|o| o.status.success()) {
            "podman"
        } else {
            "docker"
        };
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        for (path, text) in files {
            let p = repo.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        run("git", &["init", "-q"], repo);

        let survey = Survey::scan(repo).unwrap();
        let ask = Ask { app: "site", branch: "main", hostname: Some("site.example.com") };
        let plan = survey.skeleton(&ask).unwrap().plan;
        plan.check(&ask, &lease::Limits::default()).unwrap();
        // As Press does: overlays into the checkout, then build from its root.
        for (path, text) in &plan.files {
            let p = repo.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = format!("{}-{n}", std::process::id());
        let (tag, name) = (format!("henry/site-test:{id}"), format!("henry-site-test-{id}"));
        run(program, &["build", "-q", "-f", ".henry/site/Dockerfile", "-t", &tag, "."], repo);
        let mut args = vec!["run", "-d", "--name", &name, "-p", "127.0.0.1::8080"];
        for e in env {
            args.extend(["-e", e]);
        }
        args.push(&tag);
        run(program, &args, repo);
        let mapped = run(program, &["port", &name, "8080"], repo);
        let port = mapped.lines().next().and_then(|l| l.rsplit(':').next()).and_then(|p| p.trim().parse().ok());
        let served = Served { program, name, tag, port: port.unwrap_or(0) };
        let deadline = Instant::now() + Duration::from_secs(60);
        while served.status("/") != Some(200) {
            if Instant::now() > deadline {
                let logs = Command::new(program).args(["logs", &served.name]).output().unwrap();
                panic!("the site never answered: {}", String::from_utf8_lossy(&logs.stderr));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        Some(served)
    }

    fn status(&self, path: &str) -> Option<u16> {
        get(self.port, path).map(|(s, _)| s)
    }

    /// The paths answering otherwise than `served` (200) and `hidden` (404).
    fn wrong(&self, served: &[&str], hidden: &[&str]) -> Vec<String> {
        let mut wrong = Vec::new();
        for (paths, want) in [(served, 200), (hidden, 404)] {
            for p in paths {
                if self.status(p) != Some(want) {
                    wrong.push(format!("{p}: {:?}", self.status(p)));
                }
            }
        }
        wrong
    }

    /// Whether `script` succeeds in a fresh container of the image.
    fn image_holds(&self, script: &str) -> bool {
        Command::new(self.program)
            .args(["run", "--rm", "--entrypoint", "sh", &self.tag, "-c", script])
            .status()
            .unwrap()
            .success()
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = Command::new(self.program).args(["rm", "-f", &self.name]).output();
        let _ = Command::new(self.program).args(["rmi", "-f", &self.tag]).output();
    }
}

static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[test]
fn a_static_site_publishes_only_what_vercel_would() {
    let Some(site) = Served::start(FILES, &["SITE_SYNC_TOKEN=sealed-value"]) else { return };
    let wrong = site.wrong(
        &["/", "/index.html", "/static/app.js", "/scripts/gen-config.js"],
        &[
            "/notes.md",
            "/Private%20Notes%20(1).md",
            "/worker/worker.js",
            "/.env",
            "/.git/HEAD",
            "/.vercelignore",
            "/.henry/site/site.sh",
            "/vercel.json",
        ],
    );
    assert!(wrong.is_empty(), "served what it should not, or not what it should: {wrong:?}");
    let config = get(site.port, "/static/config.js").map(|(_, b)| b).unwrap_or_default();
    assert!(config.contains(r#"{"token":"sealed-value"}"#), "the build saw the app's env: {config}");
    assert!(
        site.image_holds("! grep -rq sealed-value /src && ! test -e /src/.git"),
        "the image holds no secret, no .git"
    );
}

#[test]
fn a_site_with_no_build_is_published_when_the_image_is_made() {
    let files = [("index.html", "<h1>home</h1>\n"), ("about/index.html", "<h1>about</h1>\n"), (".env", "X=1\n")];
    let Some(site) = Served::start(&files, &[]) else { return };
    let wrong = site.wrong(&["/", "/about/"], &["/.env", "/.git/HEAD", "/.henry/site/Dockerfile"]);
    assert!(wrong.is_empty(), "{wrong:?}");
    assert!(get(site.port, "/about/").is_some_and(|(_, b)| b.contains("about")));
    assert!(site.image_holds("test -d /srv && ! test -e /src"), "the image holds the published files alone");
}
