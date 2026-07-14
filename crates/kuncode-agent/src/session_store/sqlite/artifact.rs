//! Idempotent artifact persistence tied to an auditable journal fact.

use sqlx::{Row, SqlitePool};

use super::{head::compare_and_lock, next_seq, timestamp, touch_session};
use crate::session_store::{
    CommittedArtifact, JournalKind, NewToolArtifact, Seq, SessionId, SessionStoreError,
    ToolArtifactRef,
    artifact::{artifact_source, validate_artifact_content, validate_artifact_id},
};

mod journal;

use journal::load_journal_seq;

pub(super) async fn put(
    pool: &SqlitePool,
    session: &SessionId,
    expected_journal_head: Seq,
    artifact: NewToolArtifact,
) -> Result<CommittedArtifact, SessionStoreError> {
    artifact.validate_identity()?;
    let mut tx = pool.begin().await?;
    compare_and_lock(&mut tx, session, expected_journal_head).await?;
    let now = timestamp();
    // The content-derived id makes retries idempotent, but `OR IGNORE` may also
    // conceal a conflicting row. Durable state is therefore revalidated below.
    let result = sqlx::query(
        r#"
        INSERT OR IGNORE INTO tool_artifacts (
          session_id,
          artifact_id,
          content_hash,
          bytes,
          preview,
          payload_text,
          storage_ref,
          created_at
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(session.as_str())
    .bind(artifact.artifact_id())
    .bind(artifact.content_hash())
    .bind(artifact.bytes())
    .bind(artifact.preview())
    .bind(artifact.payload_text())
    .bind(artifact.storage_ref())
    .bind(&now)
    .execute(&mut *tx)
    .await?;

    if result.rows_affected() > 0 {
        let seq = next_seq(&mut tx, session).await?;
        let payload_json = serde_json::json!({
            "artifact_id": artifact.artifact_id(),
            "content_hash": artifact.content_hash(),
            "bytes": artifact.bytes(),
            "preview": artifact.preview(),
        });
        sqlx::query(
            r#"
            INSERT INTO journal_entries (session_id, seq, kind, payload_json, created_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind(session.as_str())
        .bind(seq.get())
        .bind(JournalKind::ToolArtifact.as_str())
        .bind(serde_json::to_string(&payload_json)?)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        touch_session(&mut tx, session, &now).await?;
    }

    // Receipts are derived only from the row and journal fact observed inside
    // this transaction, never from the candidate that attempted the write.
    let stored = load_artifact(&mut tx, session, artifact.artifact_id()).await?;
    // SQLite only requires at least one storage source. The application layer
    // enforces the exclusive inline/external representation and content binding.
    stored
        .validate_identity()
        .map_err(|error| SessionStoreError::ToolArtifactStoredIntegrity {
            session_id: session.as_str().to_string(),
            artifact_id: stored.artifact_id.clone(),
            message: error.to_string(),
        })?;
    if !stored.matches(&artifact) {
        return Err(SessionStoreError::ToolArtifactConflict {
            session_id: session.as_str().to_string(),
            artifact_id: artifact.artifact_id().to_string(),
        });
    }
    let journal_seq = load_journal_seq(&mut tx, session, &stored).await?;
    tx.commit()
        .await
        .map_err(|error| SessionStoreError::commit_outcome_unknown("put tool artifact", error))?;
    Ok(CommittedArtifact::new(
        session.clone(),
        stored.into_reference(),
        journal_seq,
    ))
}

async fn load_artifact(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session: &SessionId,
    artifact_id: &str,
) -> Result<StoredArtifact, SessionStoreError> {
    let row = sqlx::query(
        r#"
        SELECT artifact_id, content_hash, bytes, preview, payload_text, storage_ref
        FROM tool_artifacts
        WHERE session_id = ? AND artifact_id = ?
        "#,
    )
    .bind(session.as_str())
    .bind(artifact_id)
    .fetch_one(&mut **tx)
    .await?;

    Ok(StoredArtifact {
        artifact_id: row
            .try_get("artifact_id")
            .map_err(|error| stored_decode_error(session, artifact_id, error))?,
        content_hash: row
            .try_get("content_hash")
            .map_err(|error| stored_decode_error(session, artifact_id, error))?,
        bytes: row
            .try_get("bytes")
            .map_err(|error| stored_decode_error(session, artifact_id, error))?,
        preview: row
            .try_get("preview")
            .map_err(|error| stored_decode_error(session, artifact_id, error))?,
        payload_text: row
            .try_get("payload_text")
            .map_err(|error| stored_decode_error(session, artifact_id, error))?,
        storage_ref: row
            .try_get("storage_ref")
            .map_err(|error| stored_decode_error(session, artifact_id, error))?,
    })
}

fn stored_decode_error(
    session: &SessionId,
    artifact_id: &str,
    error: sqlx::Error,
) -> SessionStoreError {
    SessionStoreError::ToolArtifactStoredIntegrity {
        session_id: session.as_str().to_string(),
        artifact_id: artifact_id.to_string(),
        message: error.to_string(),
    }
}

struct StoredArtifact {
    artifact_id: String,
    content_hash: String,
    bytes: i64,
    preview: String,
    payload_text: Option<String>,
    storage_ref: Option<String>,
}

impl StoredArtifact {
    fn validate_identity(&self) -> Result<(), SessionStoreError> {
        validate_artifact_id(&self.artifact_id, &self.content_hash)?;
        let source = artifact_source(self.payload_text.as_deref(), self.storage_ref.as_deref())?;
        validate_artifact_content(&self.content_hash, self.bytes, source)
    }

    fn matches(&self, candidate: &NewToolArtifact) -> bool {
        self.artifact_id == candidate.artifact_id()
            && self.content_hash == candidate.content_hash()
            && self.bytes == candidate.bytes()
            && self.preview == candidate.preview()
            && self.payload_text.as_deref() == candidate.payload_text()
            && self.storage_ref.as_deref() == candidate.storage_ref()
    }

    fn into_reference(self) -> ToolArtifactRef {
        ToolArtifactRef {
            artifact_id: self.artifact_id,
            content_hash: self.content_hash,
            bytes: self.bytes,
            preview: self.preview,
        }
    }
}
