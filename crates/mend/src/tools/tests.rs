//! Tools against a scripted GitHub: releases, assets and checksums served
//! from a local server whose "latest" version a test can move.

use std::io::Write;
use std::sync::{Arc, Mutex};

use axum::extract::{Path as UrlPath, State};
use axum::routing::get;
use sha2::Digest;

use super::*;

#[derive(Clone)]
struct Hub {
    base: String,
    /// The version `latest` names, and a corruption switch.
    version: Arc<Mutex<String>>,
    corrupt: Arc<Mutex<bool>>,
}

fn restic_bytes(version: &str) -> Vec<u8> {
    let script = format!("#!/bin/sh\necho restic {version}\n");
    let mut enc = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
    enc.write_all(script.as_bytes()).unwrap();
    enc.finish().unwrap()
}

fn asset_name(version: &str) -> String {
    (spec("restic").unwrap().asset)(version, std::env::consts::OS, std::env::consts::ARCH).unwrap()
}

async fn latest(State(h): State<Hub>) -> axum::Json<Value> {
    let v = h.version.lock().unwrap().clone();
    let asset = asset_name(&v);
    axum::Json(serde_json::json!({
        "tag_name": format!("v{v}"),
        "assets": [
            { "name": asset, "browser_download_url": format!("{}/dl/{v}/{asset}", h.base) },
            { "name": "SHA256SUMS", "browser_download_url": format!("{}/dl/{v}/SHA256SUMS", h.base) },
        ],
    }))
}

async fn download(State(h): State<Hub>, UrlPath((v, file)): UrlPath<(String, String)>) -> Vec<u8> {
    if file == "SHA256SUMS" {
        let digest = hex::encode(sha2::Sha256::digest(restic_bytes(&v)));
        let digest = if *h.corrupt.lock().unwrap() { "0".repeat(64) } else { digest };
        return format!("{digest}  {}\n{}  other_file\n", asset_name(&v), "f".repeat(64)).into_bytes();
    }
    restic_bytes(&v)
}

async fn hub(version: &str) -> Hub {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let h = Hub { base, version: Arc::new(Mutex::new(version.into())), corrupt: Arc::new(Mutex::new(false)) };
    let app = axum::Router::new()
        .route("/repos/restic/restic/releases/latest", get(latest))
        .route("/dl/{v}/{file}", get(download))
        .with_state(h.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    h
}

struct Verdict(Result<(), String>, Mutex<usize>);

impl Gate for Verdict {
    async fn check(&self, _tool: &str) -> Result<(), String> {
        *self.1.lock().unwrap() += 1;
        self.0.clone()
    }
}

fn runs(path: &Path) -> String {
    let out = std::process::Command::new(path).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[tokio::test]
async fn railpack_uses_the_published_checksum_asset_and_refuses_corruption() {
    let version = "0.43.0";
    let spec = spec("railpack").unwrap();
    let asset = (spec.asset)(version, std::env::consts::OS, std::env::consts::ARCH).unwrap();
    let mut bytes = Vec::new();
    {
        let gz = flate2::write::GzEncoder::new(&mut bytes, flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        let script = b"#!/bin/sh\necho railpack 0.43.0\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(script.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, "railpack", &script[..]).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
    }
    for corrupt in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let release = serde_json::json!({
            "tag_name": format!("v{version}"),
            "assets": [
                {"name": asset, "browser_download_url": format!("{base}/asset")},
                {"name": "checksums.txt", "browser_download_url": format!("{base}/checksums")}
            ]
        });
        let digest = if corrupt { "0".repeat(64) } else { hex::encode(sha2::Sha256::digest(&bytes)) };
        let sums = format!("{digest}  {asset}\n");
        let artifact = bytes.clone();
        let app = axum::Router::new()
            .route("/repos/railwayapp/railpack/releases/latest", get(move || async { axum::Json(release) }))
            .route("/checksums", get(move || async { sums }))
            .route("/asset", get(move || async { artifact }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().join("tools"), &base, None).unwrap();
        let result = tools.adopt(&Canon::open_in_memory().unwrap(), "railpack").await;
        if corrupt {
            assert!(result.is_err(), "a checksum mismatch must never install a tool");
            assert!(Tools::current(&tools.root, "railpack").is_none());
        } else {
            assert_eq!(result.unwrap(), version);
            assert_eq!(runs(&Tools::current(&tools.root, "railpack").unwrap()), "railpack 0.43.0");
        }
        server.abort();
    }
}

#[tokio::test]
async fn a_tool_is_adopted_upgraded_through_its_gate_and_rolled_back_when_it_fails() {
    let h = hub("0.18.1").await;
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().join("tools"), &h.base, None).unwrap();
    let canon = Canon::open_in_memory().unwrap();

    assert_eq!(tools.adopt(&canon, "restic").await.unwrap(), "0.18.1");
    let current = Tools::current(&tools.root, "restic").unwrap();
    assert_eq!(runs(&current), "restic 0.18.1");

    let pass = Verdict(Ok(()), Mutex::new(0));
    assert_eq!(tools.upgrade(&canon, "restic", &pass).await.unwrap(), Upgrade::Current("0.18.1".into()));
    assert_eq!(*pass.1.lock().unwrap(), 0, "nothing new, nothing checked");

    *h.version.lock().unwrap() = "0.19.0".into();
    let up = tools.upgrade(&canon, "restic", &pass).await.unwrap();
    assert_eq!(up, Upgrade::Upgraded { from: "0.18.1".into(), to: "0.19.0".into() });
    assert_eq!(runs(&current), "restic 0.19.0", "the same path now runs the new version");
    let t = canon.tool("restic").unwrap().unwrap();
    assert_eq!((t.current.as_str(), t.previous.as_deref(), t.pending), ("0.19.0", Some("0.18.1"), None));

    *h.version.lock().unwrap() = "0.20.0".into();
    let fail = Verdict(Err("restic cat config: unknown flag".into()), Mutex::new(0));
    let up = tools.upgrade(&canon, "restic", &fail).await.unwrap();
    assert!(matches!(&up, Upgrade::RolledBack { from, to, .. } if from == "0.19.0" && to == "0.20.0"), "{up:?}");
    assert_eq!(runs(&current), "restic 0.19.0", "the old version is back");
    assert_eq!(*fail.1.lock().unwrap(), 2, "checked, then checked again after the rollback");
    assert_eq!(canon.tool("restic").unwrap().unwrap().current, "0.19.0");
    assert!(canon.events(None, 5).unwrap().iter().any(|e| e.kind == "mend.rollback"));

    let mut kept: Vec<String> = std::fs::read_dir(tools.root.join("restic"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.') && n != "current")
        .collect();
    kept.sort();
    assert_eq!(kept, ["0.18.1", "0.19.0"], "the current and the previous stay; the failed one goes");
}

#[tokio::test]
async fn a_download_that_does_not_match_its_checksum_is_refused() {
    let h = hub("0.18.1").await;
    *h.corrupt.lock().unwrap() = true;
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().join("tools"), &h.base, None).unwrap();
    let err = tools.adopt(&Canon::open_in_memory().unwrap(), "restic").await.unwrap_err().to_string();
    assert!(err.contains("does not match its published checksum"), "{err}");
    assert!(Tools::current(&tools.root, "restic").is_none());
}

#[tokio::test]
async fn a_switch_cut_off_by_a_crash_is_undone() {
    let h = hub("0.18.1").await;
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().join("tools"), &h.base, None).unwrap();
    let canon = Canon::open_in_memory().unwrap();
    tools.adopt(&canon, "restic").await.unwrap();
    *h.version.lock().unwrap() = "0.19.0".into();
    let r = tools.latest(spec("restic").unwrap()).await.unwrap();
    tools.install(spec("restic").unwrap(), &r).await.unwrap();
    // As upgrade does, then the process dies before the gate answers.
    canon.set_tool("restic", "0.18.1", None, Some("0.19.0")).unwrap();
    tools.switch("restic", "0.19.0").unwrap();

    assert_eq!(tools.recover(&canon).unwrap(), ["restic"]);
    assert_eq!(runs(&Tools::current(&tools.root, "restic").unwrap()), "restic 0.18.1");
    assert_eq!(canon.tool("restic").unwrap().unwrap().pending, None);
}

#[test]
fn checksums_are_found_in_lists_and_notes_and_archives_unpack() {
    let notes = "## Checksums\ncloudflared-darwin-arm64.tgz: 4f1a7c0e7f7f6b0a8d0f1b2c3d4e5f60718293a4b5c6d7e8f90123456789abcd\n";
    assert_eq!(
        digest_for(notes, "cloudflared-darwin-arm64.tgz", 64).as_deref(),
        Some("4f1a7c0e7f7f6b0a8d0f1b2c3d4e5f60718293a4b5c6d7e8f90123456789abcd")
    );
    assert_eq!(digest_for(notes, "cloudflared-linux-amd64", 64), None);

    let mut tarball = Vec::new();
    {
        let gz = flate2::write::GzEncoder::new(&mut tarball, flate2::Compression::default());
        let mut b = tar::Builder::new(gz);
        for (name, body) in [("LICENSE", "MIT"), ("caddy", "the binary")] {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            b.append_data(&mut header, name, body.as_bytes()).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap();
    }
    assert_eq!(unpack("caddy_2.10.2_mac_arm64.tar.gz", &tarball, "caddy").unwrap(), b"the binary");
    assert!(unpack("caddy_2.10.2_mac_arm64.tar.gz", &tarball, "restic").is_err());
    assert_eq!(caddy("2.10.2", "macos", "aarch64").as_deref(), Some("caddy_2.10.2_mac_arm64.tar.gz"));
    assert_eq!(railpack("0.40.1", "macos", "aarch64").as_deref(), Some("railpack-v0.40.1-arm64-apple-darwin.tar.gz"));
    assert_eq!(restic("0.18.1", "macos", "aarch64").as_deref(), Some("restic_0.18.1_darwin_arm64.bz2"));
}

#[tokio::test]
async fn a_release_whose_checksums_are_plain_checksums_txt_is_still_verified() {
    // Railpack's releases list `checksums.txt`, not `railpack_<v>_checksums.txt`.
    let asset = (spec("railpack").unwrap().asset)("0.30.0", std::env::consts::OS, std::env::consts::ARCH).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let digest = "a".repeat(64);
    let release = serde_json::json!({
        "tag_name": "v0.30.0",
        "assets": [
            { "name": asset, "browser_download_url": format!("{base}/dl/{asset}") },
            { "name": "checksums.txt", "browser_download_url": format!("{base}/dl/checksums.txt") },
        ],
    });
    let sums = format!("{digest}  {asset}\n{}  railpack-v0.30.0-x86_64-pc-windows-msvc.zip\n", "b".repeat(64));
    let app = axum::Router::new()
        .route("/repos/railwayapp/railpack/releases/latest", get(move || async move { axum::Json(release) }))
        .route("/dl/checksums.txt", get(move || async move { sums }));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().to_path_buf(), &base, None).unwrap();
    let r = tools.latest(spec("railpack").unwrap()).await.unwrap();
    assert_eq!((r.version.as_str(), r.digest.as_str(), r.sha512), ("0.30.0", digest.as_str(), false));
}
