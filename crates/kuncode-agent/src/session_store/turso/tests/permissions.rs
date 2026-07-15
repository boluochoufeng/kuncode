use crate::{
    session_store::{SessionStoreError, turso::TursoSessionStore},
    test_support::TestDir,
};

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
    let db_path = sessions_dir.join("session-store.db");

    let _store = TursoSessionStore::open(&db_path)
        .await
        .expect("store should open");

    assert_mode(&app_dir, 0o700);
    assert_mode(&sessions_dir, 0o700);
    assert_mode(&db_path, 0o600);
    let wal = database_sidecar_path(&db_path, "-wal");
    if wal.exists() {
        assert_mode(&wal, 0o600);
    }
}

#[tokio::test]
async fn open_rejects_non_utf8_database_paths() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let root = TestDir::new();
    let database_path = root
        .path()
        .join(OsString::from_vec(vec![b's', b't', b'o', b'r', b'e', 0xff]));

    let result = TursoSessionStore::open(&database_path).await;

    assert!(matches!(
        result,
        Err(SessionStoreError::InvalidDatabasePath { path }) if path == database_path
    ));
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

fn database_sidecar_path(path: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    value.into()
}
