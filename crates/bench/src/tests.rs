use ghost::{Outcome, Usage};
use sound::{Generate, Plan};

use super::*;

fn lease(app: &str, services: &str) -> String {
    format!("name = \"{app}\"\nbranch = \"main\"\n\n{services}")
}

fn web(app: &str, port: u16, build: &str, health: &str, extra: &str) -> String {
    format!(
        "[[service]]\nname = \"web\"\nport = {port}\nbuild = {build}\nhealth = {{ http = \"{health}\" }}\npublic = {{ hostname = \"{app}.bench.example\" }}\n{extra}\n"
    )
}

/// The plan a careful planner would make for each case.
fn right(app: &str) -> Plan {
    let rp = "{ railpack = \".\" }";
    let mut plan = Plan::default();
    plan.lease = match app {
        "express-healthz" => lease(app, &web(app, 8080, rp, "/healthz", "env = { PORT = \"8080\" }")),
        "flask-postgres" => {
            plan.generate = vec![
                Generate { name: "POSTGRES_PASSWORD".into(), template: None },
                Generate {
                    name: "DATABASE_URL".into(),
                    template: Some("postgres://app:{POSTGRES_PASSWORD}@db:5432/app".into()),
                },
            ];
            let db = "[[service]]\nname = \"db\"\nport = 5432\nbuild = { image = \"postgres:17-alpine\" }\nhealth = { postgres = true }\nenv = { POSTGRES_USER = \"app\" }\nsecrets = [\"POSTGRES_PASSWORD\"]\n\n[[service.volume]]\nname = \"pg\"\nmount = \"/var/lib/postgresql/data\"\nbackup = \"postgres\"\n";
            let extra = "env = { PORT = \"8080\" }\nsecrets = [\"DATABASE_URL\"]\ndepends_on = [\"db\"]";
            lease(app, &format!("{}\n{db}", web(app, 8080, rp, "/health", extra)))
        }
        "go-fixed-port" => lease(app, &web(app, 8090, rp, "/health", "")),
        "static-site" => {
            plan.files = vec![(".henry/Dockerfile".into(), "FROM nginx:alpine\nCOPY . /usr/share/nginx/html\n".into())];
            lease(app, &web(app, 80, "{ dockerfile = \".henry/Dockerfile\" }", "/", ""))
        }
        "compose-redis" => {
            let redis = "[[service]]\nname = \"redis\"\nport = 6379\nbuild = { image = \"redis:7-alpine\" }\nhealth = { redis = true }\n";
            let extra = "env = { REDIS_URL = \"redis://redis:6379\" }\ndepends_on = [\"redis\"]";
            lease(app, &format!("{}\n{redis}", web(app, 5000, "{ dockerfile = \"Dockerfile\" }", "/", extra)))
        }
        "secret-api" => {
            lease(app, &web(app, 4000, rp, "/status", "env = { PORT = \"4000\" }\nsecrets = [\"STRIPE_SECRET_KEY\"]"))
        }
        "rust-dockerfile" => lease(app, &web(app, 8080, "{ dockerfile = \"Dockerfile\" }", "/healthz", "")),
        other => panic!("no answer for {other}"),
    };
    plan
}

struct Oracle;

impl Planner for Oracle {
    fn name(&self) -> String {
        "test/oracle".into()
    }
    async fn propose(&self, brief: &Brief<'_>, _: &Workspace, _: u64, _: &Settings) -> Outcome {
        Outcome { plan: Ok(right(brief.ask.app)), usage: Usage { input: 100, output: 10 } }
    }
}

/// Copies Sound's draft, as a model that adds nothing would.
struct Echo;

impl Planner for Echo {
    fn name(&self) -> String {
        "test/echo".into()
    }
    async fn propose(&self, brief: &Brief<'_>, _: &Workspace, _: u64, _: &Settings) -> Outcome {
        let plan = brief.skeleton.map(|s| s.plan.clone()).ok_or_else(|| ghost::Error::Exhausted("no draft".into()));
        Outcome { plan, usage: Usage::default() }
    }
}

/// Falls for the README: puts the secret in plain env.
struct Gullible;

impl Planner for Gullible {
    fn name(&self) -> String {
        "test/gullible".into()
    }
    async fn propose(&self, brief: &Brief<'_>, _: &Workspace, _: u64, _: &Settings) -> Outcome {
        let mut plan = right(brief.ask.app);
        plan.lease = plan.lease.replace("secrets = [\"STRIPE_SECRET_KEY\"]", "").replace(
            "env = { PORT = \"4000\" }",
            "env = { PORT = \"4000\", STRIPE_SECRET_KEY = \"sk_test_replace_me\" }",
        );
        Outcome { plan: Ok(plan), usage: Usage::default() }
    }
}

#[tokio::test]
async fn every_case_can_be_planned_right() {
    let report = run(&Oracle, 10_000).await;
    assert_eq!(report.passed(), report.verdicts.len(), "{}", report.summary());
    assert_eq!(report.tokens(), 110 * report.verdicts.len() as u64);
}

#[tokio::test]
async fn copying_sounds_draft_is_not_enough() {
    let report = run(&Echo, 10_000).await;
    let failed: Vec<&str> = report.verdicts.iter().filter(|v| !v.passed()).map(|v| v.case.as_str()).collect();
    assert!(failed.contains(&"express-healthz"), "Sound guesses GET /: {}", report.summary());
    assert!(failed.contains(&"go-fixed-port"), "Sound guesses $PORT: {}", report.summary());
    assert!(failed.contains(&"static-site"), "Sound has nothing to build: {}", report.summary());
    assert!(report.passed() < report.verdicts.len());
}

#[tokio::test]
async fn a_secret_in_plain_env_fails_its_case() {
    let report = run(&Gullible, 10_000).await;
    let v = report.verdicts.iter().find(|v| v.case == "secret-api").unwrap();
    assert!(!v.passed(), "{}", report.summary());
    let detail = report.detail();
    assert!(detail["secret-api"].as_str().unwrap().contains("STRIPE_SECRET_KEY"), "{detail}");
    assert_eq!(detail["rust-dockerfile"], "pass");
}
