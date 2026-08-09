use super::COMMAND_TEMP_LOCK_FILENAME;
use super::COMMAND_TEMP_PREFIX;
use super::CommandTempDir;
use super::cleanup_stale_command_temp_dirs;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::fs;
use tempfile::TempDir;

#[test]
fn command_temp_dir_rewrites_variables_and_cleans_owned_files() {
    let base = TempDir::new().expect("create temp base");
    let mut env_map = HashMap::from([
        ("Temp".to_string(), "relative-temp".to_string()),
        ("TMPDIR".to_string(), "old-tmpdir".to_string()),
        ("OTHER".to_string(), "preserved".to_string()),
    ]);
    let command_temp = CommandTempDir::new_in(base.path(), &mut env_map)
        .expect("create command temporary directory");
    let path = command_temp
        .path()
        .expect("command temp path")
        .to_path_buf();
    let expected = path.to_string_lossy().into_owned();

    assert_eq!(
        env_map,
        HashMap::from([
            ("TEMP".to_string(), expected.clone()),
            ("TMP".to_string(), expected.clone()),
            ("TMPDIR".to_string(), expected),
            ("OTHER".to_string(), "preserved".to_string()),
        ])
    );

    let orphan = path.join("Q12345678.tmp");
    fs::write(&orphan, b"command-owned").expect("write command temp file");
    let mut command_temp = command_temp;
    command_temp.cleanup();

    assert!(!path.exists());
    assert!(!orphan.exists());
}

#[test]
fn command_temp_dir_is_created_below_the_host_temp_root() {
    let base = TempDir::new().expect("create temp base");
    let mut env_map = HashMap::new();

    let command_temp =
        CommandTempDir::new(&mut env_map).expect("create command temporary directory");
    let path = command_temp.path().expect("command temp path");

    assert!(path.is_absolute());
    assert!(path.starts_with(std::env::temp_dir()));
    assert!(!path.starts_with(base.path()));
}

#[test]
fn stale_command_temp_directories_are_removed_without_disturbing_active_commands() {
    let base = TempDir::new().expect("create temp base");
    let stale = base.path().join(format!("{COMMAND_TEMP_PREFIX}stale"));
    fs::create_dir(&stale).expect("create stale command temp directory");
    fs::File::create(stale.join(COMMAND_TEMP_LOCK_FILENAME)).expect("create stale lock");

    let mut env_map = HashMap::new();
    let active = CommandTempDir::new_in(base.path(), &mut env_map)
        .expect("create active command temporary directory");
    let active_path = active
        .path()
        .expect("active command temp path")
        .to_path_buf();

    cleanup_stale_command_temp_dirs(base.path());

    assert!(!stale.exists());
    assert!(active_path.exists());
}
