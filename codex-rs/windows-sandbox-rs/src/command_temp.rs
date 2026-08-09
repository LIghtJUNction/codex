//! Per-command temporary directories for the Windows sandbox.
//!
//! Child processes inherit the caller's current directory when a temporary path
//! environment variable is missing or points at a relative path. Keeping all
//! three Windows temporary variables in a command-owned directory makes that
//! fallback harmless and gives the parent an exact path it can remove after the
//! process tree has stopped.

use anyhow::Context;
use anyhow::Result;
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;
use tempfile::Builder;

const COMMAND_TEMP_PREFIX: &str = "codex-command-temp-";
const COMMAND_TEMP_LOCK_FILENAME: &str = ".lock";
const CLEANUP_ATTEMPTS: usize = 8;
const CLEANUP_RETRY_DELAY: Duration = Duration::from_millis(50);

static STALE_COMMAND_TEMP_DIRS_CLEANED: OnceLock<()> = OnceLock::new();

/// Owns the temporary directory exposed to one Windows sandbox command.
///
/// The directory is created below the host OS temporary directory. Cleanup is
/// idempotent and retries briefly to cover Windows handle-close races.
pub(crate) struct CommandTempDir {
    path: Option<PathBuf>,
    lock_file: Option<File>,
}

impl CommandTempDir {
    /// Creates a command-scoped temporary directory and rewrites all common
    /// temporary-directory variables in `env_map` to point at it.
    pub(crate) fn new(env_map: &mut HashMap<String, String>) -> Result<Self> {
        let base = std::env::temp_dir();
        if !base.is_absolute() {
            anyhow::bail!(
                "host temporary directory is not a safe location for a sandbox command: {}",
                base.display()
            );
        }
        if STALE_COMMAND_TEMP_DIRS_CLEANED.set(()).is_ok() {
            cleanup_stale_command_temp_dirs(&base);
        }

        Self::new_in(base.as_path(), env_map)
    }

    fn new_in(base: &Path, env_map: &mut HashMap<String, String>) -> Result<Self> {
        fs::create_dir_all(base)
            .with_context(|| format!("create host temporary directory {}", base.display()))?;
        let temp_dir = Builder::new()
            .prefix(COMMAND_TEMP_PREFIX)
            .tempdir_in(base)
            .with_context(|| format!("create command temporary directory in {}", base.display()))?;
        let path = temp_dir.keep();
        let lock_path = path.join(COMMAND_TEMP_LOCK_FILENAME);
        let lock_file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(lock_file) => lock_file,
            Err(err) => {
                let _ = fs::remove_dir_all(&path);
                return Err(err).with_context(|| {
                    format!(
                        "create command temporary directory lock {}",
                        lock_path.display()
                    )
                });
            }
        };
        if let Err(err) = lock_file.try_lock() {
            let err = match err {
                fs::TryLockError::WouldBlock => {
                    std::io::Error::new(std::io::ErrorKind::WouldBlock, "lock is already held")
                }
                fs::TryLockError::Error(err) => err,
            };
            let _ = fs::remove_dir_all(&path);
            return Err(err)
                .with_context(|| format!("lock command temporary directory {}", path.display()));
        }
        let path_value = path.to_string_lossy().into_owned();
        for key in ["TEMP", "TMP", "TMPDIR"] {
            replace_env_case_insensitive(env_map, key, &path_value);
        }

        Ok(Self {
            path: Some(path),
            lock_file: Some(lock_file),
        })
    }

    /// Removes the command-owned directory, retrying transient Windows sharing
    /// violations.
    pub(crate) fn cleanup(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        self.lock_file.take();

        for attempt in 0..CLEANUP_ATTEMPTS {
            match fs::remove_dir_all(&path) {
                Ok(()) => return,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
                Err(_) if attempt + 1 < CLEANUP_ATTEMPTS => {
                    thread::sleep(CLEANUP_RETRY_DELAY);
                }
                Err(err) => {
                    eprintln!(
                        "WARNING: failed to clean up command temporary directory {}: {err}",
                        path.display()
                    );
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

fn cleanup_stale_command_temp_dirs(base: &Path) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with(COMMAND_TEMP_PREFIX) {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }

        let lock_path = path.join(COMMAND_TEMP_LOCK_FILENAME);
        let lock_file = match OpenOptions::new().read(true).write(true).open(lock_path) {
            Ok(lock_file) => lock_file,
            Err(_) => continue,
        };
        if lock_file.try_lock().is_err() {
            continue;
        }
        drop(lock_file);
        if let Err(err) = fs::remove_dir_all(&path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "WARNING: failed to clean up stale command temporary directory {}: {err}",
                path.display()
            );
        }
    }
}

impl Drop for CommandTempDir {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn replace_env_case_insensitive(env_map: &mut HashMap<String, String>, key: &str, value: &str) {
    let matching_keys = env_map
        .keys()
        .filter(|existing| existing.eq_ignore_ascii_case(key))
        .cloned()
        .collect::<Vec<_>>();
    for matching_key in matching_keys {
        env_map.remove(&matching_key);
    }
    env_map.insert(key.to_string(), value.to_string());
}

#[cfg(test)]
#[path = "command_temp_tests.rs"]
mod tests;
