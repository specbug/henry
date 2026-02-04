//! Log rotation for Henry daemon.
//!
//! Handles rotating and cleaning up log files.

use chrono::Utc;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use flate2::write::GzEncoder;
use flate2::Compression;
use thiserror::Error;
use tracing::{debug, info, warn};

#[derive(Error, Debug)]
pub enum LogRotationError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("log directory does not exist: {0}")]
    DirectoryNotFound(PathBuf),
}

/// Log rotation manager.
pub struct LogRotator {
    log_dir: PathBuf,
    max_log_files: usize,
    max_log_size: u64,
    compress_rotated: bool,
}

impl LogRotator {
    /// Create a new log rotator.
    pub fn new(log_dir: PathBuf, max_log_files: usize, max_log_size: u64) -> Self {
        Self {
            log_dir,
            max_log_files,
            max_log_size,
            compress_rotated: true,
        }
    }

    /// Set whether to compress rotated logs.
    pub fn with_compression(mut self, compress: bool) -> Self {
        self.compress_rotated = compress;
        self
    }

    /// Perform log rotation.
    pub fn rotate(&self) -> Result<RotationResult, LogRotationError> {
        if !self.log_dir.exists() {
            return Err(LogRotationError::DirectoryNotFound(self.log_dir.clone()));
        }

        let mut result = RotationResult::default();

        // Find all log files
        let log_files = self.find_log_files()?;

        for log_file in &log_files {
            let metadata = fs::metadata(log_file)?;
            let size = metadata.len();

            // Check if file needs rotation
            if size >= self.max_log_size {
                self.rotate_file(log_file)?;
                result.files_rotated += 1;
                result.bytes_processed += size;
            }
        }

        // Cleanup old rotated logs
        let cleaned = self.cleanup_old_logs()?;
        result.files_deleted = cleaned;

        if result.files_rotated > 0 || result.files_deleted > 0 {
            info!(
                "Log rotation complete: {} rotated, {} deleted",
                result.files_rotated, result.files_deleted
            );
        }

        Ok(result)
    }

    /// Find all log files in the log directory.
    fn find_log_files(&self) -> Result<Vec<PathBuf>, LogRotationError> {
        let mut log_files = Vec::new();

        for entry in fs::read_dir(&self.log_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_file() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    // Match .log files that aren't already rotated
                    if name.ends_with(".log") && !name.contains(".rotated.") {
                        log_files.push(path);
                    }
                }
            }
        }

        Ok(log_files)
    }

    /// Rotate a single log file.
    fn rotate_file(&self, log_path: &Path) -> Result<(), LogRotationError> {
        let file_stem = log_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("log");

        let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
        let rotated_name = if self.compress_rotated {
            format!("{}.rotated.{}.log.gz", file_stem, timestamp)
        } else {
            format!("{}.rotated.{}.log", file_stem, timestamp)
        };

        let rotated_path = self.log_dir.join(&rotated_name);

        debug!(
            "Rotating {} -> {}",
            log_path.display(),
            rotated_path.display()
        );

        if self.compress_rotated {
            // Read and compress
            let mut content = Vec::new();
            File::open(log_path)?.read_to_end(&mut content)?;

            let file = File::create(&rotated_path)?;
            let mut encoder = GzEncoder::new(file, Compression::default());
            encoder.write_all(&content)?;
            encoder.finish()?;
        } else {
            // Just rename
            fs::rename(log_path, &rotated_path)?;
        }

        // Truncate original log file (create empty)
        if self.compress_rotated {
            File::create(log_path)?;
        }

        Ok(())
    }

    /// Clean up old rotated log files.
    fn cleanup_old_logs(&self) -> Result<usize, LogRotationError> {
        let mut rotated_files: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();

        for entry in fs::read_dir(&self.log_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_file() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name.contains(".rotated.") {
                        let modified = fs::metadata(&path)?.modified()?;
                        rotated_files.push((path, modified));
                    }
                }
            }
        }

        // Sort by modification time, oldest first
        rotated_files.sort_by(|a, b| a.1.cmp(&b.1));

        let mut deleted = 0;
        while rotated_files.len() > self.max_log_files {
            if let Some((path, _)) = rotated_files.first() {
                debug!("Removing old rotated log: {}", path.display());
                if let Err(e) = fs::remove_file(path) {
                    warn!("Failed to remove old log {}: {}", path.display(), e);
                } else {
                    deleted += 1;
                }
                rotated_files.remove(0);
            }
        }

        Ok(deleted)
    }

    /// Get total size of all log files.
    pub fn total_log_size(&self) -> Result<u64, LogRotationError> {
        if !self.log_dir.exists() {
            return Ok(0);
        }

        let mut total = 0u64;

        for entry in fs::read_dir(&self.log_dir)? {
            let entry = entry?;
            if entry.path().is_file() {
                total += fs::metadata(entry.path())?.len();
            }
        }

        Ok(total)
    }

    /// Get log directory path.
    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }
}

/// Result of a log rotation operation.
#[derive(Debug, Default)]
pub struct RotationResult {
    /// Number of files rotated.
    pub files_rotated: usize,
    /// Number of old files deleted.
    pub files_deleted: usize,
    /// Total bytes processed.
    pub bytes_processed: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_log_rotator_creation() {
        let dir = tempdir().unwrap();
        let rotator = LogRotator::new(dir.path().to_path_buf(), 5, 1024 * 1024);
        assert_eq!(rotator.max_log_files, 5);
        assert_eq!(rotator.max_log_size, 1024 * 1024);
    }

    #[test]
    fn test_find_log_files() {
        let dir = tempdir().unwrap();

        // Create some test log files
        fs::write(dir.path().join("henry.log"), "test log").unwrap();
        fs::write(dir.path().join("other.log"), "test log").unwrap();
        fs::write(dir.path().join("not_a_log.txt"), "test").unwrap();
        fs::write(dir.path().join("henry.rotated.20240101.log.gz"), "rotated").unwrap();

        let rotator = LogRotator::new(dir.path().to_path_buf(), 5, 1024);
        let files = rotator.find_log_files().unwrap();

        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|p| p.file_name().unwrap() == "henry.log"));
        assert!(files.iter().any(|p| p.file_name().unwrap() == "other.log"));
    }

    #[test]
    fn test_rotate_large_file() {
        let dir = tempdir().unwrap();

        // Create a "large" log file (exceeds 10 byte threshold)
        let log_path = dir.path().join("henry.log");
        fs::write(&log_path, "this is test content that exceeds the size limit").unwrap();

        let rotator = LogRotator::new(dir.path().to_path_buf(), 5, 10);
        let result = rotator.rotate().unwrap();

        assert_eq!(result.files_rotated, 1);

        // Original file should now be empty or small
        let new_content = fs::read_to_string(&log_path).unwrap();
        assert!(new_content.is_empty());

        // Should have a rotated file
        let files: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".rotated."))
            .collect();
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn test_total_log_size() {
        let dir = tempdir().unwrap();

        fs::write(dir.path().join("a.log"), "aaaa").unwrap();
        fs::write(dir.path().join("b.log"), "bbbb").unwrap();

        let rotator = LogRotator::new(dir.path().to_path_buf(), 5, 1024);
        let size = rotator.total_log_size().unwrap();

        assert_eq!(size, 8);
    }
}
