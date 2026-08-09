use super::*;
use anyhow::Context;
use anyhow::Result;
use tempfile::tempdir;

#[test]
fn hook_output_spiller_is_scoped_to_its_thread() {
    let thread_id = ThreadId::new();
    let spiller = HookOutputSpiller::new(thread_id);
    let thread_name = thread_id.to_string();

    assert_eq!(
        spiller.output_dir.file_name(),
        Some(std::ffi::OsStr::new(&thread_name))
    );
    assert_eq!(
        spiller
            .output_dir
            .parent()
            .and_then(|path| path.file_name()),
        Some(std::ffi::OsStr::new(HOOK_OUTPUTS_DIR))
    );
    assert!(
        spiller
            .output_dir
            .parent()
            .and_then(|path| path.parent())
            .and_then(|path| path.file_name())
            .is_some_and(|name| name.to_string_lossy().starts_with(OUTPUT_SPILL_DIR_PREFIX))
    );
}

#[tokio::test]
async fn small_hook_output_remains_inline() -> Result<()> {
    let dir = tempdir()?;
    let output_dir = AbsolutePathBuf::from_absolute_path(dir.path())?.join(HOOK_OUTPUTS_DIR);
    let spiller = HookOutputSpiller {
        output_dir: output_dir.clone(),
        guard: None,
        spill_enabled: true,
    };

    let output = spiller.maybe_spill_text("short".to_string()).await;

    assert_eq!(output, "short");
    assert!(!output_dir.exists());
    Ok(())
}

#[tokio::test]
async fn large_hook_output_spills_to_file() -> Result<()> {
    let dir = tempdir()?;
    let text = "hook output ".repeat(1_000);
    let output_dir = AbsolutePathBuf::from_absolute_path(dir.path())?.join(HOOK_OUTPUTS_DIR);
    let spiller = HookOutputSpiller {
        output_dir,
        guard: None,
        spill_enabled: true,
    };

    let output = spiller.maybe_spill_text(text.clone()).await;

    assert!(output.contains("tokens truncated"));
    let path = output
        .lines()
        .find_map(|line| line.strip_prefix("Full hook output saved to: "))
        .context("spill path")?;
    assert_eq!(fs::read_to_string(path).await?, text);
    Ok(())
}

#[tokio::test]
async fn additional_contexts_apply_limits_individually() -> Result<()> {
    let dir = tempdir()?;
    let limited_text = "limited hook output ".repeat(1_000);
    let unlimited_text = "unlimited hook output ".repeat(5_000);
    assert!(approx_token_count(&unlimited_text) > 10_000);
    let output_dir = AbsolutePathBuf::from_absolute_path(dir.path())?.join(HOOK_OUTPUTS_DIR);
    let spiller = HookOutputSpiller {
        output_dir,
        guard: None,
        spill_enabled: true,
    };
    let output = spiller
        .maybe_spill_additional_contexts(vec![
            AdditionalContext {
                text: limited_text.clone(),
                limit: AdditionalContextLimit::from_config(Some(1)),
            },
            AdditionalContext {
                text: unlimited_text.clone(),
                limit: AdditionalContextLimit::from_config(Some(0)),
            },
            AdditionalContext {
                text: unlimited_text.clone(),
                limit: AdditionalContextLimit::from_config(Some(usize::MAX)),
            },
        ])
        .await;
    let [limited_output, zero_limit_output, high_limit_output] = output.as_slice() else {
        panic!("expected one output for each additional context");
    };
    assert!(limited_output.contains("Full hook output saved to:"));
    assert_eq!(zero_limit_output, &unlimited_text);
    assert_eq!(high_limit_output, &unlimited_text);
    Ok(())
}

#[test]
fn owned_spill_directory_is_removed_when_spiller_is_dropped() -> Result<()> {
    let temp_root = tempdir()?;
    let thread_id = ThreadId::new();
    let (root, lock_file) = create_spill_directory(temp_root.path())?;
    let spiller = HookOutputSpiller::from_guard(thread_id, root, lock_file);
    let owned_root = spiller
        .output_dir
        .parent()
        .and_then(|path| path.parent())
        .context("owned root")?
        .to_path_buf();
    assert!(owned_root.exists());

    drop(spiller);

    assert!(!owned_root.exists());
    Ok(())
}

#[test]
fn stale_spill_directories_are_removed_only_with_an_unlocked_codex_lock() -> Result<()> {
    let temp_root = tempdir()?;
    let stale_root = temp_root
        .path()
        .join(format!("{OUTPUT_SPILL_DIR_PREFIX}stale"));
    std::fs::create_dir(&stale_root)?;
    std::fs::File::create(stale_root.join(OUTPUT_SPILL_LOCK_FILENAME))?;

    let live_root = temp_root
        .path()
        .join(format!("{OUTPUT_SPILL_DIR_PREFIX}live"));
    std::fs::create_dir(&live_root)?;
    let live_lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(live_root.join(OUTPUT_SPILL_LOCK_FILENAME))?;
    live_lock.try_lock()?;

    cleanup_stale_output_dirs(temp_root.path());

    assert!(!stale_root.exists());
    assert!(live_root.exists());
    Ok(())
}
