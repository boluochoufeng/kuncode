//! Atomic persistence of compaction facts and checkpoint material for future resume.
//!
//! Version 1 commits these records but does not read them to restore runtime context.

use sqlx::SqlitePool;

use crate::session_store::{
    CommittedCompaction, JournalKind, NewCompactionCommit, Seq, SessionStoreError,
    active_messages_sha256,
};

use crate::session_store::hash::is_canonical_sha256;

use super::{checkpoint, head::compare_and_lock, next_seq, timestamp, touch_session};

pub(super) async fn commit(
    pool: &SqlitePool,
    commit: NewCompactionCommit,
) -> Result<CommittedCompaction, SessionStoreError> {
    validate_commit(&commit)?;
    let mut tx = pool.begin().await?;
    // The writer lock preserves the candidate's source head until the compaction
    // fact, checkpoint row, and checkpoint reference become atomically visible.
    compare_and_lock(&mut tx, &commit.session_id, commit.expected_journal_head).await?;
    let now = timestamp();
    let compaction_seq = next_seq(&mut tx, &commit.session_id).await?;
    let payload = serde_json::json!({
        "schema_version": 2,
        "input_hash": commit.event.input_hash(),
        "output_hash": commit.event.output_hash(),
        "source_seq_start": commit.event.source_seq_start().get(),
        "source_seq_end": commit.event.source_seq_end().get(),
        "reason": commit.event.metadata().reason().as_str(),
        "passes": commit
            .event
            .metadata()
            .passes()
            .iter()
            .map(|pass| pass.as_str())
            .collect::<Vec<_>>(),
        "summary": commit.event.metadata().summary_json(),
        "model": commit.event.metadata().model(),
        "token_usage": commit.event.metadata().token_usage_json(),
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
    tx.commit()
        .await
        .map_err(|error| SessionStoreError::commit_outcome_unknown("commit compaction", error))?;
    Ok(CommittedCompaction::new(
        commit.session_id,
        compaction_seq,
        checkpoint_seq,
        commit.event.output_hash().to_owned(),
    ))
}

fn validate_commit(commit: &NewCompactionCommit) -> Result<(), SessionStoreError> {
    if commit.session_id != commit.checkpoint.session_id {
        return Err(invalid_compaction(
            "commit and checkpoint must target the same session",
        ));
    }
    if !is_canonical_sha256(commit.event.input_hash()) {
        return Err(invalid_compaction(
            "input_hash must be 64 lowercase hexadecimal characters",
        ));
    }
    if !is_canonical_sha256(commit.event.output_hash()) {
        return Err(invalid_compaction(
            "output_hash must be 64 lowercase hexadecimal characters",
        ));
    }
    let checkpoint_hash = active_messages_sha256(&commit.checkpoint.active_messages)?;
    if commit.event.output_hash() != checkpoint_hash {
        return Err(invalid_compaction(
            "output_hash must match checkpoint active_messages",
        ));
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
    let passes = commit.event.metadata().passes();
    // `AtomicCommit` records the persistence boundary, so it must appear once
    // and after every transformation that contributed to the committed output.
    if passes.last() != Some(&crate::session_store::CompactionPassKind::AtomicCommit)
        || passes
            .iter()
            .filter(|pass| **pass == crate::session_store::CompactionPassKind::AtomicCommit)
            .count()
            != 1
    {
        return Err(invalid_compaction("atomic_commit must be the final pass"));
    }
    let semantic = passes.contains(&crate::session_store::CompactionPassKind::SemanticSummary);
    match (
        semantic,
        commit.event.metadata().summary_json(),
        commit.event.metadata().model(),
        commit.event.metadata().token_usage_json(),
    ) {
        (false, None, None, None) => {}
        (true, Some(summary), Some(model), Some(usage)) => {
            if commit.checkpoint.summary_json.as_ref() != Some(summary)
                || commit.checkpoint.model.as_deref() != Some(model)
                || commit.checkpoint.token_usage_json.as_ref() != Some(usage)
                || commit.checkpoint.source_seq_start != Some(start)
                || commit.checkpoint.source_seq_end != Some(end)
            {
                return Err(invalid_compaction(
                    "semantic event metadata must match the checkpoint",
                ));
            }
        }
        _ => {
            return Err(invalid_compaction(
                "semantic_summary pass and generated summary metadata must agree",
            ));
        }
    }
    Ok(())
}

fn invalid_compaction(message: impl Into<String>) -> SessionStoreError {
    SessionStoreError::InvalidCompaction(message.into())
}
