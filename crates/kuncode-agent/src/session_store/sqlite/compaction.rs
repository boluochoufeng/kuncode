use sqlx::SqlitePool;

use crate::session_store::{
    CommittedCompaction, JournalKind, NewCompactionCommit, Seq, SessionStoreError,
};

use super::{checkpoint, head::compare_and_lock, next_seq, timestamp, touch_session};

pub(super) async fn commit(
    pool: &SqlitePool,
    commit: NewCompactionCommit,
) -> Result<CommittedCompaction, SessionStoreError> {
    validate_commit(&commit)?;
    let mut tx = pool.begin().await?;
    compare_and_lock(&mut tx, &commit.session_id, commit.expected_journal_head).await?;
    let now = timestamp();
    let compaction_seq = next_seq(&mut tx, &commit.session_id).await?;
    let payload = serde_json::json!({
        "schema_version": 1,
        "input_hash": commit.event.input_hash(),
        "source_seq_start": commit.event.source_seq_start().get(),
        "source_seq_end": commit.event.source_seq_end().get(),
    });
    sqlx::query(
        r#"
        INSERT INTO journal_entries (session_id, seq, kind, payload_json, created_at)
        VALUES (?, ?, ?, ?, ?)
        "#,
    )
    .bind(commit.session_id.as_str())
    .bind(compaction_seq.get())
    .bind(JournalKind::Compaction.as_str())
    .bind(serde_json::to_string(&payload)?)
    .bind(&now)
    .execute(&mut *tx)
    .await?;

    let checkpoint_seq = next_seq(&mut tx, &commit.session_id).await?;
    checkpoint::insert(&mut tx, &commit.checkpoint, checkpoint_seq, &now).await?;
    touch_session(&mut tx, &commit.session_id, &now).await?;
    tx.commit().await?;
    Ok(CommittedCompaction::new(
        commit.session_id,
        compaction_seq,
        checkpoint_seq,
    ))
}

fn validate_commit(commit: &NewCompactionCommit) -> Result<(), SessionStoreError> {
    if commit.session_id != commit.checkpoint.session_id {
        return Err(invalid_compaction(
            "commit and checkpoint must target the same session",
        ));
    }
    if commit.event.input_hash().trim().is_empty() {
        return Err(invalid_compaction("input_hash must not be empty"));
    }
    if commit.checkpoint.covers_through_seq != commit.expected_journal_head {
        return Err(invalid_compaction(
            "checkpoint must cover the journal head used to build the candidate",
        ));
    }
    let start = commit.event.source_seq_start();
    let end = commit.event.source_seq_end();
    if start <= Seq::ZERO || start > end || end > commit.expected_journal_head {
        return Err(invalid_compaction(
            "source seq range must be positive, ordered, and durable",
        ));
    }
    if commit.checkpoint.source_seq_start != Some(start)
        || commit.checkpoint.source_seq_end != Some(end)
    {
        return Err(invalid_compaction(
            "event and checkpoint source seq ranges must match",
        ));
    }
    Ok(())
}

fn invalid_compaction(message: impl Into<String>) -> SessionStoreError {
    SessionStoreError::InvalidCompaction(message.into())
}
