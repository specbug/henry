use lease::{Build, Health, Lease};

use super::*;

fn repo(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (path, text) in files {
        let p = dir.path().join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    dir
}

fn scan(dir: &tempfile::TempDir) -> Survey {
    Survey::scan(dir.path()).unwrap()
}

fn ask() -> Ask<'static> {
    Ask { app: "draw", branch: "main", hostname: Some("draw.example.com") }
}

/// A skeleton must always pass the same check as any plan.
fn lease_of(s: &Skeleton) -> Lease {
    checked(s, &ask())
}

fn checked(s: &Skeleton, ask: &Ask<'_>) -> Lease {
    s.plan.check(ask, &lease::Limits::default()).unwrap_or_else(|e| panic!("{e}\n---\n{}", s.plan.lease))
}

#[test]
fn the_repos_own_lease_wins_untouched() {
    let r = repo(&[("lease.toml", "name = \"draw\"\n"), ("Dockerfile", "FROM scratch\nEXPOSE 80\n")]);
    let s = scan(&r).skeleton(&ask()).unwrap();
    assert_eq!(s.source, SkeletonSource::Repo);
    assert_eq!(s.plan.lease, "name = \"draw\"\n");
    assert!(s.gaps.is_empty());
}

#[test]
fn a_dockerfile_gives_the_port_it_exposes() {
    let r = repo(&[(
        "Dockerfile",
        "FROM node:22 AS build\nFROM --platform=linux/amd64 node:22-slim\nEXPOSE 3000/tcp\nCMD [\"node\", \"server.js\"]\n",
    )]);
    let survey = scan(&r);
    assert_eq!(survey.dockerfiles[0].from, ["node:22", "node:22-slim"]);
    let s = survey.skeleton(&ask()).unwrap();
    let lease = lease_of(&s);
    let web = lease.service("web").unwrap();
    assert_eq!(web.port, Some(3000));
    assert_eq!(web.build, Build::Dockerfile { path: "Dockerfile".into(), context: ".".into() });
    assert_eq!(web.health, Health::Http { path: "/".into() }, "a public service must answer HTTP");
    assert_eq!(web.public.as_ref().unwrap().hostname, "draw.example.com");
    assert!(s.gaps.iter().any(|g| g.contains("GET /")), "{:?}", s.gaps);
}

#[test]
fn a_repo_with_only_a_manifest_is_built_by_railpack() {
    let r = repo(&[(
        "package.json",
        r#"{"scripts": {"start": "node index.js"}, "dependencies": {"express": "^5", "pg": "^8"}}"#,
    )]);
    let survey = scan(&r);
    assert_eq!(survey.stacks[0].kind, StackKind::Node);
    assert_eq!(survey.stacks[0].framework.as_deref(), Some("express"));
    assert_eq!(survey.stacks[0].start.as_deref(), Some("node index.js"));
    assert!(survey.needs.contains(&Need::Postgres), "pg means Postgres");

    let s = survey.skeleton(&ask()).unwrap();
    let lease = lease_of(&s);
    let web = lease.service("web").unwrap();
    assert_eq!(web.build, Build::Railpack { path: ".".into() });
    assert_eq!(web.port, Some(8080));
    assert_eq!(web.env.get("PORT").map(String::as_str), Some("8080"));
    assert!(web.secrets.contains(&"DATABASE_URL".to_owned()));
    assert_eq!(web.depends_on, ["db"]);

    let db = lease.service("db").unwrap();
    assert_eq!(db.health, Health::Postgres);
    assert_eq!(db.secrets, ["POSTGRES_PASSWORD"]);
    assert_eq!(db.volumes.len(), 1, "a database keeps its data");
    assert_eq!(
        s.plan.generate,
        [
            Generate { name: "POSTGRES_PASSWORD".into(), template: None },
            Generate {
                name: "DATABASE_URL".into(),
                template: Some("postgres://app:{POSTGRES_PASSWORD}@db:5432/app".into())
            },
        ]
    );
}

#[test]
fn compose_services_become_lease_services() {
    let compose = r#"
services:
  api:
    build: ./api
    ports: ["8000:8000"]
    environment:
      LOG_LEVEL: info
      SECRET_KEY: ${SECRET_KEY}
      DATABASE_URL: postgres://u:p@postgres:5432/x
    depends_on: [postgres, worker]
    volumes:
      - ./src:/app/src
  worker:
    build: ./api
    command: celery -A app worker
  postgres:
    image: postgres:16
    environment:
      - POSTGRES_USER=u
      - POSTGRES_PASSWORD=p
      - POSTGRES_DB=x
    volumes:
      - pg_data:/var/lib/postgresql/data
volumes:
  pg_data: {}
"#;
    let r = repo(&[("docker-compose.yml", compose), ("api/Dockerfile", "FROM python:3.13\n")]);
    let s = scan(&r).skeleton(&ask()).unwrap();
    let lease = lease_of(&s);
    assert_eq!(lease.services.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["api", "postgres"]);

    let api = lease.service("api").unwrap();
    assert_eq!(api.build, Build::Dockerfile { path: "api/Dockerfile".into(), context: "api".into() });
    assert_eq!(api.port, Some(8000));
    assert_eq!(api.env.get("LOG_LEVEL").map(String::as_str), Some("info"));
    assert!(api.secrets.contains(&"SECRET_KEY".to_owned()));
    assert!(api.secrets.contains(&"DATABASE_URL".to_owned()), "the dev URL is replaced by a generated one");
    assert_eq!(api.depends_on, ["postgres"], "the dropped worker is not a dependency");
    assert!(api.public.is_some());

    let pg = lease.service("postgres").unwrap();
    assert_eq!(pg.health, Health::Postgres);
    assert!(!pg.env.contains_key("POSTGRES_PASSWORD"), "the dev password is not carried over");
    assert_eq!(pg.secrets, ["POSTGRES_PASSWORD"]);
    assert_eq!(pg.volumes[0].name, "pg-data");
    let url = s.plan.generate.iter().find(|g| g.name == "DATABASE_URL").unwrap();
    assert_eq!(url.template.as_deref(), Some("postgres://u:{POSTGRES_PASSWORD}@postgres:5432/x"));

    assert!(s.gaps.iter().any(|g| g.contains("`worker` has no port")), "{:?}", s.gaps);
    assert!(s.gaps.iter().any(|g| g.contains("bind-mounts")), "{:?}", s.gaps);
}

#[test]
fn env_names_are_found_in_code_and_values_kept_only_when_harmless() {
    let r = repo(&[
        (
            ".env.example",
            "# config\nSTRIPE_SECRET_KEY=sk_test_123\nLOG_LEVEL=debug\nexport SITE_NAME=\"Draw\"\nPORT=3000\n",
        ),
        (
            "src/a.ts",
            "const k = process.env.STRIPE_SECRET_KEY; const u = process.env['UPLOAD_DIR']; process.env.NODE_ENV",
        ),
        ("app/b.py", "os.environ['SENTRY_DSN']\nos.getenv(\"FEATURE_FLAG\")\nos.environ.get('X_Y')"),
        ("main.go", "os.Getenv(\"GO_THING\")"),
        ("src/main.rs", "std::env::var(\"RUST_THING\")"),
        ("node_modules/x/index.js", "process.env.IGNORED"),
    ]);
    let s = scan(&r);
    let names: Vec<&str> = s.env.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        [
            "FEATURE_FLAG",
            "GO_THING",
            "LOG_LEVEL",
            "RUST_THING",
            "SENTRY_DSN",
            "SITE_NAME",
            "STRIPE_SECRET_KEY",
            "UPLOAD_DIR",
            "X_Y"
        ]
    );
    let stripe = &s.env["STRIPE_SECRET_KEY"];
    assert!(stripe.secret && stripe.documented);
    assert_eq!(stripe.example, None, "a secret's example value is never kept");
    assert_eq!(s.env["SITE_NAME"].example.as_deref(), Some("Draw"));
    assert!(s.env["SENTRY_DSN"].secret);
    assert_eq!(s.env["UPLOAD_DIR"].read_in, ["src/a.ts"]);
    assert!(!s.brief().contains("sk_test_123"), "the brief never carries a secret's value");
}

#[test]
fn documented_config_lands_in_the_lease_and_the_rest_is_named_as_a_gap() {
    let r = repo(&[
        ("Dockerfile", "FROM scratch\nEXPOSE 80\n"),
        (".env.example", "SESSION_SECRET=\nLOG_LEVEL=info\n"),
        ("server.js", "process.env.SESSION_SECRET; process.env.MYSTERY"),
    ]);
    let s = scan(&r).skeleton(&ask()).unwrap();
    let web = lease_of(&s).services.remove(0);
    assert_eq!(web.secrets, ["SESSION_SECRET"]);
    assert_eq!(web.env.get("LOG_LEVEL").map(String::as_str), Some("info"));
    assert!(s.gaps.iter().any(|g| g.contains("MYSTERY")), "{:?}", s.gaps);
}

#[test]
fn redis_is_added_when_the_code_wants_it() {
    let r = repo(&[("requirements.txt", "flask\nredis==5\n"), ("app.py", "os.environ['REDIS_URL']")]);
    let survey = scan(&r);
    assert_eq!(survey.stacks[0].framework.as_deref(), Some("flask"));
    let quiet = Ask { hostname: None, ..ask() };
    let s = survey.skeleton(&quiet).unwrap();
    let lease = checked(&s, &quiet);
    assert_eq!(lease.service("cache").unwrap().health, Health::Redis);
    assert!(lease.service("web").unwrap().secrets.contains(&"REDIS_URL".to_owned()));
    assert!(lease.service("web").unwrap().public.is_none(), "no hostname, no route");
}

#[test]
fn ci_and_procfile_commands_become_hints() {
    let ci = "jobs:\n  test:\n    steps:\n      - uses: actions/checkout@v4\n      - run: |\n          npm ci\n          npm test\n";
    let r = repo(&[(".github/workflows/ci.yml", ci), ("Procfile", "web: gunicorn app:app\n")]);
    let s = scan(&r);
    assert_eq!(
        s.hints,
        ["Procfile: web: gunicorn app:app", ".github/workflows/ci.yml: npm ci", ".github/workflows/ci.yml: npm test"]
    );
}

#[test]
fn a_repo_with_nothing_to_build_has_no_skeleton() {
    let r = repo(&[("README.md", "# hi"), ("notes.txt", "")]);
    assert!(scan(&r).skeleton(&ask()).is_none());
}

#[test]
fn a_cargo_workspace_root_is_not_an_app() {
    let r = repo(&[
        ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
        ("crates/web/Cargo.toml", "[package]\nname = \"web\"\n[dependencies]\naxum = \"0.8\"\n"),
    ]);
    let s = scan(&r);
    assert_eq!(s.stacks.len(), 1);
    assert_eq!(s.stacks[0].dir, "crates/web");
    assert_eq!(s.stacks[0].framework.as_deref(), Some("axum"));
}

#[test]
fn secret_files_are_never_readable() {
    for path in [".env", "config/.env.production", "deploy/key.pem", ".npmrc", "home/id_ed25519", "x/credentials.json"]
    {
        assert!(!readable(path), "{path}");
    }
    for path in [".env.example", "src/main.rs", "Dockerfile", "config/.env.sample", "keys.md"] {
        assert!(readable(path), "{path}");
    }
}

#[test]
fn the_walk_skips_dependencies_and_stops_at_its_limit() {
    let r = repo(&[("a.txt", ""), ("node_modules/b/package.json", "{}"), (".git/config", "")]);
    let s = scan(&r);
    assert_eq!(s.files, ["a.txt"]);
    assert!(s.stacks.is_empty());
    assert!(!s.truncated);
}

#[test]
fn a_plan_must_fit_the_ask_and_keep_its_files_to_itself() {
    let plan = |lease: &str, files: Vec<(&str, &str)>, generate: Vec<Generate>| Plan {
        lease: lease.into(),
        files: files.into_iter().map(|(p, c)| (p.to_owned(), c.to_owned())).collect(),
        generate,
        notes: String::new(),
    };
    let good = "name = \"draw\"\n[[service]]\nname = \"web\"\nport = 80\nbuild = { dockerfile = \".henry/Dockerfile\" }\nhealth = { http = \"/\" }\npublic = { hostname = \"draw.example.com\" }\nsecrets = [\"A\", \"B\"]\n";
    let secret = |n: &str, t: Option<&str>| Generate { name: n.into(), template: t.map(str::to_owned) };
    let limits = lease::Limits::default();
    plan(good, vec![(".henry/Dockerfile", "FROM scratch")], vec![secret("A", None), secret("B", Some("x{A}"))])
        .check(&ask(), &limits)
        .unwrap();

    let refused = |p: Plan| p.check(&ask(), &limits).unwrap_err();
    assert!(refused(plan(good, vec![], vec![])).contains("does not include"));
    assert!(
        refused(plan(good, vec![(".henry/Dockerfile", ""), ("Dockerfile", "")], vec![]))
            .contains("only be added under")
    );
    assert!(refused(plan(good, vec![(".henry/../x", "")], vec![])).contains("only be added under"));
    assert!(
        refused(plan(&good.replace("draw\"\n[", "other\"\n["), vec![(".henry/Dockerfile", "")], vec![]))
            .contains("must be named `draw`")
    );
    assert!(
        refused(plan(&good.replace("draw.example.com", "x.example.com"), vec![(".henry/Dockerfile", "")], vec![]))
            .contains("public at")
    );
    assert!(
        refused(plan(good, vec![(".henry/Dockerfile", "")], vec![secret("C", None)])).contains("no service lists it")
    );
    assert!(refused(plan(good, vec![(".henry/Dockerfile", "")], vec![secret("A", Some("{Z}"))])).contains("{Z}"));
    assert!(refused(plan("name = 1", vec![], vec![])).contains("refused"));
}
