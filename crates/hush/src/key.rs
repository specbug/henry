//! Where the master key lives.

use std::path::PathBuf;

use zeroize::Zeroizing;

use crate::{Error, Result};

const KEY_LEN: usize = 32;

/// The 256-bit master key. Wiped from memory when dropped.
pub struct Key(Zeroizing<[u8; KEY_LEN]>);

impl Key {
    pub fn generate() -> Result<Key> {
        let mut bytes = Zeroizing::new([0u8; KEY_LEN]);
        getrandom::fill(bytes.as_mut()).map_err(|e| Error::Random(e.to_string()))?;
        Ok(Key(bytes))
    }

    pub fn bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }

    fn to_hex(&self) -> Zeroizing<String> {
        Zeroizing::new(hex::encode(self.0.as_ref()))
    }

    fn from_hex(text: &str) -> Result<Key> {
        let mut bytes = Zeroizing::new([0u8; KEY_LEN]);
        hex::decode_to_slice(text.trim(), bytes.as_mut())
            .map_err(|e| Error::Key(format!("stored key is malformed: {e}")))?;
        Ok(Key(bytes))
    }
}

pub trait KeyStore {
    /// Return the key, creating and storing a new one if none exists yet.
    fn load_or_create(&self) -> Result<Key>;
}

/// A key in a file readable only by you. For Linux, development and tests;
/// on the Mac, [`Keychain`] keeps it off the disk Henry backs up.
pub struct FileKey {
    pub path: PathBuf,
}

impl KeyStore for FileKey {
    fn load_or_create(&self) -> Result<Key> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => Key::from_hex(&Zeroizing::new(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = Key::generate()?;
                write_private(&self.path, key.to_hex().as_bytes())
                    .map_err(|e| Error::Key(format!("{}: {e}", self.path.display())))?;
                Ok(key)
            }
            Err(e) => Err(Error::Key(format!("{}: {e}", self.path.display()))),
        }
    }
}

/// Create the file with mode 0600 from the start, so the key is never
/// readable by anyone else, even for a moment. Refuses to overwrite.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// The key as a generic password in the login Keychain. Henry runs as a
/// LaunchAgent in your session, so the Keychain is unlocked whenever it runs.
#[cfg(target_os = "macos")]
pub struct Keychain {
    pub service: String,
    pub account: String,
}

#[cfg(target_os = "macos")]
impl Default for Keychain {
    fn default() -> Self {
        Keychain { service: "dev.henry.hush".into(), account: "master-key".into() }
    }
}

#[cfg(target_os = "macos")]
impl KeyStore for Keychain {
    fn load_or_create(&self) -> Result<Key> {
        use security_framework::passwords::{get_generic_password, set_generic_password};
        // errSecItemNotFound
        const NOT_FOUND: i32 = -25300;
        match get_generic_password(&self.service, &self.account) {
            Ok(bytes) => {
                let bytes = Zeroizing::new(bytes);
                let text = std::str::from_utf8(&bytes).map_err(|_| Error::Key("Keychain item is not text".into()))?;
                Key::from_hex(text)
            }
            Err(e) if e.code() == NOT_FOUND => {
                let key = Key::generate()?;
                set_generic_password(&self.service, &self.account, key.to_hex().as_bytes())
                    .map_err(|e| Error::Key(format!("Keychain write failed: {e}")))?;
                Ok(key)
            }
            Err(e) => Err(Error::Key(format!("Keychain read failed: {e}"))),
        }
    }
}
