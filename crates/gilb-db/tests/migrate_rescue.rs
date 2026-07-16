//! Rescue path for incompatible databases: a failed migration is recognised
//! by [`gilb_db::is_migrate_error`], [`gilb_db::archive_incompatible_db`]
//! moves the files aside, and a fresh database opens at the original path.

use std::env;
use std::path::{Path, PathBuf};

use gilb_db::{archive_incompatible_db, is_migrate_error, open_db};
use uuid::Uuid;

fn temp_db_path() -> PathBuf {
    let mut p = env::temp_dir();
    p.push(format!(
        "gilb-migrate-rescue-test-{}.sqlite",
        Uuid::new_v4()
    ));
    p
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

#[tokio::test]
async fn archive_and_reopen_after_checksum_mismatch() {
    let path = temp_db_path();

    // Create + migrate a fresh db, then tamper with a recorded checksum so
    // the next open fails the way a db from an incompatible build would.
    {
        let db = open_db(&path).await.expect("initial open");
        sqlx::query(
            "UPDATE _sqlx_migrations SET checksum = x'00' \
             WHERE version = (SELECT MIN(version) FROM _sqlx_migrations)",
        )
        .execute(&db)
        .await
        .expect("tamper with migration checksum");
        db.close().await;
    }

    let err = open_db(&path)
        .await
        .expect_err("tampered checksum must fail the migration");
    assert!(is_migrate_error(&err), "not a migrate error: {err:#}");
    // Environment-style errors must not trigger the rescue path.
    assert!(!is_migrate_error(&anyhow::anyhow!("disk full")));

    // A clean close removes the WAL sidecars; recreate them to check they
    // are renamed in lockstep with the main file.
    std::fs::write(sidecar(&path, "-wal"), b"wal").unwrap();
    std::fs::write(sidecar(&path, "-shm"), b"shm").unwrap();

    let archived = archive_incompatible_db(&path).expect("archive");
    assert!(archived.exists(), "archived db missing");
    assert!(sidecar(&archived, "-wal").exists(), "archived -wal missing");
    assert!(sidecar(&archived, "-shm").exists(), "archived -shm missing");
    assert!(!path.exists(), "original db still present");
    assert!(
        !sidecar(&path, "-wal").exists(),
        "original -wal still present"
    );
    assert!(
        !sidecar(&path, "-shm").exists(),
        "original -shm still present"
    );

    // A fresh, fully-migrated db now opens at the original path.
    let db = open_db(&path).await.expect("reopen after archive");
    db.close().await;

    for p in [
        &path,
        &archived,
        &sidecar(&archived, "-wal"),
        &sidecar(&archived, "-shm"),
    ] {
        let _ = std::fs::remove_file(p);
    }
    let _ = std::fs::remove_file(sidecar(&path, "-wal"));
    let _ = std::fs::remove_file(sidecar(&path, "-shm"));
}
