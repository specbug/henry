//! What Ghost may do with a checkout: list it, read it, search it. Only the
//! files Sound walked are visible, and secret-bearing files never are.

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::Tool;

const READ_LIMIT: usize = 48 * 1024;
const LIST_LIMIT: usize = 400;
const SEARCH_LIMIT: usize = 60;

/// A read-only view of one checkout.
pub struct Workspace {
    root: PathBuf,
    files: Vec<String>,
}

impl Workspace {
    /// `files` is what Sound walked; nothing outside it can be read.
    pub fn new(root: PathBuf, files: Vec<String>) -> Workspace {
        let files = files.into_iter().filter(|f| sound::readable(f)).collect();
        Workspace { root, files }
    }

    pub fn list(&self, dir: &str) -> String {
        let dir = dir.trim_matches('/').trim_start_matches("./");
        let prefix = if dir.is_empty() || dir == "." { String::new() } else { format!("{dir}/") };
        let matches: Vec<&str> = self.files.iter().filter(|f| f.starts_with(&prefix)).map(String::as_str).collect();
        if matches.is_empty() {
            return format!("no files under `{dir}`");
        }
        let mut out = matches.iter().take(LIST_LIMIT).copied().collect::<Vec<_>>().join("\n");
        if matches.len() > LIST_LIMIT {
            out.push_str(&format!("\n… and {} more", matches.len() - LIST_LIMIT));
        }
        out
    }

    pub fn read(&self, path: &str, offset: usize) -> Result<String, String> {
        let path = path.trim_start_matches("./");
        if !sound::readable(path) {
            return Err(format!("`{path}` may hold secrets; Henry never shows it"));
        }
        if !self.files.iter().any(|f| f == path) {
            return Err(format!("no file `{path}`; list_files shows what exists"));
        }
        let bytes = std::fs::read(self.root.join(path)).map_err(|e| format!("{path}: {e}"))?;
        let text = String::from_utf8_lossy(&bytes);
        let start = floor_char(&text, offset.min(text.len()));
        let end = floor_char(&text, (start + READ_LIMIT).min(text.len()));
        let mut out = text[start..end].to_owned();
        if end < text.len() {
            out.push_str(&format!("\n[… truncated; {} more bytes, read again with offset {end}]", text.len() - end));
        }
        Ok(out)
    }

    /// Lines containing `needle`, as `path:line: text`.
    pub fn search(&self, needle: &str) -> String {
        if needle.is_empty() {
            return "search needs some text".into();
        }
        let mut hits = Vec::new();
        for f in &self.files {
            let Ok(text) = std::fs::read_to_string(self.root.join(f)) else { continue };
            for (i, line) in text.lines().enumerate().filter(|(_, l)| l.contains(needle)) {
                let line: String = line.trim().chars().take(200).collect();
                hits.push(format!("{f}:{}: {line}", i + 1));
                if hits.len() >= SEARCH_LIMIT {
                    hits.push("[… more matches not shown]".into());
                    return hits.join("\n");
                }
            }
        }
        if hits.is_empty() { format!("no line contains `{needle}`") } else { hits.join("\n") }
    }
}

fn floor_char(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

pub(crate) const PROPOSE: &str = "propose";

pub(crate) fn tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "list_files",
            description: "List the repo's files under a directory ('.' for all).",
            schema: json!({
                "type": "object",
                "properties": { "dir": { "type": "string" } },
                "required": ["dir"],
            }),
        },
        Tool {
            name: "read_file",
            description: "Read a file from the repo. Long files come in pieces: pass the offset the last piece names.",
            schema: json!({
                "type": "object",
                "properties": { "path": { "type": "string" }, "offset": { "type": "integer", "minimum": 0 } },
                "required": ["path"],
            }),
        },
        Tool {
            name: "search",
            description: "Find lines in the repo containing some literal text.",
            schema: json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"],
            }),
        },
        Tool {
            name: PROPOSE,
            description: "Propose how to run the app. Henry checks it at once and answers with any problem; \
                          fix it and propose again. Once it passes, Henry builds and tries it.",
            schema: json!({
                "type": "object",
                "properties": {
                    "lease": { "type": "string", "description": "The whole lease.toml." },
                    "files": {
                        "type": "array",
                        "description": "Files Henry adds to the checkout, each under .henry/ (e.g. .henry/Dockerfile).",
                        "items": {
                            "type": "object",
                            "properties": { "path": { "type": "string" }, "content": { "type": "string" } },
                            "required": ["path", "content"],
                        },
                    },
                    "generate": {
                        "type": "array",
                        "description": "Secrets Henry generates, e.g. a database password. A template may name other generated secrets in braces.",
                        "items": {
                            "type": "object",
                            "properties": { "name": { "type": "string" }, "template": { "type": "string" } },
                            "required": ["name"],
                        },
                    },
                    "notes": { "type": "string", "description": "What you decided and why, in a few lines." },
                },
                "required": ["lease"],
            }),
        },
    ]
}

/// Run a read-only tool. `propose` is handled by the loop, not here.
pub(crate) fn run(ws: &Workspace, name: &str, input: &Value) -> Result<String, String> {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or_default();
    match name {
        "list_files" => Ok(ws.list(s("dir"))),
        "read_file" => ws.read(s("path"), input.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize),
        "search" => Ok(ws.search(s("text"))),
        other => Err(format!("there is no tool `{other}`")),
    }
}
