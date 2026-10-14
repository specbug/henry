//! The Cloudflare side of the tunnel: its ingress rules and the DNS records
//! that point hostnames at it.
//!
//! DNS is touched carefully. Henry tags every record it creates with
//! [`MANAGED`], and only ever edits or deletes records carrying that tag. A
//! record you made by hand is reported, never overwritten.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use canon::serde_json::{Value, json};

use crate::{Error, Result, Tunnel};

/// The comment on every DNS record Henry owns.
pub const MANAGED: &str = "managed by henry";

/// What [`Cloudflare::take_over`] did to a hostname's record.
#[derive(Debug, Clone, PartialEq)]
pub enum TakeOver {
    /// It already pointed at this tunnel, as Henry's.
    Already,
    /// There was none; Henry made one.
    Created,
    /// Henry rewrote this record, kept here so it can be put back.
    Replaced(Value),
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct DnsReport {
    pub created: Vec<String>,
    pub updated: Vec<String>,
    pub deleted: Vec<String>,
    /// Hosts Henry wanted but did not touch, and why.
    pub skipped: Vec<(String, String)>,
}

pub struct Cloudflare {
    /// `https://api.cloudflare.com/client/v4`, or a test server.
    pub base: String,
    pub account: String,
    pub tunnel: String,
    /// The zones Henry may manage records in, e.g. `example.com`.
    pub zones: Vec<String>,
    token: String,
    client: reqwest::Client,
    zone_ids: Mutex<BTreeMap<String, String>>,
}

impl Cloudflare {
    pub fn new(base: &str, account: &str, tunnel: &str, zones: Vec<String>, token: &str) -> Cloudflare {
        let client =
            reqwest::Client::builder().timeout(Duration::from_secs(30)).build().expect("an HTTPS client always builds");
        Cloudflare {
            base: base.trim_end_matches('/').to_owned(),
            account: account.to_owned(),
            tunnel: tunnel.to_owned(),
            zones,
            token: token.to_owned(),
            client,
            zone_ids: Mutex::new(BTreeMap::new()),
        }
    }

    /// Where a tunnel's hostnames point.
    pub fn target(&self) -> String {
        format!("{}.cfargotunnel.com", self.tunnel)
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut req = self.client.request(method, format!("{}{path}", self.base));
        if let Some(body) = body {
            req = req.json(&body);
        }
        self.send(req, path).await
    }

    async fn send(&self, req: reqwest::RequestBuilder, path: &str) -> Result<Value> {
        let res =
            req.bearer_auth(&self.token).send().await.map_err(|e| Error::Unreachable("Cloudflare", e.to_string()))?;
        let status = res.status();
        let value: Value = res.json().await.unwrap_or(Value::Null);
        if !status.is_success() || value["success"] != Value::Bool(true) {
            let errors = value["errors"].to_string();
            return Err(Error::Refused("Cloudflare", format!("{status} on {path}: {errors}")));
        }
        Ok(value)
    }

    /// Whether the token may read `path`: how Henry checks a permission
    /// without changing anything.
    pub async fn reaches(&self, path: &str) -> std::result::Result<(), String> {
        self.call(reqwest::Method::GET, &format!("/accounts/{}{path}", self.account), None)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// The configured zone a host belongs to: the longest suffix match.
    fn zone_of(&self, host: &str) -> Option<&str> {
        self.zones
            .iter()
            .filter(|z| host == z.as_str() || host.ends_with(&format!(".{z}")))
            .max_by_key(|z| z.len())
            .map(String::as_str)
    }

    async fn zone_id(&self, zone: &str) -> Result<String> {
        if let Some(id) = self.zone_ids.lock().unwrap().get(zone) {
            return Ok(id.clone());
        }
        let found = self.call(reqwest::Method::GET, &format!("/zones?name={zone}"), None).await?;
        let id = found["result"][0]["id"]
            .as_str()
            .ok_or_else(|| Error::Refused("Cloudflare", format!("no zone `{zone}` on this account")))?
            .to_owned();
        self.zone_ids.lock().unwrap().insert(zone.to_owned(), id.clone());
        Ok(id)
    }

    /// The records named `host`, of any type.
    async fn records(&self, zone_id: &str, host: &str) -> Result<Vec<Value>> {
        let res = self
            .call(reqwest::Method::GET, &format!("/zones/{zone_id}/dns_records?name={host}&per_page=100"), None)
            .await?;
        Ok(res["result"].as_array().into_iter().flatten().filter(|r| r["name"] == host).cloned().collect())
    }

    /// The record `host` has now, as `{type, content, proxied, ttl, comment}`.
    pub async fn record(&self, host: &str) -> Result<Option<Value>> {
        let zone = self
            .zone_of(host)
            .ok_or_else(|| Error::Refused("Cloudflare", format!("{host} is not in any [cloudflare] zones")))?;
        let id = self.zone_id(zone).await?;
        Ok(self.records(&id, host).await?.first().map(|r| {
            json!({ "type": r["type"], "content": r["content"], "proxied": r["proxied"], "ttl": r["ttl"], "comment": r["comment"] })
        }))
    }

    /// Whether a record from [`Cloudflare::record`] is Henry's, for this tunnel.
    pub fn is_ours(&self, record: &Value) -> bool {
        record["content"] == self.target().as_str() && record["comment"] == MANAGED
    }

    /// Point `host` at this tunnel even though Henry did not make its record:
    /// the cutover of an app from hostd, and only because you asked. The old
    /// record comes back so [`Cloudflare::give_back`] can restore it.
    pub async fn take_over(&self, host: &str) -> Result<TakeOver> {
        let zone = self
            .zone_of(host)
            .ok_or_else(|| Error::Refused("Cloudflare", format!("{host} is not in any [cloudflare] zones")))?;
        let id = self.zone_id(zone).await?;
        let target = self.target();
        let record =
            json!({ "type": "CNAME", "name": host, "content": target, "proxied": true, "ttl": 1, "comment": MANAGED });
        let existing = self.records(&id, host).await?;
        if let Some(other) = existing.iter().find(|r| r["type"] != "CNAME") {
            return Err(Error::Refused(
                "Cloudflare",
                format!(
                    "{host} has a {} record; replace it with a CNAME by hand first",
                    other["type"].as_str().unwrap_or("?")
                ),
            ));
        }
        match existing.first() {
            None => {
                self.call(reqwest::Method::POST, &format!("/zones/{id}/dns_records"), Some(record)).await?;
                Ok(TakeOver::Created)
            }
            Some(r) if r["content"] == target.as_str() && r["comment"] == MANAGED => Ok(TakeOver::Already),
            Some(r) => {
                let rid = r["id"].as_str().unwrap_or_default();
                self.call(reqwest::Method::PUT, &format!("/zones/{id}/dns_records/{rid}"), Some(record)).await?;
                let old = json!({ "type": r["type"], "content": r["content"], "proxied": r["proxied"], "ttl": r["ttl"], "comment": r["comment"] });
                Ok(TakeOver::Replaced(old))
            }
        }
    }

    /// Undo [`Cloudflare::take_over`]: put the old record back, or remove the
    /// one Henry made.
    pub async fn give_back(&self, host: &str, old: Option<&Value>) -> Result<()> {
        let zone = self
            .zone_of(host)
            .ok_or_else(|| Error::Refused("Cloudflare", format!("{host} is not in any [cloudflare] zones")))?;
        let id = self.zone_id(zone).await?;
        let ours = self.records(&id, host).await?.into_iter().find(|r| r["comment"] == MANAGED);
        let Some(rid) = ours.as_ref().and_then(|r| r["id"].as_str()) else { return Ok(()) };
        match old {
            Some(old) => {
                let record = json!({
                    "type": old["type"].as_str().unwrap_or("CNAME"),
                    "name": host,
                    "content": old["content"],
                    "proxied": old["proxied"].as_bool().unwrap_or(true),
                    "ttl": old["ttl"].as_u64().unwrap_or(1),
                    "comment": old["comment"].as_str().unwrap_or(""),
                });
                self.call(reqwest::Method::PUT, &format!("/zones/{id}/dns_records/{rid}"), Some(record)).await?;
            }
            None => {
                self.call(reqwest::Method::DELETE, &format!("/zones/{id}/dns_records/{rid}"), None).await?;
            }
        }
        Ok(())
    }

    /// Every CNAME in a zone, by name: (id, content, comment).
    async fn cnames(&self, zone_id: &str) -> Result<BTreeMap<String, (String, String, String)>> {
        let mut out = BTreeMap::new();
        let mut page = 1;
        loop {
            let res = self
                .call(
                    reqwest::Method::GET,
                    &format!("/zones/{zone_id}/dns_records?type=CNAME&per_page=100&page={page}"),
                    None,
                )
                .await?;
            for r in res["result"].as_array().into_iter().flatten() {
                let s = |k: &str| r[k].as_str().unwrap_or_default().to_owned();
                out.insert(s("name"), (s("id"), s("content"), s("comment")));
            }
            let pages = res["result_info"]["total_pages"].as_u64().unwrap_or(1);
            if page >= pages {
                return Ok(out);
            }
            page += 1;
        }
    }
}

/// The boundary between parts of a Worker upload, made from its contents so
/// it never appears inside one.
fn boundary(parts: &[&str]) -> String {
    (0..)
        .map(|i| format!("henry-worker-upload-{i}"))
        .find(|b| !parts.iter().any(|p| p.contains(b.as_str())))
        .expect("an unbounded range finds a boundary")
}

/// A Worker upload as `multipart/form-data`: its metadata, then each module.
pub fn multipart(upload: &crate::workers::Upload) -> (String, Vec<u8>) {
    let metadata = upload.metadata().to_string();
    let mut parts: Vec<&str> = vec![&metadata];
    parts.extend(upload.modules.iter().map(|(_, src)| src.as_str()));
    let b = boundary(&parts);
    let mut body = format!(
        "--{b}\r\nContent-Disposition: form-data; name=\"metadata\"\r\nContent-Type: application/json\r\n\r\n{metadata}\r\n"
    );
    for (name, src) in &upload.modules {
        body.push_str(&format!(
            "--{b}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{name}\"\r\n\
             Content-Type: application/javascript+module\r\n\r\n{src}\r\n"
        ));
    }
    body.push_str(&format!("--{b}--\r\n"));
    (format!("multipart/form-data; boundary={b}"), body.into_bytes())
}

impl crate::workers::Edge for Cloudflare {
    async fn subdomain(&self) -> Result<String> {
        let v = self.call(reqwest::Method::GET, &format!("/accounts/{}/workers/subdomain", self.account), None).await?;
        v["result"]["subdomain"].as_str().filter(|s| !s.is_empty()).map(str::to_owned).ok_or_else(|| {
            Error::Refused(
                "Cloudflare",
                "the account has no workers.dev subdomain; pick one under Workers & Pages in the dashboard".into(),
            )
        })
    }

    async fn upload(&self, script: &str, upload: &crate::workers::Upload) -> Result<()> {
        let path = format!("/accounts/{}/workers/scripts/{script}", self.account);
        let (content_type, body) = multipart(upload);
        let req = self
            .client
            .request(reqwest::Method::PUT, format!("{}{path}", self.base))
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(body);
        self.send(req, &path).await.map(|_| ())
    }

    async fn enable_subdomain(&self, script: &str) -> Result<()> {
        let path = format!("/accounts/{}/workers/scripts/{script}/subdomain", self.account);
        self.call(reqwest::Method::POST, &path, Some(json!({ "enabled": true, "previews_enabled": false })))
            .await
            .map(|_| ())
    }

    async fn delete(&self, script: &str) -> Result<()> {
        let path = format!("/accounts/{}/workers/scripts/{script}?force=true", self.account);
        match self.call(reqwest::Method::DELETE, &path, None).await {
            Ok(_) => Ok(()),
            // Already gone: 10007 is Cloudflare's "no such script".
            Err(Error::Refused(_, why)) if why.contains("10007") || why.starts_with("404") => Ok(()),
            Err(e) => Err(e),
        }
    }
}

impl Tunnel for Cloudflare {
    async fn set_ingress(&self, hosts: &[String], origin: &str) -> Result<()> {
        let mut ingress: Vec<Value> = hosts.iter().map(|h| json!({ "hostname": h, "service": origin })).collect();
        // cloudflared requires the last rule to match everything.
        ingress.push(json!({ "service": "http_status:404" }));
        let path = format!("/accounts/{}/cfd_tunnel/{}/configurations", self.account, self.tunnel);
        self.call(reqwest::Method::PUT, &path, Some(json!({ "config": { "ingress": ingress } }))).await?;
        Ok(())
    }

    async fn sync_dns(&self, hosts: &[String]) -> Result<DnsReport> {
        let mut report = DnsReport::default();
        let target = self.target();
        let mut by_zone: BTreeMap<&str, Vec<&String>> = BTreeMap::new();
        for host in hosts {
            match self.zone_of(host) {
                Some(zone) => by_zone.entry(zone).or_default().push(host),
                None => report.skipped.push((host.clone(), "not in any zone listed in [cloudflare] zones".into())),
            }
        }
        for zone in &self.zones {
            let id = self.zone_id(zone).await?;
            let existing = self.cnames(&id).await?;
            let wanted = by_zone.get(zone.as_str()).cloned().unwrap_or_default();
            for host in &wanted {
                let record = json!({ "type": "CNAME", "name": host, "content": target, "proxied": true, "ttl": 1, "comment": MANAGED });
                match existing.get(host.as_str()) {
                    None => {
                        self.call(reqwest::Method::POST, &format!("/zones/{id}/dns_records"), Some(record)).await?;
                        report.created.push((*host).clone());
                    }
                    Some((_, content, _)) if *content == target => {}
                    Some((rid, _, comment)) if comment == MANAGED => {
                        self.call(reqwest::Method::PUT, &format!("/zones/{id}/dns_records/{rid}"), Some(record))
                            .await?;
                        report.updated.push((*host).clone());
                    }
                    Some((_, content, _)) => report.skipped.push((
                        (*host).clone(),
                        format!("a record you made points it at {content}; Henry will not overwrite it"),
                    )),
                }
            }
            for (name, (rid, _, comment)) in &existing {
                if comment == MANAGED && !wanted.contains(&name) {
                    self.call(reqwest::Method::DELETE, &format!("/zones/{id}/dns_records/{rid}"), None).await?;
                    report.deleted.push(name.clone());
                }
            }
        }
        Ok(report)
    }
}
