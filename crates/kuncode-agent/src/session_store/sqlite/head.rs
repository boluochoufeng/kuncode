//! Journal-head reads and compare-and-lock coordination for SQLite writers.

use sqlx::Row;

use crate::session_store::{Seq, SessionId, SessionStoreError};

pub(super) async fn read(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session: &SessionId,
) -> Result<Seq, SessionStoreError> {
    let row = sqlx::query("SELECT COALESCE(MAX(seq), 0) FROM journal_entries WHERE session_id = ?")
        .bind(session.as_str())
        .fetch_one(&mut **tx)
        .await?;
    let value: i64 = row
        .try_get(0)
        .map_err(|error| stored_integrity(session, error.to_string()))?;
    if value < 0 {
        return Err(stored_integrity(
            session,
            format!("journal head must be non-negative, found {value}"),
        ));
    }
    Ok(Seq::new(value))
}

pub(super) async fn compare_and_lock(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session: &SessionId,
    expected: Seq,
) -> Result<(), SessionStoreError> {
    // This no-op update intentionally acquires SQLite's writer lock while the
    // head predicate is evaluated. The transaction retains that lock, preventing
    // an append between the comparison and the caller's dependent writes.
    let result = sqlx::query(
        r#"
        UPDATE sessions
        SET updated_at = updated_at
        WHERE id = ?
          AND ? = (
            SELECT COALESCE(MAX(seq), 0)
            FROM journal_entries
            WHERE session_id = ?
          )
        "#,
    )
    .bind(session.as_str())
    .bind(expected.get())
    .bind(session.as_str())
    .execute(&mut **tx)
    .await?;
    if result.rows_affected() == 1 {
        return Ok(());
    }

    let actual = read(tx, session).await?.get();
    Err(SessionStoreError::JournalHeadConflict {
        expected: expected.get(),
        actual,
    })
}

fn stored_integrity(session: &SessionId, message: String) -> SessionStoreError {
    SessionStoreError::JournalStoredIntegrity {
        session_id: session.as_str().to_string(),
        message,
    }
}
