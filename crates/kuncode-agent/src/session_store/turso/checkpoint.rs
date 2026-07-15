//! Atomic checkpoint persistence reserved for future active-context resume.
//!
//! Version 1 writes these records, but the runtime does not read them to resume
//! sessions yet.

use ::turso::{Connection, Row, transaction::TransactionBehavior};
use tokio::sync::Mutex;

use crate::session_store::{
    Checkpoint, JournalKind, NewCheckpoint, Seq, SessionId, SessionStoreError, dto,
};

use super::{next_seq, timestamp, touch_session};

pub(super) async fn latest(
    connection: &Mutex<Connection>,
    session: &SessionId,
) -> Result<Option<Checkpoint>, SessionStoreError> {
    let connection = connection.lock().await;
    let mut rows = connection
        .query(
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
            WHERE session_id = ?1
            ORDER BY checkpoint_seq DESC
            LIMIT 1
            "#,
            [session.as_str()],
        )
        .await?;

    rows.next()
        .await?
        .as_ref()
        .map(row_to_checkpoint)
        .transpose()
}

pub(super) async fn write(
    connection: &Mutex<Connection>,
    checkpoint: NewCheckpoint,
) -> Result<Seq, SessionStoreError> {
    let mut connection = connection.lock().await;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await?;
    let outcome = async {
        let seq = next_seq(&tx, &checkpoint.session_id).await?;
        let now = timestamp();
        insert(&tx, &checkpoint, seq, &now).await?;
        touch_session(&tx, &checkpoint.session_id, &now).await?;
        Ok(seq)
    }
    .await;
    match outcome {
        Ok(seq) => {
            tx.commit().await.map_err(|error| {
                SessionStoreError::commit_outcome_unknown("write checkpoint", error)
            })?;
            Ok(seq)
        }
        Err(error) => {
            tx.rollback().await?;
            Err(error)
        }
    }
}

pub(super) async fn insert(
    connection: &Connection,
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

    // Sharing one journal coordinate and transaction makes the checkpoint row
    // and `CheckpointRef` an atomic durable unit for a future resume path.
    connection
        .execute(
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
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            "#,
            ::turso::params![
                checkpoint.session_id.as_str(),
                seq.get(),
                checkpoint.covers_through_seq.get(),
                checkpoint.source_seq_start.map(Seq::get),
                checkpoint.source_seq_end.map(Seq::get),
                active_messages,
                summary.as_deref(),
                checkpoint.model.as_deref(),
                token_usage.as_deref(),
                now,
            ],
        )
        .await?;

    let payload = serde_json::json!({
        "schema_version": 1,
        "checkpoint_seq": seq.get(),
        "covers_through_seq": checkpoint.covers_through_seq.get()
    });
    connection
        .execute(
            r#"
            INSERT INTO journal_entries (session_id, seq, kind, payload_json, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5)
            "#,
            ::turso::params![
                checkpoint.session_id.as_str(),
                seq.get(),
                JournalKind::CheckpointRef.as_str(),
                serde_json::to_string(&payload)?,
                now,
            ],
        )
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

    // Generated summaries require complete provenance. Partial metadata cannot
    // prove which durable source range and model produced the active messages.
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

fn row_to_checkpoint(row: &Row) -> Result<Checkpoint, SessionStoreError> {
    let active_messages = row.get::<String>(4)?;
    let summary = row.get::<Option<String>>(5)?;
    let token_usage = row.get::<Option<String>>(7)?;
    Ok(Checkpoint {
        checkpoint_seq: Seq::new(row.get(0)?),
        covers_through_seq: Seq::new(row.get(1)?),
        source_seq_start: row.get::<Option<i64>>(2)?.map(Seq::new),
        source_seq_end: row.get::<Option<i64>>(3)?.map(Seq::new),
        active_messages: dto::messages_from_str(&active_messages)?,
        summary_json: summary
            .map(|json| serde_json::from_str(&json))
            .transpose()?,
        model: row.get(6)?,
        token_usage_json: token_usage
            .map(|json| serde_json::from_str(&json))
            .transpose()?,
    })
}
