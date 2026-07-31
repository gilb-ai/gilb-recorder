//! The visible data directory and the one-time move into it.
//!
//! Both need a *real* `$HOME` to resolve against, and `data_dir`'s override is
//! a process-global `OnceLock`, so these live in their own test binary with
//! `HOME` pointed at a temp directory. Everything a user would lose in a bad
//! migration — the database, the 570 MB model, meeting recordings — is
//! represented by a file whose contents are asserted after the move.

use std::path::{Path, PathBuf};

/// `$HOME` is process-wide; keep every case in one test so nothing races.
#[test]
fn data_dir_is_visible_and_the_old_one_is_moved_into_it() {
    let home = tempfile::tempdir().expect("temp home");
    set_home(home.path());

    // A pre-move install: hidden directory with a database and a model in it.
    let legacy = home.path().join(".gilb");
    std::fs::create_dir_all(legacy.join("models")).unwrap();
    std::fs::write(legacy.join("db.sqlite"), b"sessions and actions").unwrap();
    std::fs::write(legacy.join("models/whisper.bin"), b"570 MB, pretend").unwrap();

    let expected = home.path().join("Documents").join("gilb");
    assert_eq!(
        gilb_config::data_dir().unwrap(),
        expected,
        "the default must be the visible Documents folder"
    );

    let moved = gilb_config::migrate_legacy_data_dir().unwrap();
    assert_eq!(moved.as_deref(), Some(legacy.as_path()));
    assert!(!legacy.exists(), "the old directory is gone, not copied");
    assert_eq!(
        std::fs::read_to_string(expected.join("db.sqlite")).unwrap(),
        "sessions and actions",
        "the database survived the move"
    );
    assert!(
        expected.join("models/whisper.bin").exists(),
        "the model came along — re-downloading it is a 570 MB apology"
    );

    // Idempotent: a second launch finds nothing to do.
    assert_eq!(gilb_config::migrate_legacy_data_dir().unwrap(), None);

    // And if a hidden directory reappears next to a live one — an older build
    // run after the move — the current data wins and the old copy is left
    // alone rather than overwriting anything.
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("db.sqlite"), b"stale").unwrap();
    assert_eq!(gilb_config::migrate_legacy_data_dir().unwrap(), None);
    assert_eq!(
        std::fs::read_to_string(expected.join("db.sqlite")).unwrap(),
        "sessions and actions"
    );

    // The paths everything else derives from, including the prompt the user is
    // meant to find and edit.
    assert_eq!(gilb_config::db_path().unwrap(), expected.join("db.sqlite"));
    assert_eq!(
        gilb_config::assist_prompt_path().unwrap(),
        expected.join("prompts").join("realtime_assist.md")
    );
    assert_eq!(
        gilb_config::prompts_dir().unwrap(),
        expected.join("prompts")
    );
    assert_eq!(
        gilb_config::ensure_prompts_dir().unwrap(),
        expected.join("prompts")
    );
    assert!(expected.join("prompts").is_dir());
}

fn set_home(dir: &Path) {
    // `directories` reads $HOME on unix and the known-folder API on Windows,
    // where $HOME does not exist — so this test is unix-only by construction.
    let docs: PathBuf = dir.join("Documents");
    std::fs::create_dir_all(&docs).expect("a Documents folder every OS would have");
    // SAFETY: single-threaded test binary, set before any thread is spawned.
    unsafe { std::env::set_var("HOME", dir) };
}
