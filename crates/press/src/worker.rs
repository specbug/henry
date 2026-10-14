//! A Worker's code, taken from the commit it is released with. Henry uploads
//! plain JavaScript modules as they are, so a Worker that needs a bundler, or
//! bindings Henry does not set up, fails its release with the reason.

use std::path::Path;

use canon::serde_json;
use sha2::{Digest, Sha256};

/// Upload limits, well inside Cloudflare's.
const MAX_MODULES: usize = 100;
const MAX_BYTES: usize = 5 << 20;
/// Wrangler requires a date; a Worker written without one gets this.
const DEFAULT_COMPATIBILITY_DATE: &str = "2025-01-01";

/// The bundle for one Worker and its hash, or why it cannot be deployed.
pub fn bundle(checkout: &Path, worker: &lease::Worker) -> Result<(String, serde_json::Value), String> {
    let dir = checkout.join(&worker.dir);
    let config_path = ["wrangler.toml", "wrangler.json", "wrangler.jsonc"]
        .iter()
        .map(|f| dir.join(f))
        .find(|p| p.is_file())
        .ok_or_else(|| format!("Worker `{}`: no wrangler.toml in {}", worker.name, worker.dir))?;
    let config = sound::wrangler::read(&config_path, &worker.dir)
        .ok_or_else(|| format!("Worker `{}`: {} does not parse", worker.name, config_path.display()))?;
    if !config.unsupported.is_empty() {
        return Err(format!(
            "Worker `{}` uses {}, which Henry does not set up",
            worker.name,
            config.unsupported.join(", ")
        ));
    }
    if let Some(b) = worker.buckets.iter().find(|b| !config.buckets.contains(b)) {
        return Err(format!("Worker `{}`: the lease binds `{b}`, which wrangler.toml does not", worker.name));
    }

    let mut modules = serde_json::Map::new();
    let mut bytes = 0;
    for path in walk(&dir) {
        let name = path.strip_prefix(&dir).expect("walked from dir").to_string_lossy().replace('\\', "/");
        if !sound::is_worker_module(&name) {
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(|e| format!("Worker `{}`: {name}: {e}", worker.name))?;
        let packages = sound::wrangler::packages(&text);
        if !packages.is_empty() {
            return Err(format!(
                "Worker `{}`: {name} imports {}; Henry uploads plain modules, so bundle it into one file first",
                worker.name,
                packages.join(", ")
            ));
        }
        bytes += text.len();
        modules.insert(name, text.into());
    }
    if !modules.contains_key(&config.main) {
        return Err(format!("Worker `{}`: its main `{}` is not a JavaScript module here", worker.name, config.main));
    }
    if modules.len() > MAX_MODULES || bytes > MAX_BYTES {
        return Err(format!(
            "Worker `{}` is {} module(s), {bytes} bytes: over Henry's {MAX_MODULES} modules or {MAX_BYTES} bytes",
            worker.name,
            modules.len()
        ));
    }
    let date = if config.compatibility_date.is_empty() {
        DEFAULT_COMPATIBILITY_DATE.to_owned()
    } else {
        config.compatibility_date
    };
    let bundle = serde_json::json!({
        "main": config.main,
        "compatibility_date": date,
        "compatibility_flags": config.compatibility_flags,
        "vars": config.vars,
        "modules": modules,
    });
    let hash = hex::encode(Sha256::digest(bundle.to_string().as_bytes()));
    Ok((hash, bundle))
}

/// Every file under `dir`, in name order, skipping dependencies and dotfiles.
fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name == "node_modules" {
                continue;
            }
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => out.push(e.path()),
                _ => {}
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker(dir: &str) -> lease::Worker {
        lease::Worker {
            name: "sync".into(),
            dir: dir.into(),
            buckets: vec!["BUCKET".into()],
            secrets: Default::default(),
            url: None,
        }
    }

    fn checkout(files: &[(&str, &str)]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for (p, t) in files {
            let p = d.path().join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, t).unwrap();
        }
        d
    }

    const TOML: &str = "name = \"x\"\nmain = \"src/index.js\"\ncompatibility_date = \"2026-05-01\"\n\
                        [[r2_buckets]]\nbinding = \"BUCKET\"\nbucket_name = \"old\"\n[vars]\nMODE = \"prod\"\n";

    #[test]
    fn a_worker_is_bundled_from_its_modules_alone() {
        let c = checkout(&[
            ("w/wrangler.toml", TOML),
            (
                "w/src/index.js",
                "import { k } from './lib/keys.js';\nexport default { fetch() { return new Response(k) } }\n",
            ),
            ("w/src/lib/keys.js", "export const k = 'state.json';\n"),
            ("w/README.md", "notes\n"),
            ("w/node_modules/x/index.js", "junk\n"),
            ("w/.wrangler/tmp.js", "junk\n"),
        ]);
        let (hash, b) = bundle(c.path(), &worker("w")).unwrap();
        let names: Vec<&String> = b["modules"].as_object().unwrap().keys().collect();
        assert_eq!(names, ["src/index.js", "src/lib/keys.js"]);
        assert_eq!((b["main"].as_str(), b["vars"]["MODE"].as_str()), (Some("src/index.js"), Some("prod")));
        assert_eq!(bundle(c.path(), &worker("w")).unwrap().0, hash, "the same code is the same bundle");
    }

    #[test]
    fn what_henry_cannot_upload_as_is_fails_with_the_reason() {
        let pkg = checkout(&[("w/wrangler.toml", TOML), ("w/src/index.js", "import { Hono } from 'hono';\n")]);
        assert!(bundle(pkg.path(), &worker("w")).unwrap_err().contains("imports hono"));
        let kv = checkout(&[
            ("w/wrangler.toml", &format!("{TOML}[[kv_namespaces]]\nbinding = \"KV\"\nid = \"1\"\n")),
            ("w/src/index.js", "export default {}\n"),
        ]);
        assert!(bundle(kv.path(), &worker("w")).unwrap_err().contains("kv_namespaces"));
        let unbound = checkout(&[("w/wrangler.toml", "main = \"src/index.js\"\n"), ("w/src/index.js", "")]);
        assert!(bundle(unbound.path(), &worker("w")).unwrap_err().contains("binds `BUCKET`"));
        assert!(bundle(checkout(&[]).path(), &worker("w")).unwrap_err().contains("no wrangler.toml"));
    }
}
