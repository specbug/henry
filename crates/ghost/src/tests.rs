//! Ghost against a scripted provider: a local server that answers each call
//! with the next reply in its script and keeps every request it was sent.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use lease::Provider;
use serde_json::{Value, json};
use sound::{Ask, Survey};

use super::*;

#[derive(Default)]
struct Script {
    replies: VecDeque<(u16, Value)>,
    requests: Vec<(HeaderMap, Value)>,
}

type Shared = Arc<Mutex<Script>>;

async fn answer(State(s): State<Shared>, headers: HeaderMap, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let mut s = s.lock().unwrap();
    s.requests.push((headers, body));
    let (status, reply) = s.replies.pop_front().unwrap_or((500, json!({ "error": "script ran out" })));
    (StatusCode::from_u16(status).unwrap(), Json(reply))
}

async fn server(replies: Vec<(u16, Value)>) -> (String, Shared) {
    let shared: Shared = Arc::new(Mutex::new(Script { replies: replies.into(), requests: vec![] }));
    let app = axum::Router::new()
        .route("/chat/completions", post(answer))
        .route("/messages", post(answer))
        .with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), shared)
}

fn openai_call(id: &str, name: &str, args: Value) -> Value {
    json!({
        "choices": [{ "message": { "role": "assistant", "content": null, "tool_calls": [
            { "id": id, "type": "function", "function": { "name": name, "arguments": args.to_string() } }
        ] } }],
        "usage": { "prompt_tokens": 100, "completion_tokens": 10 },
    })
}

fn openai_text(text: &str) -> Value {
    json!({
        "choices": [{ "message": { "role": "assistant", "content": text } }],
        "usage": { "prompt_tokens": 100, "completion_tokens": 10 },
    })
}

fn claude_call(id: &str, name: &str, input: Value) -> Value {
    json!({
        "content": [{ "type": "text", "text": "Let me look." }, { "type": "tool_use", "id": id, "name": name, "input": input }],
        "stop_reason": "tool_use",
        "usage": { "input_tokens": 200, "output_tokens": 20 },
    })
}

const GOOD: &str = "name = \"draw\"\n[[service]]\nname = \"web\"\nport = 3000\nbuild = { dockerfile = \".henry/Dockerfile\" }\nhealth = { http = \"/health\" }\npublic = { hostname = \"draw.example.com\" }\nsecrets = [\"SESSION_KEY\"]\n";

fn good_plan() -> Value {
    json!({
        "lease": GOOD,
        "files": [{ "path": ".henry/Dockerfile", "content": "FROM node:22\nCOPY . .\nCMD [\"node\", \"server.js\"]\n" }],
        "generate": [{ "name": "SESSION_KEY" }],
        "notes": "an express app on 3000",
    })
}

struct Repo {
    dir: tempfile::TempDir,
    survey: Survey,
}

fn repo() -> Repo {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("package.json"), r#"{"scripts":{"start":"node server.js"}}"#).unwrap();
    std::fs::write(dir.path().join("server.js"), "app.listen(3000); process.env.SESSION_KEY").unwrap();
    std::fs::write(dir.path().join(".env"), "SESSION_KEY=hunter2-real-value\n").unwrap();
    let survey = Survey::scan(dir.path()).unwrap();
    Repo { dir, survey }
}

impl Repo {
    fn workspace(&self) -> Workspace {
        Workspace::new(self.dir.path().to_path_buf(), self.survey.files.clone())
    }
}

const ASK: Ask<'static> = Ask { app: "draw", branch: "main", hostname: Some("draw.example.com") };

async fn propose(model: &Model, repo: &Repo, attempts: &[Attempt], budget: u64) -> Outcome {
    let limits = lease::Limits::default();
    let brief = Brief { ask: &ASK, survey: &repo.survey, skeleton: None, attempts, limits: &limits };
    model.propose(&brief, &repo.workspace(), budget, &Settings::default()).await
}

#[tokio::test]
async fn an_openai_style_provider_reads_proposes_and_repairs() {
    let bad = json!({ "lease": GOOD.replace("name = \"draw\"", "name = \"drawing\"") });
    let (base, script) = server(vec![
        (200, openai_call("c1", "read_file", json!({ "path": "server.js" }))),
        (200, openai_call("c2", "propose", bad)),
        (200, openai_call("c3", "propose", good_plan())),
    ])
    .await;
    let model = Model::connect(Provider::Meta, "llama-x", hush::Secret::new("k-meta"), Some(&base)).unwrap();
    assert_eq!(model.name(), "meta/llama-x");
    let repo = repo();
    let out = propose(&model, &repo, &[], 1_000_000).await;

    let plan = out.plan.unwrap();
    assert_eq!(plan.files[0].0, ".henry/Dockerfile");
    assert_eq!(plan.generate[0].name, "SESSION_KEY");
    assert_eq!(out.usage, Usage { input: 300, output: 30 });

    let s = script.lock().unwrap();
    assert_eq!(s.requests.len(), 3);
    let (headers, first) = &s.requests[0];
    assert_eq!(headers["authorization"], "Bearer k-meta");
    assert_eq!(first["model"], "llama-x");
    assert_eq!(first["messages"][0]["role"], "system");
    assert!(first["tools"].as_array().unwrap().iter().any(|t| t["function"]["name"] == "propose"));

    let second = &s.requests[1].1["messages"];
    let tool = second.as_array().unwrap().last().unwrap();
    assert_eq!((tool["role"].as_str(), tool["tool_call_id"].as_str()), (Some("tool"), Some("c1")));
    assert!(tool["content"].as_str().unwrap().contains("app.listen(3000)"));

    let refusal =
        s.requests[2].1["messages"].as_array().unwrap().last().unwrap()["content"].as_str().unwrap().to_owned();
    assert!(refusal.contains("must be named `draw`"), "the check's answer goes straight back: {refusal}");
}

#[tokio::test]
async fn claude_speaks_its_own_wire_format() {
    let (base, script) = server(vec![
        (200, claude_call("t1", "list_files", json!({ "dir": "." }))),
        (200, claude_call("t2", "propose", good_plan())),
    ])
    .await;
    let model =
        Model::connect(Provider::Claude, "claude-sonnet-5-5", hush::Secret::new("k-claude"), Some(&base)).unwrap();
    let repo = repo();
    let out = propose(&model, &repo, &[], 1_000_000).await;
    out.plan.unwrap();
    assert_eq!(out.usage.total(), 440);

    let s = script.lock().unwrap();
    let (headers, first) = &s.requests[0];
    assert_eq!(headers["x-api-key"], "k-claude");
    assert_eq!(headers["anthropic-version"], "2023-06-01");
    assert!(first["system"].as_str().unwrap().contains("You propose; Henry decides"));
    assert!(first["tools"].as_array().unwrap().iter().any(|t| t["name"] == "propose" && t["input_schema"].is_object()));

    let messages = s.requests[1].1["messages"].as_array().unwrap();
    let assistant = &messages[1];
    assert_eq!(assistant["content"][1]["type"], "tool_use");
    let result = &messages[2]["content"][0];
    assert_eq!((result["type"].as_str(), result["tool_use_id"].as_str()), (Some("tool_result"), Some("t1")));
    let listing = result["content"].as_str().unwrap();
    assert!(listing.contains("server.js") && !listing.contains(".env"), "{listing}");
}

#[tokio::test]
async fn secret_files_stay_hidden_whatever_the_model_asks() {
    let (base, script) = server(vec![
        (200, openai_call("c1", "read_file", json!({ "path": ".env" }))),
        (200, openai_call("c2", "search", json!({ "text": "SESSION_KEY" }))),
        (200, openai_call("c3", "propose", good_plan())),
    ])
    .await;
    let model = Model::connect(Provider::OpenAi, "gpt-x", hush::Secret::new("k"), Some(&base)).unwrap();
    let repo = repo();
    propose(&model, &repo, &[], 1_000_000).await.plan.unwrap();
    let s = script.lock().unwrap();
    let all: String = s.requests.iter().map(|(_, b)| b.to_string()).collect();
    assert!(!all.contains("hunter2"), "a secret value never reaches a provider");
    assert!(all.contains("may hold secrets"));
    assert!(all.contains("server.js:1:"), "search still finds the code that reads it");
}

#[tokio::test]
async fn a_rate_limit_is_transient_and_a_bad_key_is_not() {
    let repo = repo();
    let (base, _) = server(vec![(429, json!({ "error": "slow down" }))]).await;
    let model = Model::connect(Provider::Gemini, "g", hush::Secret::new("k"), Some(&base)).unwrap();
    assert!(propose(&model, &repo, &[], 1_000_000).await.plan.unwrap_err().is_transient());

    let (base, _) = server(vec![(401, json!({ "error": "bad key" }))]).await;
    let model = Model::connect(Provider::Claude, "c", hush::Secret::new("k"), Some(&base)).unwrap();
    let err = propose(&model, &repo, &[], 1_000_000).await.plan.unwrap_err();
    assert!(matches!(err, Error::Provider(_)), "{err}");
    assert!(err.to_string().contains("401"));
}

#[tokio::test]
async fn the_spend_cap_and_the_turn_limit_both_end_the_loop() {
    let repo = repo();
    let reads = (0..5).map(|i| (200, openai_call(&format!("c{i}"), "list_files", json!({ "dir": "." })))).collect();
    let (base, _) = server(reads).await;
    let model = Model::connect(Provider::OpenAi, "m", hush::Secret::new("k"), Some(&base)).unwrap();
    let out = propose(&model, &repo, &[], 250).await;
    assert!(out.plan.unwrap_err().to_string().contains("spend cap"));
    assert_eq!(out.usage.total(), 330, "it stops on the call that crosses the cap");

    let (base, _) = server(vec![(200, openai_text("I think it is a node app.")); 3]).await;
    let model = Model::connect(Provider::OpenAi, "m", hush::Secret::new("k"), Some(&base)).unwrap();
    let err = propose(&model, &repo, &[], 1_000_000).await.plan.unwrap_err();
    assert!(err.to_string().contains("without proposing"), "{err}");
}

#[tokio::test]
async fn earlier_failures_are_part_of_the_brief() {
    let (base, script) = server(vec![(200, openai_call("c1", "propose", good_plan()))]).await;
    let model = Model::connect(Provider::Meta, "m", hush::Secret::new("k"), Some(&base)).unwrap();
    let repo = repo();
    let earlier = Attempt {
        plan: sound::Plan {
            lease: GOOD.into(),
            files: vec![(".henry/Dockerfile".into(), "FROM node:12".into())],
            ..Default::default()
        },
        failure: "`web` did not build: npm ERR! engine node>=20".into(),
    };
    propose(&model, &repo, &[earlier], 1_000_000).await.plan.unwrap();
    let s = script.lock().unwrap();
    let first = s.requests[0].1["messages"][1]["content"].as_str().unwrap().to_owned();
    assert!(first.contains("Attempt 1 failed") && first.contains("engine node>=20") && first.contains("FROM node:12"));
    assert!(first.contains("draw.example.com"));
    assert!(!first.contains("hunter2"));
}
