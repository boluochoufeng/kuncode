use crate::{session_store::sqlite::SqliteSessionStore, test_support::TestDir};

#[tokio::test]
async fn open_restricts_session_store_permissions() {
    use std::{fs, os::unix::fs::PermissionsExt};

    let root = TestDir::new();
    let app_dir = root.path().join(".kuncode");
    let sessions_dir = app_dir.join("sessions");
    fs::create_dir_all(&sessions_dir).expect("session directory should be created");
    fs::set_permissions(&app_dir, fs::Permissions::from_mode(0o777))
        .expect("app dir mode should be widened");
    fs::set_permissions(&sessions_dir, fs::Permissions::from_mode(0o777))
        .expect("sessions dir mode should be widened");
    let db_path = sessions_dir.join("session-store.sqlite3");

    let _store = SqliteSessionStore::open(&db_path)
        .await
        .expect("store should open");

    assert_mode(&app_dir, 0o700);
    assert_mode(&sessions_dir, 0o700);
    assert_mode(&db_path, 0o600);
    for path in [
        sqlite_sidecar_path(&db_path, "-wal"),
        sqlite_sidecar_path(&db_path, "-shm"),
    ] {
        if path.exists() {
            assert_mode(&path, 0o600);
        }
    }
}

fn assert_mode(path: &std::path::Path, expected: u32) {
    use std::os::unix::fs::PermissionsExt;

    let actual = std::fs::metadata(path)
        .expect("path metadata should load")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(actual, expected, "unexpected mode for {}", path.display());
}

fn sqlite_sidecar_path(path: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    value.into()
}
