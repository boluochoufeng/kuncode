//! Local Turso-backed durable session storage.

use std::{
    collections::BTreeSet,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use ::turso::{Builder, Connection, Row, Value, transaction::TransactionBehavior};
use chrono::{SecondsFormat, Utc};
use tokio::sync::Mutex;

use super::{
    Checkpoint, CommittedArtifact, CommittedCompaction, JournalEntry, JournalSnapshot,
    NewCheckpoint, NewCompactionCommit, NewJournalEntry, NewSession, NewToolArtifact, Seq,
    SessionId, SessionStore, SessionStoreError,
};

mod artifact;
mod checkpoint;
mod compaction;
mod schema;
#[cfg(test)]
mod tests;

/// Serializes local access so journal coordinates and commits remain deterministic.
///
/// A database file is owned by one Kuncode process; cross-process access is unsupported.
#[derive(Debug)]
pub struct TursoSessionStore {
    connection: Mutex<Connection>,
}

impl TursoSessionStore {
    /// Opens or initializes the local Turso store and restricts database permissions.
    ///
    /// # Errors
    /// Returns an error when the path is not valid UTF-8, directory or permission
    /// operations fail, the database cannot open, or schema initialization fails.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, SessionStoreError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        secure_store_directories(&path).await?;
        let path_text = path
            .to_str()
            .ok_or_else(|| SessionStoreError::InvalidDatabasePath { path: path.clone() })?;
        let database = Builder::new_local(path_text).build().await?;
        let mut connection = database.connect()?;
        configure_connection(&connection).await?;
        schema::initialize(&mut connection).await?;
        secure_store_files(&path).await?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    #[cfg(test)]
    pub(crate) async fn connection_for_test(&self) -> tokio::sync::MutexGuard<'_, Connection> {
        self.connection.lock().await
    }
}

async fn configure_connection(connection: &Connection) -> Result<(), SessionStoreError> {
    connection.busy_timeout(Duration::from_secs(5))?;
    let rows = connection.pragma_update("journal_mode", "'wal'").await?;
    let mode = rows
        .first()
        .ok_or_else(|| ::turso::Error::Error("PRAGMA journal_mode returned no row".to_string()))?
        .get::<String>(0)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(
            ::turso::Error::Error(format!("expected WAL journal mode, found `{mode}`")).into(),
        );
    }
    connection.pragma_update("foreign_keys", "ON").await?;
    connection.pragma_update("synchronous", "FULL").await?;
    Ok(())
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
    set_private_file_if_exists(&database_file_with_suffix(path, "-wal")).await?;
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
fn database_file_with_suffix(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    value.into()
}

#[async_trait::async_trait]
impl SessionStore for TursoSessionStore {
    async fn create_session(&self, session: NewSession) -> Result<SessionId, SessionStoreError> {
        let id = new_session_id();
        let now = timestamp();
        let connection = self.connection.lock().await;
        connection
            .execute(
                r#"
                INSERT INTO sessions (
                    id, project_root, project_slug, status, created_at, updated_at
                )
                VALUES (?1, ?2, ?3, 'active', ?4, ?5)
                "#,
                ::turso::params![
                    id.as_str(),
                    session.project_root.to_string_lossy().as_ref(),
                    session.project_slug.as_str(),
                    now.as_str(),
                    now.as_str(),
                ],
            )
            .await?;
        Ok(id)
    }

    async fn append(
        &self,
        session: &SessionId,
        entry: NewJournalEntry,
    ) -> Result<Seq, SessionStoreError> {
        let payload = serde_json::to_string(&entry.payload_json)?;
        let mut connection = self.connection.lock().await;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await?;
        let seq = next_seq(&tx, session).await?;
        let now = timestamp();
        tx.execute(
            r#"
            INSERT INTO journal_entries (session_id, seq, kind, payload_json, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5)
            "#,
            ::turso::params![
                session.as_str(),
                seq.get(),
                entry.kind.as_str(),
                payload,
                now.as_str(),
            ],
        )
        .await?;
        touch_session(&tx, session, &now).await?;
        tx.commit().await.map_err(|error| {
            SessionStoreError::commit_outcome_unknown("append journal entry", error)
        })?;
        Ok(seq)
    }

    async fn put_tool_artifact(
        &self,
        session: &SessionId,
        expected_journal_head: Seq,
        artifact: NewToolArtifact,
    ) -> Result<CommittedArtifact, SessionStoreError> {
        artifact::put(&self.connection, session, expected_journal_head, artifact).await
    }

    async fn latest_checkpoint(
        &self,
        session: &SessionId,
    ) -> Result<Option<Checkpoint>, SessionStoreError> {
        checkpoint::latest(&self.connection, session).await
    }

    async fn write_checkpoint(&self, checkpoint: NewCheckpoint) -> Result<Seq, SessionStoreError> {
        checkpoint::write(&self.connection, checkpoint).await
    }

    async fn commit_compaction(
        &self,
        commit: NewCompactionCommit,
    ) -> Result<CommittedCompaction, SessionStoreError> {
        compaction::commit(&self.connection, commit).await
    }

    async fn replay_after(
        &self,
        session: &SessionId,
        seq: Seq,
    ) -> Result<Vec<JournalEntry>, SessionStoreError> {
        let connection = self.connection.lock().await;
        let mut rows = connection
            .query(
                r#"
                SELECT seq, kind, payload_json
                FROM journal_entries
                WHERE session_id = ?1 AND seq > ?2
                ORDER BY seq ASC
                "#,
                (session.as_str(), seq.get()),
            )
            .await?;
        let mut entries = Vec::new();
        while let Some(row) = rows.next().await? {
            entries.push(row_to_entry(&row)?);
        }
        Ok(entries)
    }

    async fn journal_snapshot(
        &self,
        session: &SessionId,
        seqs: &[Seq],
    ) -> Result<JournalSnapshot, SessionStoreError> {
        snapshot(&self.connection, session, seqs).await
    }
}

pub(super) async fn next_seq(
    connection: &Connection,
    session: &SessionId,
) -> Result<Seq, SessionStoreError> {
    let head = read_head(connection, session).await?;
    let value =
        head.get()
            .checked_add(1)
            .ok_or_else(|| SessionStoreError::JournalStoredIntegrity {
                session_id: session.as_str().to_string(),
                message: "journal sequence exhausted Turso's signed integer range".to_string(),
            })?;
    Ok(Seq::new(value))
}

pub(super) async fn compare_and_lock(
    connection: &Connection,
    session: &SessionId,
    expected: Seq,
) -> Result<(), SessionStoreError> {
    // The surrounding immediate transaction already owns writer intent. This
    // predicate binds dependent writes to the exact journal head audited by the caller.
    let affected = connection
        .execute(
            r#"
            UPDATE sessions
            SET updated_at = updated_at
            WHERE id = ?1
              AND ?2 = (
                SELECT COALESCE(MAX(seq), 0)
                FROM journal_entries
                WHERE session_id = ?3
              )
            "#,
            (session.as_str(), expected.get(), session.as_str()),
        )
        .await?;
    if affected == 1 {
        return Ok(());
    }

    let actual = read_head(connection, session).await?.get();
    Err(SessionStoreError::JournalHeadConflict {
        expected: expected.get(),
        actual,
    })
}

async fn read_head(connection: &Connection, session: &SessionId) -> Result<Seq, SessionStoreError> {
    let mut rows = connection
        .query(
            "SELECT COALESCE(MAX(seq), 0) FROM journal_entries WHERE session_id = ?1",
            [session.as_str()],
        )
        .await?;
    let row = rows
        .next()
        .await?
        .ok_or(::turso::Error::QueryReturnedNoRows)?;
    let value = row
        .get::<i64>(0)
        .map_err(|error| journal_integrity(session, error.to_string()))?;
    if value < 0 {
        return Err(journal_integrity(
            session,
            format!("journal head must be non-negative, found {value}"),
        ));
    }
    Ok(Seq::new(value))
}

// Leave ample room under the host-parameter limit after the session bind.
const SNAPSHOT_QUERY_CHUNK: usize = 256;

async fn snapshot(
    connection: &Mutex<Connection>,
    session: &SessionId,
    seqs: &[Seq],
) -> Result<JournalSnapshot, SessionStoreError> {
    let mut connection = connection.lock().await;
    let tx = connection.transaction().await?;
    let outcome = async {
        // The first read establishes the transaction snapshot before any chunked
        // row query can overlap with a later append.
        let head = read_head(&tx, session).await?;
        let requested = seqs.iter().copied().collect::<BTreeSet<_>>();
        let requested = requested.into_iter().collect::<Vec<_>>();
        let mut entries = Vec::with_capacity(requested.len());
        for chunk in requested.chunks(SNAPSHOT_QUERY_CHUNK) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let query = format!(
                "SELECT seq, kind, payload_json FROM journal_entries \
                 WHERE session_id = ? AND seq IN ({placeholders}) ORDER BY seq ASC"
            );
            // Only placeholder count is generated dynamically; every value remains
            // bound so session data can never alter the query text.
            let mut params = Vec::with_capacity(chunk.len() + 1);
            params.push(Value::Text(session.as_str().to_string()));
            params.extend(chunk.iter().map(|seq| Value::Integer(seq.get())));
            let mut rows = tx.query(query, params).await?;
            while let Some(row) = rows.next().await? {
                let seq = row
                    .get(0)
                    .map(Seq::new)
                    .map_err(|error| journal_integrity(session, error.to_string()))?;
                let kind = row
                    .get(1)
                    .map_err(|error| journal_integrity(session, error.to_string()))?;
                let payload = row
                    .get::<String>(2)
                    .map_err(|error| journal_integrity(session, error.to_string()))?;
                let payload_json = serde_json::from_str(&payload)
                    .map_err(|error| journal_integrity(session, error.to_string()))?;
                entries.push(JournalEntry {
                    seq,
                    kind,
                    payload_json,
                });
            }
        }
        Ok(JournalSnapshot::new(head, entries))
    }
    .await;
    match outcome {
        Ok(snapshot) => {
            tx.commit().await?;
            Ok(snapshot)
        }
        Err(error) => {
            tx.rollback().await?;
            Err(error)
        }
    }
}

fn journal_integrity(session: &SessionId, message: String) -> SessionStoreError {
    SessionStoreError::JournalStoredIntegrity {
        session_id: session.as_str().to_string(),
        message,
    }
}

pub(super) async fn touch_session(
    connection: &Connection,
    session: &SessionId,
    updated_at: &str,
) -> Result<(), SessionStoreError> {
    connection
        .execute(
            "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
            (updated_at, session.as_str()),
        )
        .await?;
    Ok(())
}

fn row_to_entry(row: &Row) -> Result<JournalEntry, SessionStoreError> {
    let payload = row.get::<String>(2)?;
    Ok(JournalEntry {
        seq: Seq::new(row.get(0)?),
        kind: row.get(1)?,
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
