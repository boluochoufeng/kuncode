use std::{
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use chrono::{SecondsFormat, Utc};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};

use super::{
    Checkpoint, JournalEntry, NewCheckpoint, NewJournalEntry, NewSession, NewToolArtifact, Seq,
    SessionId, SessionStore, SessionStoreError, ToolArtifactRef,
};

mod artifact;
mod checkpoint;
mod schema;
#[cfg(test)]
mod tests;

#[derive(Debug)]
pub struct SqliteSessionStore {
    pool: SqlitePool,
}

impl SqliteSessionStore {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, SessionStoreError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        secure_store_directories(&path).await?;
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        let store = Self { pool };
        schema::migrate(&store.pool).await?;
        secure_store_files(&path).await?;
        Ok(store)
    }
}

#[cfg(unix)]
async fn secure_store_directories(path: &Path) -> Result<(), SessionStoreError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };

    set_private_dir(parent).await?;
    if parent.file_name() == Some(std::ffi::OsStr::new("sessions"))
        && let Some(app_dir) = parent.parent()
        && app_dir.file_name() == Some(std::ffi::OsStr::new(".kuncode"))
    {
        set_private_dir(app_dir).await?;
    }
    Ok(())
}

#[cfg(not(unix))]
async fn secure_store_directories(_path: &Path) -> Result<(), SessionStoreError> {
    Ok(())
}

#[cfg(unix)]
async fn secure_store_files(path: &Path) -> Result<(), SessionStoreError> {
    set_private_file_if_exists(path).await?;
    for sidecar in [
        sqlite_file_with_suffix(path, "-wal"),
        sqlite_file_with_suffix(path, "-shm"),
    ] {
        set_private_file_if_exists(&sidecar).await?;
    }
    Ok(())
}

#[cfg(not(unix))]
async fn secure_store_files(_path: &Path) -> Result<(), SessionStoreError> {
    Ok(())
}

#[cfg(unix)]
async fn set_private_dir(path: &Path) -> Result<(), SessionStoreError> {
    use std::{fs::Permissions, os::unix::fs::PermissionsExt};

    tokio::fs::set_permissions(path, Permissions::from_mode(0o700)).await?;
    Ok(())
}

#[cfg(unix)]
async fn set_private_file_if_exists(path: &Path) -> Result<(), SessionStoreError> {
    use std::{fs::Permissions, io::ErrorKind, os::unix::fs::PermissionsExt};

    match tokio::fs::metadata(path).await {
        Ok(metadata) if metadata.is_file() => {
            tokio::fs::set_permissions(path, Permissions::from_mode(0o600)).await?;
            Ok(())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn sqlite_file_with_suffix(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    value.into()
}

#[async_trait::async_trait]
impl SessionStore for SqliteSessionStore {
    async fn create_session(&self, session: NewSession) -> Result<SessionId, SessionStoreError> {
        let id = new_session_id();
        let now = timestamp();
        sqlx::query(
            r#"
            INSERT INTO sessions (id, project_root, project_slug, status, created_at, updated_at)
            VALUES (?, ?, ?, 'active', ?, ?)
            "#,
        )
        .bind(id.as_str())
        .bind(session.project_root.to_string_lossy().as_ref())
        .bind(&session.project_slug)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    async fn append(
        &self,
        session: &SessionId,
        entry: NewJournalEntry,
    ) -> Result<Seq, SessionStoreError> {
        let mut tx = self.pool.begin().await?;
        let seq = next_seq(&mut tx, session).await?;
        let now = timestamp();
        let payload = serde_json::to_string(&entry.payload_json)?;
        sqlx::query(
            r#"
            INSERT INTO journal_entries (session_id, seq, kind, payload_json, created_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind(session.as_str())
        .bind(seq.get())
        .bind(entry.kind.as_str())
        .bind(payload)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        touch_session(&mut tx, session, &now).await?;
        tx.commit().await?;
        Ok(seq)
    }

    async fn put_tool_artifact(
        &self,
        session: &SessionId,
        artifact: NewToolArtifact,
    ) -> Result<ToolArtifactRef, SessionStoreError> {
        artifact::put(&self.pool, session, artifact).await
    }

    async fn latest_checkpoint(
        &self,
        session: &SessionId,
    ) -> Result<Option<Checkpoint>, SessionStoreError> {
        checkpoint::latest(&self.pool, session).await
    }

    async fn write_checkpoint(&self, checkpoint: NewCheckpoint) -> Result<Seq, SessionStoreError> {
        checkpoint::write(&self.pool, checkpoint).await
    }

    async fn replay_after(
        &self,
        session: &SessionId,
        seq: Seq,
    ) -> Result<Vec<JournalEntry>, SessionStoreError> {
        let rows = sqlx::query(
            r#"
            SELECT seq, kind, payload_json
            FROM journal_entries
            WHERE session_id = ? AND seq > ?
            ORDER BY seq ASC
            "#,
        )
        .bind(session.as_str())
        .bind(seq.get())
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(row_to_entry).collect()
    }
}

pub(super) async fn next_seq(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session: &SessionId,
) -> Result<Seq, SessionStoreError> {
    let value: i64 = sqlx::query_scalar(
        r#"
        SELECT COALESCE(MAX(seq), 0) + 1
        FROM journal_entries
        WHERE session_id = ?
        "#,
    )
    .bind(session.as_str())
    .fetch_one(&mut **tx)
    .await?;
    Ok(Seq::new(value))
}

pub(super) async fn touch_session(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session: &SessionId,
    updated_at: &str,
) -> Result<(), SessionStoreError> {
    sqlx::query("UPDATE sessions SET updated_at = ? WHERE id = ?")
        .bind(updated_at)
        .bind(session.as_str())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn row_to_entry(row: sqlx::sqlite::SqliteRow) -> Result<JournalEntry, SessionStoreError> {
    let payload: String = row.try_get("payload_json")?;
    Ok(JournalEntry {
        seq: Seq::new(row.try_get("seq")?),
        kind: row.try_get("kind")?,
        payload_json: serde_json::from_str(&payload)?,
    })
}

fn new_session_id() -> SessionId {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let seq = NEXT.fetch_add(1, Ordering::Relaxed);
    SessionId::new(format!(
        "session-{}-{}-{seq}",
        Utc::now().timestamp_micros(),
        std::process::id()
    ))
}

pub(super) fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}
