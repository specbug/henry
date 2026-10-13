//! Where Henry keeps its things, and the config that lives there.
//!
//! On the Mac that is `~/Library/Application Support/Henry`, deliberately
//! outside `~/Documents`: macOS's privacy layer (TCC) denies background
//! agents there, which is what made hostd read manifests through `git show`.
//! `HENRY_HOME` overrides it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

pub struct Home {
    pub root: PathBuf,
}

impl Home {
    pub fn locate() -> Result<Home> {
        if let Some(root) = std::env::var_os("HENRY_HOME") {
            return Ok(Home { root: root.into() });
        }
        let home = std::env::var_os("HOME").context("HOME is not set; set HENRY_HOME instead")?;
        let home = PathBuf::from(home);
        let root = if cfg!(target_os = "macos") {
            home.join("Library/Application Support/Henry")
        } else {
            match std::env::var_os("XDG_DATA_HOME") {
                Some(data) => PathBuf::from(data).join("henry"),
                None => home.join(".local/share/henry"),
            }
        };
        Ok(Home { root })
    }

    /// Create the directory tree, private to you.
    pub fn ensure(&self) -> Result<()> {
        for dir in [self.root.clone(), self.checkouts(), self.logs()] {
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        Ok(())
    }

    pub fn canon(&self) -> PathBuf {
        self.root.join("canon.db")
    }

    pub fn config(&self) -> PathBuf {
        self.root.join("henry.toml")
    }

    pub fn lock(&self) -> PathBuf {
        self.root.join("henry.lock")
    }

    /// The master key, where no Keychain is available.
    #[cfg(not(target_os = "macos"))]
    pub fn key_file(&self) -> PathBuf {
        self.root.join("master.key")
    }

    /// Henry's own clones. Your working copies are never touched.
    pub fn checkouts(&self) -> PathBuf {
        self.root.join("checkouts")
    }

    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }
}

/// `henry.toml`. Every field has a default, so the file is optional.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub limits: LimitsConfig,
    pub ai: AiConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LimitsConfig {
    /// The most memory one service may ask for, e.g. `8GiB`.
    pub max_service_memory: String,
    pub max_service_cpus: f64,
    pub max_services_per_app: usize,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        let d = lease::Limits::default();
        LimitsConfig {
            max_service_memory: format!("{}GiB", d.max_memory_bytes >> 30),
            max_service_cpus: d.max_cpus,
            max_services_per_app: d.max_services,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AiConfig {
    /// `provider/model`. Unset means Meta's default until Bench says otherwise.
    pub default_model: Option<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("reading {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn limits(&self) -> Result<lease::Limits> {
        let memory = lease::parse_bytes(&self.limits.max_service_memory).with_context(|| {
            format!("limits.max_service_memory: `{}` is not a size", self.limits.max_service_memory)
        })?;
        Ok(lease::Limits {
            max_services: self.limits.max_services_per_app,
            max_memory_bytes: memory,
            max_cpus: self.limits.max_service_cpus,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_config_is_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let c = Config::load(&dir.path().join("henry.toml")).unwrap();
        let l = c.limits().unwrap();
        let d = lease::Limits::default();
        assert_eq!((l.max_memory_bytes, l.max_cpus, l.max_services), (d.max_memory_bytes, d.max_cpus, d.max_services));
    }

    #[test]
    fn config_overrides_and_rejects_typos() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("henry.toml");
        std::fs::write(&path, "[limits]\nmax_service_memory = \"2GiB\"\n[ai]\ndefault_model = \"meta/muse-spark\"\n")
            .unwrap();
        let c = Config::load(&path).unwrap();
        assert_eq!(c.limits().unwrap().max_memory_bytes, 2 << 30);
        assert_eq!(c.ai.default_model.as_deref(), Some("meta/muse-spark"));

        std::fs::write(&path, "[limit]\nmax_service_memory = \"2GiB\"\n").unwrap();
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn home_dirs_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home { root: dir.path().join("henry") };
        home.ensure().unwrap();
        assert!(home.checkouts().is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&home.root).unwrap().permissions().mode() & 0o777, 0o700);
        }
    }
}
