//! Hush: secrets, kept quiet.
//!
//! Secrets are sealed with XChaCha20-Poly1305 before they reach Canon, and
//! opened only when a container is about to start. Each ciphertext is bound
//! to its slot (scope and name), so a sealed value copied into another slot
//! fails to open instead of leaking into the wrong app.
//!
//! The master key lives in the macOS login Keychain. Off macOS, and in tests,
//! it lives in a file only you can read.

mod key;

use std::fmt;
use std::sync::Arc;

use canon::{Canon, serde_json};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

#[cfg(target_os = "macos")]
pub use key::Keychain;
pub use key::{FileKey, Key, KeyStore};

/// Scope for Henry's own secrets: provider keys, Cloudflare and R2 tokens.
pub const HOST: &str = "";

const VERSION: u8 = 1;
const NONCE_LEN: usize = 24;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Canon(#[from] canon::Error),
    #[error("the sealed value for {0} could not be opened: wrong key, or it was moved from another slot")]
    Unseal(String),
    #[error("the sealed value for {0} has an unknown format")]
    Format(String),
    #[error("secret names are A-Z, 0-9 and _, starting with a letter or _: `{0}`")]
    BadName(String),
    #[error("the master key is unavailable: {0}")]
    Key(String),
    #[error("no randomness available: {0}")]
    Random(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A secret value. It never prints, and its memory is wiped when dropped.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Zeroizing<String>);

impl Secret {
    pub fn new(value: impl Into<String>) -> Secret {
        Secret(Zeroizing::new(value.into()))
    }

    /// The only way to read a secret. Search for this call to audit every
    /// place a value leaves Hush.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// What a service needs, split into what Hush holds and what it
/// still lacks. A non-empty `missing` is what puts an app in "Needs you".
#[derive(Debug)]
pub struct Provision {
    pub env: Vec<(String, Secret)>,
    pub missing: Vec<String>,
}

pub struct Hush {
    db: Arc<Canon>,
    cipher: XChaCha20Poly1305,
}

impl Hush {
    pub fn new(db: Arc<Canon>, key: &Key) -> Hush {
        Hush { db, cipher: XChaCha20Poly1305::new(key.bytes().into()) }
    }

    /// Load the master key from `store`, creating it on first use.
    pub fn open(db: Arc<Canon>, store: &dyn KeyStore) -> Result<Hush> {
        Ok(Hush::new(db, &store.load_or_create()?))
    }

    pub fn set(&self, scope: &str, name: &str, value: &Secret) -> Result<()> {
        check_name(name)?;
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).map_err(|e| Error::Random(e.to_string()))?;
        let aad = slot(scope, name);
        let sealed = self
            .cipher
            .encrypt(XNonce::from_slice(&nonce), Payload { msg: value.expose().as_bytes(), aad: &aad })
            .map_err(|_| Error::Unseal(label(scope, name)))?;
        let mut out = Vec::with_capacity(1 + NONCE_LEN + sealed.len());
        out.push(VERSION);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        self.db.put_sealed(scope, name, &out)?;
        self.db.record(
            (!scope.is_empty()).then_some(scope),
            "secret.set",
            &format!("{} set", label(scope, name)),
            &serde_json::Value::Null,
        )?;
        Ok(())
    }

    pub fn get(&self, scope: &str, name: &str) -> Result<Option<Secret>> {
        let Some(blob) = self.db.get_sealed(scope, name)? else { return Ok(None) };
        let (version, rest) = blob.split_first().ok_or_else(|| Error::Format(label(scope, name)))?;
        if *version != VERSION || rest.len() < NONCE_LEN {
            return Err(Error::Format(label(scope, name)));
        }
        let (nonce, sealed) = rest.split_at(NONCE_LEN);
        let aad = slot(scope, name);
        let plain = Zeroizing::new(
            self.cipher
                .decrypt(XNonce::from_slice(nonce), Payload { msg: sealed, aad: &aad })
                .map_err(|_| Error::Unseal(label(scope, name)))?,
        );
        let text = std::str::from_utf8(&plain).map_err(|_| Error::Format(label(scope, name)))?;
        Ok(Some(Secret::new(text)))
    }

    /// Names only. Values never leave except through [`Hush::get`].
    pub fn names(&self, scope: &str) -> Result<Vec<String>> {
        Ok(self.db.sealed_names(scope)?)
    }

    pub fn remove(&self, scope: &str, name: &str) -> Result<bool> {
        let removed = self.db.delete_sealed(scope, name)?;
        if removed {
            self.db.record(
                (!scope.is_empty()).then_some(scope),
                "secret.removed",
                &format!("{} removed", label(scope, name)),
                &serde_json::Value::Null,
            )?;
        }
        Ok(removed)
    }

    /// Open every secret `names` lists for `scope`, reporting what is absent.
    pub fn provision(&self, scope: &str, names: &[String]) -> Result<Provision> {
        let mut env = Vec::with_capacity(names.len());
        let mut missing = Vec::new();
        for name in names {
            match self.get(scope, name)? {
                Some(v) => env.push((name.clone(), v)),
                None => missing.push(name.clone()),
            }
        }
        Ok(Provision { env, missing })
    }
}

/// The additional data each ciphertext is bound to.
fn slot(scope: &str, name: &str) -> Vec<u8> {
    let mut aad = b"henry/hush/v1\0".to_vec();
    aad.extend_from_slice(scope.as_bytes());
    aad.push(0);
    aad.extend_from_slice(name.as_bytes());
    aad
}

fn label(scope: &str, name: &str) -> String {
    if scope.is_empty() { name.to_owned() } else { format!("{scope}/{name}") }
}

fn check_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if ok { Ok(()) } else { Err(Error::BadName(name.to_owned())) }
}

#[cfg(test)]
mod tests;
