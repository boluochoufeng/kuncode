use sqlx::{Row, SqlitePool};

use crate::session_store::{
    Checkpoint, JournalKind, NewCheckpoint, Seq, SessionId, SessionStoreError, dto,
};

use super::{next_seq, timestamp, touch_session};

pub(super) async fn latest(
    pool: &SqlitePool,
    session: &SessionId,
) -> Result<Option<Checkpoint>, SessionStoreError> {
    let row = sqlx::query(
        r#"
        SELECT
          checkpoint_seq,
          covers_through_seq,
          source_seq_start,
          source_seq_end,
          active_messages_json,
          summary_json,
          model,
          token_usage_json
        FROM active_context_checkpoints
        WHERE session_id = ?
        ORDER BY checkpoint_seq DESC
        LIMIT 1
        "#,
    )
    .bind(session.as_str())
    .fetch_optional(pool)
    .await?;

    row.map(row_to_checkpoint).transpose()
}

pub(super) async fn write(
    pool: &SqlitePool,
    checkpoint: NewCheckpoint,
) -> Result<Seq, SessionStoreError> {
    let mut tx = pool.begin().await?;
    let seq = next_seq(&mut tx, &checkpoint.session_id).await?;
    let now = timestamp();
    insert(&mut tx, &checkpoint, seq, &now).await?;
    touch_session(&mut tx, &checkpoint.session_id, &now).await?;
    tx.commit()
        .await
        .map_err(|error| SessionStoreError::commit_outcome_unknown("write checkpoint", error))?;
    Ok(seq)
}

pub(super) async fn insert(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    checkpoint: &NewCheckpoint,
    seq: Seq,
    now: &str,
) -> Result<(), SessionStoreError> {
    validate_checkpoint(checkpoint, seq)?;
    let active_messages = dto::messages_to_string(&checkpoint.active_messages)?;
    let summary = checkpoint
        .summary_json
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    let token_usage = checkpoint
        .token_usage_json
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;

    sqlx::query(
        r#"
        INSERT INTO active_context_checkpoints (
          session_id,
          checkpoint_seq,
          covers_through_seq,
          source_seq_start,
          source_seq_end,
          active_messages_json,
          summary_json,
          model,
          token_usage_json,
          created_at
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(checkpoint.session_id.as_str())
    .bind(seq.get())
    .bind(checkpoint.covers_through_seq.get())
    .bind(checkpoint.source_seq_start.map(Seq::get))
    .bind(checkpoint.source_seq_end.map(Seq::get))
    .bind(active_messages)
    .bind(summary)
    .bind(&checkpoint.model)
    .bind(token_usage)
    .bind(now)
    .execute(&mut **tx)
    .await?;

    let payload = serde_json::json!({
        "schema_version": 1,
        "checkpoint_seq": seq.get(),
        "covers_through_seq": checkpoint.covers_through_seq.get()
    });
    sqlx::query(
        r#"
        INSERT INTO journal_entries (session_id, seq, kind, payload_json, created_at)
        VALUES (?, ?, ?, ?, ?)
        "#,
    )
    .bind(checkpoint.session_id.as_str())
    .bind(seq.get())
    .bind(JournalKind::CheckpointRef.as_str())
    .bind(serde_json::to_string(&payload)?)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn validate_checkpoint(
    checkpoint: &NewCheckpoint,
    checkpoint_seq: Seq,
) -> Result<(), SessionStoreError> {
    let covers = checkpoint.covers_through_seq.get();
    if covers < Seq::ZERO.get() {
        return Err(invalid_checkpoint(
            "covers_through_seq must not be negative",
        ));
    }
    if covers >= checkpoint_seq.get() {
        return Err(invalid_checkpoint(format!(
            "covers_through_seq {covers} is beyond committed journal seq {}",
            checkpoint_seq.get() - 1
        )));
    }

    match (
        checkpoint.summary_json.as_ref(),
        checkpoint.source_seq_start,
        checkpoint.source_seq_end,
        checkpoint.model.as_deref(),
        checkpoint.token_usage_json.as_ref(),
    ) {
        (None, None, None, None, None) => Ok(()),
        (Some(_), Some(start), Some(end), Some(model), Some(_)) if !model.is_empty() => {
            validate_source_range(start, end, checkpoint.covers_through_seq)
        }
        (None, _, _, _, _) => Err(invalid_checkpoint(
            "deterministic checkpoint cannot carry summary provenance",
        )),
        (Some(_), _, _, _, _) => Err(invalid_checkpoint(
            "summary checkpoint requires source range, model, and token usage",
        )),
    }
}

fn validate_source_range(start: Seq, end: Seq, covers: Seq) -> Result<(), SessionStoreError> {
    if start <= Seq::ZERO || end <= Seq::ZERO {
        return Err(invalid_checkpoint("source seq range must be positive"));
    }
    if start > end {
        return Err(invalid_checkpoint(format!(
            "source_seq_start {} is greater than source_seq_end {}",
            start.get(),
            end.get()
        )));
    }
    if end > covers {
        return Err(invalid_checkpoint(format!(
            "source_seq_end {} is beyond covers_through_seq {}",
            end.get(),
            covers.get()
        )));
    }
    Ok(())
}

fn invalid_checkpoint(message: impl Into<String>) -> SessionStoreError {
    SessionStoreError::InvalidCheckpoint(message.into())
}

fn row_to_checkpoint(row: sqlx::sqlite::SqliteRow) -> Result<Checkpoint, SessionStoreError> {
    let active_messages: String = row.try_get("active_messages_json")?;
    let summary: Option<String> = row.try_get("summary_json")?;
    let token_usage: Option<String> = row.try_get("token_usage_json")?;
    Ok(Checkpoint {
        checkpoint_seq: Seq::new(row.try_get("checkpoint_seq")?),
        covers_through_seq: Seq::new(row.try_get("covers_through_seq")?),
        source_seq_start: row
            .try_get::<Option<i64>, _>("source_seq_start")?
            .map(Seq::new),
        source_seq_end: row
            .try_get::<Option<i64>, _>("source_seq_end")?
            .map(Seq::new),
        active_messages: dto::messages_from_str(&active_messages)?,
        summary_json: summary
            .map(|json| serde_json::from_str(&json))
            .transpose()?,
        model: row.try_get("model")?,
        token_usage_json: token_usage
            .map(|json| serde_json::from_str(&json))
            .transpose()?,
    })
}
