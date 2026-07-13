use sqlx::{Row, SqlitePool};

use super::{head::compare_and_lock, next_seq, timestamp, touch_session};
use crate::session_store::{
    CommittedArtifact, JournalKind, NewToolArtifact, Seq, SessionId, SessionStoreError,
    ToolArtifactRef,
    artifact::{artifact_source, validate_artifact_content, validate_artifact_id},
};

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

    let stored = load_artifact(&mut tx, session, artifact.artifact_id()).await?;
    stored.validate_identity()?;
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

async fn load_journal_seq(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session: &SessionId,
    artifact: &StoredArtifact,
) -> Result<Seq, SessionStoreError> {
    let row = sqlx::query(
        r#"
        SELECT seq, payload_json
        FROM journal_entries
        WHERE session_id = ?
          AND kind = ?
          AND json_extract(payload_json, '$.artifact_id') = ?
        ORDER BY seq ASC
        LIMIT 1
        "#,
    )
    .bind(session.as_str())
    .bind(JournalKind::ToolArtifact.as_str())
    .bind(&artifact.artifact_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Err(SessionStoreError::ToolArtifactJournalMissing {
            session_id: session.as_str().to_string(),
            artifact_id: artifact.artifact_id.clone(),
        });
    };
    let seq: i64 = row.try_get("seq")?;
    let payload_text: String = row.try_get("payload_json")?;
    let payload: serde_json::Value = serde_json::from_str(&payload_text)?;
    if payload
        .get("content_hash")
        .and_then(serde_json::Value::as_str)
        != Some(artifact.content_hash.as_str())
        || payload.get("bytes").and_then(serde_json::Value::as_i64) != Some(artifact.bytes)
        || payload.get("preview").and_then(serde_json::Value::as_str)
            != Some(artifact.preview.as_str())
    {
        return Err(SessionStoreError::ToolArtifactJournalMismatch {
            session_id: session.as_str().to_string(),
            artifact_id: artifact.artifact_id.clone(),
        });
    }
    Ok(Seq::new(seq))
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
        artifact_id: row.try_get("artifact_id")?,
        content_hash: row.try_get("content_hash")?,
        bytes: row.try_get("bytes")?,
        preview: row.try_get("preview")?,
        payload_text: row.try_get("payload_text")?,
        storage_ref: row.try_get("storage_ref")?,
    })
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
