use super::*;

const FULL: &str = r#"
name = "draw"
branch = "master"

[ai]
model = "claude/claude-sonnet-5-5"

[[service]]
name = "api"
port = 8100
secrets = ["SESSION_KEY"]
env = { LOG_LEVEL = "info" }
depends_on = ["db"]
build = { dockerfile = "apps/api/Dockerfile", context = "." }
health = { http = "/health" }
public = { hostname = "draw.example.com" }
resources = { memory = "1GiB", cpus = 2.0 }

[[service.volume]]
name = "scenes"
mount = "/data"
backup = "sqlite:app.db"

[[service]]
name = "db"
port = 5432
build = { image = "postgres:17" }
health = { postgres = true }
secrets = ["POSTGRES_PASSWORD"]

[[service.volume]]
name = "pg"
mount = "/var/lib/postgresql/data"
backup = "postgres"
"#;

fn codes(text: &str) -> Vec<&'static str> {
    match Lease::parse(text) {
        Err(Refusal::Policy(v)) => v.into_iter().map(|v| v.code).collect(),
        Err(Refusal::Syntax(e)) => panic!("expected a policy refusal, got a syntax error: {e}"),
        Ok(_) => panic!("expected a refusal, got a lease"),
    }
}

/// A minimal one-service lease with `extra` spliced into the service.
fn one(extra: &str) -> String {
    format!("name = \"app\"\n[[service]]\nname = \"web\"\n{extra}\n")
}

#[test]
fn parses_a_full_lease() {
    let c = Lease::parse(FULL).unwrap();
    assert_eq!(c.name, "draw");
    assert_eq!(c.branch, "master");
    assert!(c.auto_deploy);
    assert_eq!(c.ai, Some(AiPrefs { provider: Provider::Claude, model: "claude-sonnet-5-5".into() }));

    let api = c.service("api").unwrap();
    assert_eq!(api.build, Build::Dockerfile { path: "apps/api/Dockerfile".into(), context: ".".into() });
    assert_eq!(api.health, Health::Http { path: "/health".into() });
    assert_eq!(api.public, Some(Public { hostname: "draw.example.com".into(), path: "/".into() }));
    assert_eq!(api.resources, Resources { memory_bytes: 1 << 30, cpus: 2.0 });
    assert_eq!(api.volumes[0].backup, Some(Backup::Sqlite { path: "app.db".into() }));

    let db = c.service("db").unwrap();
    assert_eq!(db.build, Build::Image("postgres:17".into()));
    assert_eq!(db.resources, Resources { memory_bytes: 512 << 20, cpus: 1.0 });

    let order: Vec<&str> = c.start_order().iter().map(|s| s.name.as_str()).collect();
    assert_eq!(order, ["db", "api"]);
    assert_eq!(c.hostnames().collect::<Vec<_>>(), ["draw.example.com"]);
}

#[test]
fn defaults_fill_in() {
    let c = Lease::parse(&one("port = 80\nbuild = { railpack = \".\" }\nhealth = { tcp = true }")).unwrap();
    assert_eq!(c.branch, "main");
    assert_eq!(c.ai, None);
    assert_eq!(c.services[0].build, Build::Railpack { path: ".".into() });
}

#[test]
fn unknown_fields_are_refused_not_ignored() {
    let text = one("port = 80\nbuild = { image = \"x\" }\nhealth = { tcp = true }\nprivileged = true");
    assert!(matches!(Lease::parse(&text), Err(Refusal::Syntax(_))));

    let text = one("port = 80\nbuild = { image = \"x\" }\nhealth = { tcp = true }\nnetwork_mode = \"host\"");
    assert!(matches!(Lease::parse(&text), Err(Refusal::Syntax(_))));
}

#[test]
fn every_service_owes_a_health_check() {
    assert_eq!(codes(&one("port = 80\nbuild = { image = \"x\" }")), ["health_missing"]);
    assert_eq!(codes(&one("build = { image = \"x\" }\nhealth = { tcp = true }")), ["health_needs_port"]);
    assert_eq!(codes(&one("port = 80\nbuild = { image = \"x\" }\nhealth = { tcp = false }")), ["health_false"]);
    assert_eq!(
        codes(&one("port = 80\nbuild = { image = \"x\" }\nhealth = { tcp = true, redis = true }")),
        ["health_ambiguous"]
    );
    assert_eq!(
        codes(&one("port = 80\nbuild = { image = \"x\" }\nhealth = { http = \"health\" }")),
        ["health_path_invalid"]
    );
}

#[test]
fn build_takes_exactly_one_source() {
    let h = "port = 80\nhealth = { tcp = true }\n";
    assert_eq!(codes(&one(&format!("{h}build = {{}}"))), ["build_ambiguous"]);
    assert_eq!(codes(&one(&format!("{h}build = {{ image = \"x\", railpack = \".\" }}"))), ["build_ambiguous"]);
    assert_eq!(codes(&one(&format!("{h}build = {{ image = \"x\", context = \".\" }}"))), ["build_context_unused"]);
    assert_eq!(codes(&one(&format!("{h}build = {{ dockerfile = \"../Dockerfile\" }}"))), ["path_outside_repo"]);
    assert_eq!(codes(&one(&format!("{h}build = {{ dockerfile = \"/etc/passwd\" }}"))), ["path_outside_repo"]);
    assert_eq!(codes(&one(&format!("{h}build = {{ image = \"two words\" }}"))), ["image_invalid"]);
}

#[test]
fn public_services_must_answer_http() {
    let text =
        one("port = 80\nbuild = { image = \"x\" }\nhealth = { tcp = true }\npublic = { hostname = \"a.example.com\" }");
    assert_eq!(codes(&text), ["public_needs_http_health"]);

    let text =
        one("port = 80\nbuild = { image = \"x\" }\nhealth = { http = \"/\" }\npublic = { hostname = \"Example.com\" }");
    assert_eq!(codes(&text), ["hostname_invalid"]);

    let text =
        one("port = 80\nbuild = { image = \"x\" }\nhealth = { http = \"/\" }\npublic = { hostname = \"localhost\" }");
    assert_eq!(codes(&text), ["hostname_invalid"]);
}

#[test]
fn two_services_cannot_share_a_route() {
    let text = r#"
name = "app"
[[service]]
name = "a"
port = 80
build = { image = "x" }
health = { http = "/" }
public = { hostname = "app.example.com" }
[[service]]
name = "b"
port = 80
build = { image = "y" }
health = { http = "/" }
public = { hostname = "app.example.com", path = "/" }
"#;
    assert_eq!(codes(text), ["route_duplicate"]);
}

#[test]
fn volumes_stay_off_system_paths() {
    let base = "port = 80\nbuild = { image = \"x\" }\nhealth = { tcp = true }\n";
    let vol = |m: &str| one(&format!("{base}[[service.volume]]\nname = \"v\"\nmount = \"{m}\""));
    assert_eq!(codes(&vol("/")), ["mount_invalid"]);
    assert_eq!(codes(&vol("data")), ["mount_invalid"]);
    assert_eq!(codes(&vol("/data/../etc")), ["mount_invalid"]);
    assert_eq!(codes(&vol("/proc")), ["mount_forbidden"]);
    assert_eq!(codes(&vol("/dev/shm")), ["mount_forbidden"]);
    assert!(Lease::parse(&vol("/procfs-data")).is_ok());
}

#[test]
fn engine_backups_need_the_engine() {
    let text = one(
        "port = 80\nbuild = { image = \"x\" }\nhealth = { tcp = true }\n[[service.volume]]\nname = \"v\"\nmount = \"/v\"\nbackup = \"postgres\"",
    );
    assert_eq!(codes(&text), ["backup_engine_mismatch"]);

    let text = one(
        "port = 80\nbuild = { image = \"x\" }\nhealth = { tcp = true }\n[[service.volume]]\nname = \"v\"\nmount = \"/v\"\nbackup = \"rsync\"",
    );
    assert_eq!(codes(&text), ["backup_unknown"]);
}

#[test]
fn secrets_never_appear_as_plain_values() {
    let text = one(
        "port = 80\nbuild = { image = \"x\" }\nhealth = { tcp = true }\nenv = { TOKEN = \"hunter2\" }\nsecrets = [\"TOKEN\"]",
    );
    assert_eq!(codes(&text), ["secret_in_env"]);

    let text = one("port = 80\nbuild = { image = \"x\" }\nhealth = { tcp = true }\nenv = { HENRY_URL = \"x\" }");
    assert_eq!(codes(&text), ["var_name_reserved"]);

    let text = one("port = 80\nbuild = { image = \"x\" }\nhealth = { tcp = true }\nsecrets = [\"9LIVES\"]");
    assert_eq!(codes(&text), ["var_name_invalid"]);
}

#[test]
fn resources_stay_within_limits() {
    let base = "port = 80\nbuild = { image = \"x\" }\nhealth = { tcp = true }\n";
    assert_eq!(codes(&one(&format!("{base}resources = {{ memory = \"64GiB\" }}"))), ["memory_over_limit"]);
    assert_eq!(codes(&one(&format!("{base}resources = {{ memory = \"1MiB\" }}"))), ["memory_too_small"]);
    assert_eq!(codes(&one(&format!("{base}resources = {{ memory = \"lots\" }}"))), ["memory_invalid"]);
    assert_eq!(codes(&one(&format!("{base}resources = {{ cpus = 0.0 }}"))), ["cpus_out_of_range"]);
    assert_eq!(codes(&one(&format!("{base}resources = {{ cpus = 64.0 }}"))), ["cpus_out_of_range"]);
}

#[test]
fn dependencies_must_resolve_without_loops() {
    let svc = |name: &str, dep: &str| {
        format!(
            "[[service]]\nname = \"{name}\"\nport = 80\nbuild = {{ image = \"x\" }}\nhealth = {{ tcp = true }}\ndepends_on = [\"{dep}\"]\n"
        )
    };
    let text = format!("name = \"app\"\n{}{}", svc("a", "b"), svc("b", "a"));
    assert_eq!(codes(&text), ["depends_on_cycle"]);

    let text = format!("name = \"app\"\n{}", svc("a", "ghost"));
    assert_eq!(codes(&text), ["depends_on_unknown"]);

    let text = format!("name = \"app\"\n{}", svc("a", "a"));
    assert_eq!(codes(&text), ["depends_on_self"]);
}

#[test]
fn names_are_dns_safe() {
    for bad in ["", "App", "1app", "app-", "a_b", "this-name-is-far-too-long-for-a-label"] {
        let text = format!(
            "name = \"{bad}\"\n[[service]]\nname = \"web\"\nport = 80\nbuild = {{ image = \"x\" }}\nhealth = {{ tcp = true }}"
        );
        assert_eq!(codes(&text), ["name_invalid"], "{bad:?} should be refused");
    }
}

#[test]
fn henry_is_a_reserved_name() {
    let text =
        "name = \"henry\"\n[[service]]\nname = \"web\"\nport = 80\nbuild = { image = \"x\" }\nhealth = { tcp = true }";
    assert_eq!(codes(text), ["name_reserved"]);
}

#[test]
fn ai_model_names_a_known_provider() {
    let base = one("port = 80\nbuild = { image = \"x\" }\nhealth = { tcp = true }");
    let text = base.replacen("name = \"app\"", "name = \"app\"\n[ai]\nmodel = \"llama\"", 1);
    assert_eq!(codes(&text), ["ai_model_invalid"]);
    let text = base.replacen("name = \"app\"", "name = \"app\"\n[ai]\nmodel = \"mistral/large\"", 1);
    assert_eq!(codes(&text), ["ai_provider_unknown"]);
    let text = base.replacen("name = \"app\"", "name = \"app\"\n[ai]\nmodel = \"meta/muse-spark\"", 1);
    assert_eq!(Lease::parse(&text).unwrap().ai.unwrap().provider, Provider::Meta);
}

#[test]
fn all_violations_are_reported_at_once() {
    let text = "name = \"Bad\"\nbranch = \"..\"\n[[service]]\nname = \"web\"\nbuild = { image = \"x\" }";
    let got = codes(text);
    assert!(got.contains(&"name_invalid"));
    assert!(got.contains(&"branch_invalid"));
    assert!(got.contains(&"health_missing"));
}

const SITE: &str = r#"
name = "cairn"
[[service]]
name = "web"
port = 8080
build = { image = "x" }
health = { http = "/" }
secrets = ["CAIRN_SYNC_TOKEN", "CAIRN_SYNC_URL"]
"#;

#[test]
fn a_worker_is_deployed_under_henrys_name_with_its_bindings() {
    let text = format!(
        "{SITE}\n[[worker]]\nname = \"sync\"\ndir = \"worker/\"\nbuckets = [\"BUCKET\"]\n\
         secrets = {{ SYNC_TOKEN = \"CAIRN_SYNC_TOKEN\" }}\nurl = \"CAIRN_SYNC_URL\"\n"
    );
    let lease = Lease::parse(&text).unwrap();
    let w = &lease.workers[0];
    assert_eq!((w.script("cairn"), w.dir.as_str()), ("henry-cairn-sync".to_owned(), "worker"));
    assert_eq!(w.secrets["SYNC_TOKEN"], "CAIRN_SYNC_TOKEN");
    assert_eq!(w.url.as_deref(), Some("CAIRN_SYNC_URL"));
}

#[test]
fn a_worker_that_would_misbehave_is_refused() {
    let worker = |body: &str| format!("{SITE}\n[[worker]]\nname = \"sync\"\ndir = \"worker\"\n{body}\n");
    assert!(codes(&format!("{SITE}\n[[worker]]\nname = \"sync\"\ndir = \"../up\"\n")).contains(&"path_outside_repo"));
    assert!(codes(&worker("buckets = [\"B\", \"B\"]")).contains(&"worker_binding_duplicate"));
    assert!(codes(&worker("buckets = [\"T\"]\nsecrets = { T = \"X\" }")).contains(&"worker_binding_duplicate"));
    assert!(codes(&worker("secrets = { HENRY_X = \"Y\" }")).contains(&"var_name_reserved"));
    assert!(codes(&worker("secrets = { T = \"U\" }\nurl = \"U\"")).contains(&"worker_url_conflict"));
    assert!(
        matches!(Lease::parse(&worker("privileged = true")), Err(Refusal::Syntax(_))),
        "unknown fields are refused"
    );
    let long = format!(
        "{}\n[[worker]]\nname = \"{}\"\ndir = \"w\"\n",
        SITE.replace("\"cairn\"", &format!("\"{}\"", "c".repeat(32))),
        "a".repeat(32)
    );
    assert!(codes(&long).contains(&"worker_name_too_long"));
    let twice = format!("{}[[worker]]\nname = \"sync\"\ndir = \"w\"\n", worker(""));
    assert!(codes(&twice).contains(&"worker_duplicate"));
}
