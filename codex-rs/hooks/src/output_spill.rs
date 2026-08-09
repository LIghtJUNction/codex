use std::fs as std_fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use codex_protocol::ThreadId;
use codex_protocol::items::HookPromptFragment;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::formatted_truncate_text;
use tokio::fs;
use tracing::warn;
use uuid::Uuid;

const HOOK_OUTPUTS_DIR: &str = "hook_outputs";
const OUTPUT_SPILL_DIR_PREFIX: &str = "codex-hook-outputs-";
const OUTPUT_SPILL_LOCK_FILENAME: &str = ".lock";
pub(crate) const DEFAULT_HOOK_OUTPUT_TOKEN_LIMIT: usize = 2_500;

static STALE_OUTPUT_DIRS_CLEANED: OnceLock<()> = OnceLock::new();

struct SpillDirectoryGuard {
    root: PathBuf,
    lock_file: Mutex<Option<File>>,
}

impl SpillDirectoryGuard {
    fn cleanup(&self) {
        let mut lock_file = self
            .lock_file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lock_file.take();
        drop(lock_file);
        match std_fs::remove_dir_all(&self.root) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => warn!(
                "failed to remove hook output directory {}: {err}",
                self.root.display()
            ),
        }
    }
}

impl Drop for SpillDirectoryGuard {
    fn drop(&mut self) {
        self.cleanup();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AdditionalContextLimit {
    token_limit: usize,
}

impl AdditionalContextLimit {
    pub(crate) fn from_config(value: Option<usize>) -> Self {
        Self {
            token_limit: value.unwrap_or(DEFAULT_HOOK_OUTPUT_TOKEN_LIMIT),
        }
    }
}

impl Default for AdditionalContextLimit {
    fn default() -> Self {
        Self::from_config(/*value*/ None)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdditionalContext {
    pub text: String,
    pub limit: AdditionalContextLimit,
}

#[derive(Clone)]
pub(crate) struct HookOutputSpiller {
    output_dir: AbsolutePathBuf,
    guard: Option<Arc<SpillDirectoryGuard>>,
    spill_enabled: bool,
}

impl HookOutputSpiller {
    pub(crate) fn new(thread_id: ThreadId) -> Self {
        let temp_root = std::env::temp_dir();
        let create_result = if temp_root.is_absolute() {
            if STALE_OUTPUT_DIRS_CLEANED.set(()).is_ok() {
                cleanup_stale_output_dirs(&temp_root);
            }
            create_spill_directory(&temp_root)
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "host temporary directory is not absolute",
            ))
        };

        match create_result {
            Ok((root, lock_file)) => Self::from_guard(thread_id, root, lock_file),
            Err(err) => {
                warn!(
                    "failed to create owned hook output directory under {}: {err}",
                    temp_root.display()
                );
                Self {
                    output_dir: AbsolutePathBuf::resolve_path_against_base(temp_root, "/")
                        .join(HOOK_OUTPUTS_DIR)
                        .join(thread_id.to_string()),
                    guard: None,
                    spill_enabled: false,
                }
            }
        }
    }

    fn from_guard(thread_id: ThreadId, root: PathBuf, lock_file: File) -> Self {
        let output_dir = AbsolutePathBuf::from_absolute_path(root)
            .expect("hook output root should be absolute")
            .join(HOOK_OUTPUTS_DIR)
            .join(thread_id.to_string());
        let owned_root = output_dir
            .as_path()
            .parent()
            .and_then(Path::parent)
            .expect("hook output directory should have an owned root")
            .to_path_buf();
        Self {
            output_dir,
            guard: Some(Arc::new(SpillDirectoryGuard {
                root: owned_root,
                lock_file: Mutex::new(Some(lock_file)),
            })),
            spill_enabled: true,
        }
    }

    /// Remove this spiller's owned directory after outstanding hook work has
    /// been joined. The guard also performs this cleanup when its last clone
    /// is dropped, covering callers that do not invoke explicit shutdown.
    pub(crate) fn cleanup(&self) {
        if let Some(guard) = &self.guard {
            guard.cleanup();
        }
    }

    /// Keeps hook text within the model-visible hook-output budget.
    ///
    /// Oversized text is written in full under the OS temp directory at
    /// `<temp_dir>/codex-hook-outputs-<pid>-<id>/hook_outputs/<thread_id>/`
    /// and replaced with the same head/tail preview style used for other truncated
    /// output, plus a path back to the preserved full text.
    pub(crate) async fn maybe_spill_text(&self, text: String) -> String {
        self.maybe_spill_text_with_limit(text, AdditionalContextLimit::default())
            .await
    }

    async fn maybe_spill_text_with_limit(
        &self,
        text: String,
        limit: AdditionalContextLimit,
    ) -> String {
        let token_limit = limit.token_limit;
        if token_limit == 0 || approx_token_count(&text) <= token_limit {
            return text;
        }
        if !self.spill_enabled {
            return formatted_truncate_text(&text, TruncationPolicy::Tokens(token_limit));
        }

        let path = self.output_dir.join(format!("{}.txt", Uuid::new_v4()));
        if let Some(parent) = path.parent()
            && let Err(err) = fs::create_dir_all(parent.as_ref()).await
        {
            warn!(
                "failed to create hook output directory {}: {err}",
                parent.display()
            );
            return formatted_truncate_text(&text, TruncationPolicy::Tokens(token_limit));
        }

        if let Err(err) = fs::write(path.as_ref(), &text).await {
            warn!("failed to write hook output {}: {err}", path.display());
            return formatted_truncate_text(&text, TruncationPolicy::Tokens(token_limit));
        }

        spilled_hook_output_preview(&text, &path, token_limit)
    }

    pub(crate) async fn maybe_spill_additional_contexts(
        &self,
        contexts: Vec<AdditionalContext>,
    ) -> Vec<String> {
        let mut spilled = Vec::with_capacity(contexts.len());
        for context in contexts {
            spilled.push(
                self.maybe_spill_text_with_limit(context.text, context.limit)
                    .await,
            );
        }
        spilled
    }

    pub(crate) async fn maybe_spill_prompt_fragments(
        &self,
        fragments: Vec<HookPromptFragment>,
    ) -> Vec<HookPromptFragment> {
        let mut spilled = Vec::with_capacity(fragments.len());
        for fragment in fragments {
            spilled.push(HookPromptFragment {
                text: self.maybe_spill_text(fragment.text).await,
                hook_run_id: fragment.hook_run_id,
            });
        }
        spilled
    }
}

fn create_spill_directory(temp_root: &Path) -> io::Result<(PathBuf, File)> {
    std_fs::create_dir_all(temp_root)?;
    for _ in 0..128 {
        let root = temp_root.join(format!(
            "{OUTPUT_SPILL_DIR_PREFIX}{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        match std_fs::create_dir(&root) {
            Ok(()) => {
                if let Err(err) = set_private_directory_permissions(&root) {
                    let _ = std_fs::remove_dir_all(&root);
                    return Err(err);
                }
                let lock_path = root.join(OUTPUT_SPILL_LOCK_FILENAME);
                let lock_file = match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(&lock_path)
                {
                    Ok(lock_file) => lock_file,
                    Err(err) => {
                        let _ = std_fs::remove_dir_all(&root);
                        return Err(err);
                    }
                };
                if let Err(err) = lock_file.try_lock() {
                    let err = match err {
                        std_fs::TryLockError::WouldBlock => io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "new hook output directory lock is already held",
                        ),
                        std_fs::TryLockError::Error(err) => err,
                    };
                    let _ = std_fs::remove_dir_all(&root);
                    return Err(err);
                }
                return Ok((root, lock_file));
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "unable to allocate a unique hook output directory",
    ))
}

fn cleanup_stale_output_dirs(temp_root: &Path) {
    let Ok(entries) = std_fs::read_dir(temp_root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with(OUTPUT_SPILL_DIR_PREFIX) {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }

        let Some(lock_file) = try_lock_output_dir(&path) else {
            continue;
        };
        drop(lock_file);
        match std_fs::remove_dir_all(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => warn!(
                "failed to remove stale hook output directory {}: {err}",
                path.display()
            ),
        }
    }
}

fn try_lock_output_dir(dir: &Path) -> Option<File> {
    let lock_path = dir.join(OUTPUT_SPILL_LOCK_FILENAME);
    let lock_file = match OpenOptions::new().read(true).write(true).open(lock_path) {
        Ok(lock_file) => lock_file,
        Err(_) => return None,
    };
    match lock_file.try_lock() {
        Ok(()) => Some(lock_file),
        Err(std_fs::TryLockError::WouldBlock | std_fs::TryLockError::Error(_)) => None,
    }
}

fn set_private_directory_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std_fs::set_permissions(path, std_fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Builds the model-visible replacement for a spilled hook output.
///
/// The path footer is budgeted before truncation so adding the recovery path
/// does not consume the configured preview budget.
fn spilled_hook_output_preview(text: &str, path: &AbsolutePathBuf, token_limit: usize) -> String {
    let footer = format!("\n\nFull hook output saved to: {}", path.display());
    let preview_policy =
        TruncationPolicy::Tokens(token_limit.saturating_sub(approx_token_count(&footer)));
    format!("{}{footer}", formatted_truncate_text(text, preview_policy))
}

#[cfg(test)]
#[path = "output_spill_tests.rs"]
mod tests;
