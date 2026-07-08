use sqlx::{Row, SqlitePool};

use super::{next_seq, timestamp, touch_session};
use crate::session_store::{
    JournalKind, NewToolArtifact, SessionId, SessionStoreError, ToolArtifactRef,
};

pub(super) async fn put(
    pool: &SqlitePool,
    session: &SessionId,
    artifact: NewToolArtifact,
) -> Result<ToolArtifactRef, SessionStoreError> {
    let mut tx = pool.begin().await?;
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

    let reference = load_ref(&mut tx, session, artifact.artifact_id()).await?;
    tx.commit().await?;
    Ok(reference)
}

async fn load_ref(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session: &SessionId,
    artifact_id: &str,
) -> Result<ToolArtifactRef, SessionStoreError> {
    let row = sqlx::query(
        r#"
        SELECT artifact_id, content_hash, bytes, preview
        FROM tool_artifacts
        WHERE session_id = ? AND artifact_id = ?
        "#,
    )
    .bind(session.as_str())
    .bind(artifact_id)
    .fetch_one(&mut **tx)
    .await?;

    Ok(ToolArtifactRef {
        artifact_id: row.try_get("artifact_id")?,
        content_hash: row.try_get("content_hash")?,
        bytes: row.try_get("bytes")?,
        preview: row.try_get("preview")?,
    })
}
