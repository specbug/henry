//! Cloudflare Access, checked at the origin.
//!
//! Access stands in front of Glass's hostname and lets in only the people
//! its policy names. It tells the origin who they are with a signed token in
//! `Cf-Access-Jwt-Assertion`. Glass does not take Access's word for it on
//! trust: every request through the edge must carry a token signed by the
//! team's current keys, for this application, unexpired. A tunnel that
//! routes Glass without an Access application in front gets nothing in.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use canon::serde_json::Value;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;

/// The header Cloudflare Access sets on every request it lets through.
pub const HEADER: &str = "cf-access-jwt-assertion";

/// How long fetched keys are trusted before they are fetched again.
const KEYS_FRESH: Duration = Duration::from_secs(3600);
/// The least time between fetches, so junk tokens cannot make Glass hammer
/// Cloudflare.
const REFETCH_GAP: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum Refused {
    #[error("no Access token: sign in through Cloudflare Access")]
    Missing,
    #[error("the Access token is not one: {0}")]
    Malformed(String),
    #[error("the Access token is signed by a key the team does not have")]
    UnknownKey,
    #[error("the Access token does not hold: {0}")]
    Invalid(String),
    #[error("cannot fetch the team's signing keys: {0}")]
    Keys(String),
}

/// Who Access let in.
#[derive(Debug, Clone, PartialEq)]
pub struct Identity {
    pub email: Option<String>,
    pub subject: String,
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
    #[serde(default)]
    email: Option<String>,
}

struct Keys {
    by_kid: HashMap<String, DecodingKey>,
    fetched: Option<Instant>,
}

pub struct Access {
    issuer: String,
    aud: String,
    certs: String,
    client: reqwest::Client,
    keys: RwLock<Keys>,
}

impl Access {
    /// `team` is the Zero Trust team name (`acme`) or its domain
    /// (`acme.cloudflareaccess.com`); `aud` is the application's AUD tag.
    pub fn new(team: &str, aud: &str) -> Access {
        let domain = team.trim_start_matches("https://").trim_end_matches('/');
        let domain = if domain.contains('.') { domain.to_owned() } else { format!("{domain}.cloudflareaccess.com") };
        let issuer = format!("https://{domain}");
        Access::with_certs(&issuer, aud, &format!("{issuer}/cdn-cgi/access/certs"))
    }

    /// Fetch keys from `certs` instead of the team's own URL.
    pub fn with_certs(issuer: &str, aud: &str, certs: &str) -> Access {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("a plain HTTPS client always builds");
        Access {
            issuer: issuer.to_owned(),
            aud: aud.to_owned(),
            certs: certs.to_owned(),
            client,
            keys: RwLock::new(Keys { by_kid: HashMap::new(), fetched: None }),
        }
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Check a token from [`HEADER`]. Fetches the team's keys when they are
    /// stale or the token names one not seen yet.
    pub async fn verify(&self, token: Option<&str>) -> Result<Identity, Refused> {
        let token = token.filter(|t| !t.is_empty()).ok_or(Refused::Missing)?;
        let header = jsonwebtoken::decode_header(token).map_err(|e| Refused::Malformed(e.to_string()))?;
        if header.alg != Algorithm::RS256 {
            return Err(Refused::Invalid(format!("signed with {:?}, not RS256", header.alg)));
        }
        let kid = header.kid.ok_or_else(|| Refused::Malformed("it names no key".into()))?;
        let (known, stale, may_fetch) = {
            let keys = self.keys.read().expect("the key cache lock is never poisoned");
            let age = keys.fetched.map(|f| f.elapsed());
            (keys.by_kid.contains_key(&kid), age.is_none_or(|a| a > KEYS_FRESH), age.is_none_or(|a| a > REFETCH_GAP))
        };
        if stale || (!known && may_fetch) {
            match self.fetch().await {
                Ok(jwks) => self.set_keys(&jwks),
                // Keys fetched an hour ago still verify; with none, nothing does.
                Err(e) if !known => return Err(e),
                Err(_) => {}
            }
        }
        let keys = self.keys.read().expect("the key cache lock is never poisoned");
        let key = keys.by_kid.get(&kid).ok_or(Refused::UnknownKey)?;
        let mut v = Validation::new(Algorithm::RS256);
        v.set_audience(&[&self.aud]);
        v.set_issuer(&[&self.issuer]);
        v.set_required_spec_claims(&["exp", "aud", "iss", "sub"]);
        v.leeway = 60;
        let data = jsonwebtoken::decode::<Claims>(token, key, &v).map_err(|e| Refused::Invalid(e.to_string()))?;
        Ok(Identity { email: data.claims.email, subject: data.claims.sub })
    }

    async fn fetch(&self) -> Result<Value, Refused> {
        let resp = self.client.get(&self.certs).send().await.map_err(|e| Refused::Keys(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(Refused::Keys(format!("{} answered {}", self.certs, resp.status())));
        }
        resp.json().await.map_err(|e| Refused::Keys(e.to_string()))
    }

    /// Replace the known keys with a JWKS document (`{"keys": [...]}`).
    /// Keys that are not RSA signing keys are skipped.
    pub fn set_keys(&self, jwks: &Value) {
        let mut by_kid = HashMap::new();
        for k in jwks["keys"].as_array().into_iter().flatten() {
            let (Some(kid), Some(n), Some(e)) = (k["kid"].as_str(), k["n"].as_str(), k["e"].as_str()) else {
                continue;
            };
            if k["kty"].as_str() != Some("RSA") {
                continue;
            }
            if let Ok(key) = DecodingKey::from_rsa_components(n, e) {
                by_kid.insert(kid.to_owned(), key);
            }
        }
        let mut keys = self.keys.write().expect("the key cache lock is never poisoned");
        *keys = Keys { by_kid, fetched: Some(Instant::now()) };
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use canon::serde_json::json;
    use jsonwebtoken::{EncodingKey, Header};

    use super::*;

    pub(crate) const ISSUER: &str = "https://acme.cloudflareaccess.com";
    pub(crate) const AUD: &str = "4714c1358e65fe4b408ad6d432a5f878f08194bdb4752441fd56faefa9b2b6f2";

    pub(crate) fn now() -> i64 {
        canon::now_ms() / 1000
    }

    /// Signs with a key made for these tests alone; its public half is `jwks`.
    pub(crate) fn sign(kid: &str, claims: Value) -> String {
        let key = EncodingKey::from_rsa_der(include_bytes!("testdata/access-key.der"));
        jsonwebtoken::encode(&Header { kid: Some(kid.into()), ..Header::new(Algorithm::RS256) }, &claims, &key).unwrap()
    }

    pub(crate) fn jwks(kid: &str) -> Value {
        json!({ "keys": [{ "kid": kid, "kty": "RSA", "alg": "RS256", "e": "AQAB", "n": include_str!("testdata/access-key.n") }] })
    }

    fn access() -> Access {
        // Nothing listens on port 9: a fetch fails fast.
        let a = Access::with_certs(ISSUER, AUD, "http://127.0.0.1:9/certs");
        a.set_keys(&jwks("k1"));
        a
    }

    fn claims() -> Value {
        json!({ "aud": [AUD], "iss": ISSUER, "sub": "u-1", "email": "me@example.com", "exp": now() + 300, "iat": now() })
    }

    #[test]
    fn the_team_name_becomes_its_access_domain() {
        assert_eq!(Access::new("acme", AUD).issuer(), ISSUER);
        assert_eq!(Access::new("acme.cloudflareaccess.com", AUD).issuer(), ISSUER);
        assert_eq!(
            Access::new("https://acme.cloudflareaccess.com/", AUD).certs,
            format!("{ISSUER}/cdn-cgi/access/certs")
        );
    }

    #[tokio::test]
    async fn a_token_for_this_application_lets_its_holder_in() {
        let who = access().verify(Some(&sign("k1", claims()))).await.unwrap();
        assert_eq!(who, Identity { email: Some("me@example.com".into()), subject: "u-1".into() });
    }

    #[tokio::test]
    async fn anything_else_is_refused() {
        let a = access();
        assert_eq!(a.verify(None).await, Err(Refused::Missing));
        assert!(matches!(a.verify(Some("not.a.jwt")).await, Err(Refused::Malformed(_))));

        let mut other_app = claims();
        other_app["aud"] = json!(["someone-else"]);
        assert!(matches!(a.verify(Some(&sign("k1", other_app))).await, Err(Refused::Invalid(_))));

        let mut other_team = claims();
        other_team["iss"] = json!("https://evil.cloudflareaccess.com");
        assert!(matches!(a.verify(Some(&sign("k1", other_team))).await, Err(Refused::Invalid(_))));

        let mut expired = claims();
        expired["exp"] = json!(now() - 600);
        assert!(matches!(a.verify(Some(&sign("k1", expired))).await, Err(Refused::Invalid(_))));

        // A key the team does not have, and no way to fetch more.
        assert!(matches!(a.verify(Some(&sign("k2", claims()))).await, Err(Refused::UnknownKey)));

        // Tampered: the payload of one token with the signature of another.
        let good = sign("k1", claims());
        let mut forged = claims();
        forged["email"] = json!("boss@example.com");
        let forged = sign("k1", forged);
        let (g, f) = (good.split('.').collect::<Vec<_>>(), forged.split('.').collect::<Vec<_>>());
        let spliced = format!("{}.{}.{}", g[0], f[1], g[2]);
        assert!(matches!(a.verify(Some(&spliced)).await, Err(Refused::Invalid(_))));

        // An unsigned token.
        let none = format!("{}.{}.", b64(r#"{"alg":"none","kid":"k1"}"#), b64(&claims().to_string()));
        assert!(a.verify(Some(&none)).await.is_err());
    }

    #[tokio::test]
    async fn with_no_keys_and_no_way_to_fetch_them_nothing_gets_in() {
        let a = Access::with_certs(ISSUER, AUD, "http://127.0.0.1:9/certs");
        assert!(matches!(a.verify(Some(&sign("k1", claims()))).await, Err(Refused::Keys(_))));
    }

    fn b64(s: &str) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s)
    }
}
