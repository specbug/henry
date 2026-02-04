//! Backup management for Henry daemon.
//!
//! Handles automated backups of configuration and state databases.

use crate::types::BackupInfo;
use chrono::Utc;
use flate2::write::GzEncoder;
use flate2::Compression;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use tar::Builder;
use thiserror::Error;
use tracing::{debug, info, warn};

#[derive(Error, Debug)]
pub enum BackupError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("backup directory does not exist: {0}")]
    DirectoryNotFound(PathBuf),

    #[error("source file not found: {0}")]
    SourceNotFound(PathBuf),

    #[error("failed to create backup archive: {0}")]
    ArchiveError(String),
}

/// Backup manager for handling automated backups.
pub struct BackupManager {
    backup_dir: PathBuf,
    max_backups: usize,
}

impl BackupManager {
    /// Create a new backup manager.
    pub fn new(backup_dir: PathBuf, max_backups: usize) -> Self {
        Self {
            backup_dir,
            max_backups,
        }
    }

    /// Ensure the backup directory exists.
    pub fn ensure_backup_dir(&self) -> Result<(), BackupError> {
        if !self.backup_dir.exists() {
            fs::create_dir_all(&self.backup_dir)?;
            debug!("Created backup directory: {}", self.backup_dir.display());
        }
        Ok(())
    }

    /// Create a backup of the specified files.
    pub fn create_backup(&self, files: &[PathBuf]) -> Result<BackupInfo, BackupError> {
        self.ensure_backup_dir()?;

        // Generate backup filename with timestamp
        let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
        let backup_name = format!("henry_backup_{}.tar.gz", timestamp);
        let backup_path = self.backup_dir.join(&backup_name);

        info!("Creating backup: {}", backup_path.display());

        // Create the compressed tar archive
        let file = File::create(&backup_path)?;
        let encoder = GzEncoder::new(file, Compression::default());
        let mut archive = Builder::new(encoder);

        for source_path in files {
            if !source_path.exists() {
                warn!("Skipping non-existent file: {}", source_path.display());
                continue;
            }

            let file_name = source_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");

            if source_path.is_file() {
                archive.append_path_with_name(source_path, file_name)?;
                debug!("Added file to backup: {}", source_path.display());
            } else if source_path.is_dir() {
                archive.append_dir_all(file_name, source_path)?;
                debug!("Added directory to backup: {}", source_path.display());
            }
        }

        // Finalize the archive
        let encoder = archive
            .into_inner()
            .map_err(|e| BackupError::ArchiveError(e.to_string()))?;
        encoder
            .finish()
            .map_err(|e| BackupError::ArchiveError(e.to_string()))?;

        // Get file size
        let metadata = fs::metadata(&backup_path)?;
        let size = metadata.len();

        info!(
            "Backup created: {} ({} bytes)",
            backup_path.display(),
            size
        );

        // Clean up old backups
        self.cleanup_old_backups()?;

        Ok(BackupInfo {
            name: backup_name,
            path: backup_path.display().to_string(),
            created_at: Utc::now(),
            size,
            compressed: true,
        })
    }

    /// List all existing backups.
    pub fn list_backups(&self) -> Result<Vec<BackupInfo>, BackupError> {
        if !self.backup_dir.exists() {
            return Ok(vec![]);
        }

        let mut backups = Vec::new();

        for entry in fs::read_dir(&self.backup_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_file() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name.starts_with("henry_backup_") && name.ends_with(".tar.gz") {
                        let metadata = fs::metadata(&path)?;
                        let created_at = metadata
                            .modified()
                            .ok()
                            .map(chrono::DateTime::from)
                            .unwrap_or_else(Utc::now);

                        backups.push(BackupInfo {
                            name: name.to_string(),
                            path: path.display().to_string(),
                            created_at,
                            size: metadata.len(),
                            compressed: true,
                        });
                    }
                }
            }
        }

        // Sort by creation time, newest first
        backups.sort_by(|a, b| b.created_at.cmp(&a.created_at));

        Ok(backups)
    }

    /// Clean up old backups, keeping only the most recent ones.
    fn cleanup_old_backups(&self) -> Result<(), BackupError> {
        let mut backups = self.list_backups()?;

        if backups.len() <= self.max_backups {
            return Ok(());
        }

        // Sort by creation time (newest first) and remove old ones
        backups.sort_by(|a, b| b.created_at.cmp(&a.created_at));

        for backup in backups.iter().skip(self.max_backups) {
            info!("Removing old backup: {}", backup.path);
            if let Err(e) = fs::remove_file(&backup.path) {
                warn!("Failed to remove old backup {}: {}", backup.path, e);
            }
        }

        Ok(())
    }

    /// Delete a specific backup.
    pub fn delete_backup(&self, name: &str) -> Result<(), BackupError> {
        let backup_path = self.backup_dir.join(name);

        if !backup_path.exists() {
            return Err(BackupError::SourceNotFound(backup_path));
        }

        fs::remove_file(&backup_path)?;
        info!("Deleted backup: {}", name);

        Ok(())
    }

    /// Get total size of all backups.
    pub fn total_backup_size(&self) -> Result<u64, BackupError> {
        let backups = self.list_backups()?;
        Ok(backups.iter().map(|b| b.size).sum())
    }

    /// Get the backup directory path.
    pub fn backup_dir(&self) -> &Path {
        &self.backup_dir
    }

    /// Get the maximum number of backups to retain.
    pub fn max_backups(&self) -> usize {
        self.max_backups
    }
}

/// Format bytes in a human-readable way.
pub fn format_backup_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_backup_manager_creation() {
        let dir = tempdir().unwrap();
        let manager = BackupManager::new(dir.path().join("backups"), 5);
        assert_eq!(manager.max_backups(), 5);
    }

    #[test]
    fn test_ensure_backup_dir() {
        let dir = tempdir().unwrap();
        let backup_dir = dir.path().join("backups");
        let manager = BackupManager::new(backup_dir.clone(), 5);

        assert!(!backup_dir.exists());
        manager.ensure_backup_dir().unwrap();
        assert!(backup_dir.exists());
    }

    #[test]
    fn test_create_and_list_backups() {
        let dir = tempdir().unwrap();
        let backup_dir = dir.path().join("backups");
        let manager = BackupManager::new(backup_dir, 5);

        // Create a test file to backup
        let test_file = dir.path().join("test.txt");
        fs::write(&test_file, "test content").unwrap();

        // Create backup
        let backup = manager.create_backup(&[test_file]).unwrap();
        assert!(backup.name.starts_with("henry_backup_"));
        assert!(backup.compressed);

        // List backups
        let backups = manager.list_backups().unwrap();
        assert_eq!(backups.len(), 1);
        assert_eq!(backups[0].name, backup.name);
    }

    #[test]
    fn test_cleanup_old_backups() {
        let dir = tempdir().unwrap();
        let backup_dir = dir.path().join("backups");
        let manager = BackupManager::new(backup_dir.clone(), 2);
        manager.ensure_backup_dir().unwrap();

        // Create test file
        let test_file = dir.path().join("test.txt");
        fs::write(&test_file, "test content").unwrap();

        // Create 3 backups
        for i in 0..3 {
            // Add small delay to ensure different timestamps
            std::thread::sleep(std::time::Duration::from_millis(10));
            let backup_name = format!("henry_backup_2024010{}_120000.tar.gz", i);
            let backup_path = backup_dir.join(&backup_name);
            fs::write(&backup_path, format!("backup content {}", i)).unwrap();
        }

        // Cleanup should keep only 2
        manager.cleanup_old_backups().unwrap();

        let backups = manager.list_backups().unwrap();
        assert_eq!(backups.len(), 2);
    }

    #[test]
    fn test_format_backup_size() {
        assert_eq!(format_backup_size(500), "500 B");
        assert_eq!(format_backup_size(1024), "1.00 KB");
        assert_eq!(format_backup_size(1024 * 1024), "1.00 MB");
        assert_eq!(format_backup_size(1024 * 1024 * 1024), "1.00 GB");
    }
}
