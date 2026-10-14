//! A Cloudflare Worker as wrangler describes it: `wrangler.toml` (or
//! `wrangler.json`), and the code beside it. Henry deploys what it can
//! upload as it is: plain JavaScript modules, R2 buckets, plain `vars`, and
//! secrets. Anything else wrangler does for a Worker is named, not guessed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

/// What a Worker's config says, as far as Henry deploys it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    /// Its directory, relative to the repo root.
    pub dir: String,
    /// `name`, which wrangler deploys it as.
    pub name: String,
    /// `main`, relative to `dir`.
    pub main: String,
    pub compatibility_date: String,
    pub compatibility_flags: Vec<String>,
    /// R2 binding names.
    pub buckets: Vec<String>,
    /// Plain `vars`, committed to the repo, so never secret.
    pub vars: BTreeMap<String, String>,
    /// What it uses that Henry does not deploy, e.g. `kv_namespaces`.
    pub unsupported: Vec<String>,
}

/// Config keys that bind or trigger something Henry does not set up.
const UNSUPPORTED: &[&str] = &[
    "kv_namespaces",
    "d1_databases",
    "durable_objects",
    "queues",
    "services",
    "ai",
    "vectorize",
    "hyperdrive",
    "analytics_engine_datasets",
    "dispatch_namespaces",
    "browser",
    "workflows",
    "send_email",
    "routes",
    "route",
    "triggers",
    "build",
    "site",
    "assets",
];

/// Whether a file name is a Worker's config.
pub fn is_config(name: &str) -> bool {
    ["wrangler.toml", "wrangler.json", "wrangler.jsonc"].contains(&name)
}

/// Read a Worker's config. `dir` is where it sits, relative to the repo root.
pub fn read(path: &Path, dir: &str) -> Option<Config> {
    let text = std::fs::read_to_string(path).ok()?;
    let doc: serde_json::Value = if path.extension().is_some_and(|e| e == "toml") {
        serde_json::to_value(text.parse::<toml::Table>().ok()?).ok()?
    } else {
        serde_json::from_str(&strip_comments(&text)).ok()?
    };
    let s = |k: &str| doc.get(k).and_then(|v| v.as_str()).map(str::to_owned);
    let buckets = doc
        .get("r2_buckets")
        .and_then(|b| b.as_array())
        .into_iter()
        .flatten()
        .filter_map(|b| b.get("binding").and_then(|v| v.as_str()).map(str::to_owned))
        .collect();
    let vars = doc
        .get("vars")
        .and_then(|v| v.as_object())
        .into_iter()
        .flatten()
        .map(|(k, v)| (k.clone(), v.as_str().map_or_else(|| v.to_string(), str::to_owned)))
        .collect();
    let mut unsupported: Vec<String> =
        UNSUPPORTED.iter().filter(|k| doc.get(**k).is_some()).map(|k| (*k).to_owned()).collect();
    if doc.get("env").is_some() {
        unsupported.push("env (per-environment config)".into());
    }
    Some(Config {
        dir: dir.to_owned(),
        name: s("name").unwrap_or_default(),
        main: s("main").unwrap_or_default().trim_start_matches("./").to_owned(),
        compatibility_date: s("compatibility_date").unwrap_or_default(),
        compatibility_flags: doc
            .get("compatibility_flags")
            .and_then(|f| f.as_array())
            .into_iter()
            .flatten()
            .filter_map(|f| f.as_str().map(str::to_owned))
            .collect(),
        buckets,
        vars,
        unsupported,
    })
}

/// `wrangler.jsonc` allows `//` comments; JSON does not.
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if c == '\\' {
                out.extend(chars.next());
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
            out.push(c);
        } else if c == '/' && chars.peek() == Some(&'/') {
            for c in chars.by_ref() {
                if c == '\n' {
                    out.push('\n');
                    break;
                }
            }
        } else if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut last = ' ';
            for c in chars.by_ref() {
                if last == '*' && c == '/' {
                    break;
                }
                last = c;
            }
        } else {
            out.push(c);
        }
    }
    out
}

static ENV_USE: LazyLock<Regex> = LazyLock::new(|| {
    // `env` on its own, not `process.env`.
    Regex::new(r#"(?:^|[^.\w$])env\s*(?:\.\s*([A-Z][A-Z0-9_]{0,63})\b|\[\s*['"]([A-Z][A-Z0-9_]{0,63})['"]\s*\])"#)
        .expect("the env pattern compiles")
});

/// The names a Worker's code reads from `env` that its config does not bind:
/// its secrets.
pub fn secrets(config: &Config, sources: &[String]) -> Vec<String> {
    let bound: BTreeSet<&str> = config.buckets.iter().chain(config.vars.keys()).map(String::as_str).collect();
    let mut out = BTreeSet::new();
    for text in sources {
        for c in ENV_USE.captures_iter(text) {
            if let Some(m) = c.get(1).or_else(|| c.get(2))
                && !bound.contains(m.as_str())
            {
                out.insert(m.as_str().to_owned());
            }
        }
    }
    out.into_iter().collect()
}

static IMPORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)(?:^|[;\s])(?:import|export)\b[^'"`;]*?\bfrom\s*['"]([^'"]+)['"]|(?:^|[;\s])import\s*\(?\s*['"]([^'"]+)['"]"#)
        .expect("the import pattern compiles")
});

/// What a module imports that is not a file beside it or part of the
/// runtime: a package, which only a bundler could include.
pub fn packages(source: &str) -> Vec<String> {
    let mut out = BTreeSet::new();
    for c in IMPORT.captures_iter(source) {
        let Some(m) = c.get(1).or_else(|| c.get(2)) else { continue };
        let spec = m.as_str();
        let runtime = spec.starts_with("cloudflare:") || spec.starts_with("node:");
        if !spec.starts_with("./") && !spec.starts_with("../") && !runtime {
            out.insert(spec.to_owned());
        }
    }
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str, name: &str) -> Config {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        read(&path, "worker").unwrap()
    }

    #[test]
    fn a_wrangler_toml_gives_what_henry_deploys_and_names_the_rest() {
        let c = config(
            "name = \"cairn-sync\"\nmain = \"./worker.js\"\ncompatibility_date = \"2026-05-01\"\n\
             [[r2_buckets]]\nbinding = \"BUCKET\"\nbucket_name = \"cairn-state\"\n\
             [vars]\nMAX = 30\nMODE = \"prod\"\n[[kv_namespaces]]\nbinding = \"KV\"\nid = \"x\"\n",
            "wrangler.toml",
        );
        assert_eq!(
            (c.name.as_str(), c.main.as_str(), c.compatibility_date.as_str()),
            ("cairn-sync", "worker.js", "2026-05-01")
        );
        assert_eq!(c.buckets, ["BUCKET"]);
        assert_eq!(c.vars["MAX"], "30");
        assert_eq!(c.unsupported, ["kv_namespaces"]);
    }

    #[test]
    fn a_wrangler_jsonc_may_carry_comments() {
        let c = config(
            "{\n  // the sync proxy\n  \"name\": \"s\", /* main */ \"main\": \"src/index.js\",\n  \"vars\": { \"URL\": \"https://x//y\" }\n}\n",
            "wrangler.jsonc",
        );
        assert_eq!((c.name.as_str(), c.main.as_str()), ("s", "src/index.js"));
        assert_eq!(c.vars["URL"], "https://x//y", "a // inside a string is not a comment");
    }

    #[test]
    fn its_secrets_are_what_the_code_reads_and_the_config_does_not_bind() {
        let c =
            Config { buckets: vec!["BUCKET".into()], vars: [("MODE".into(), "x".into())].into(), ..Config::default() };
        let code = "const t = env.SYNC_TOKEN; env.BUCKET.get('k'); env['OTHER_KEY']; env.MODE; process.env.NOT_THIS"
            .to_owned();
        assert_eq!(secrets(&c, &[code]), ["OTHER_KEY", "SYNC_TOKEN"]);
    }

    #[test]
    fn only_packages_need_a_bundler() {
        let src = "import worker from './worker.js';\nimport { connect } from 'cloudflare:sockets';\n\
                   import { Buffer } from 'node:buffer';\nimport { Hono } from 'hono';\nexport * from \"../shared/x.js\";\n\
                   const m = await import('lodash');\n";
        assert_eq!(packages(src), ["hono", "lodash"]);
    }
}
