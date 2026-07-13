use crate::session_store::{Seq, SessionId, SessionStoreError};

pub(super) async fn compare_and_lock(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session: &SessionId,
    expected: Seq,
) -> Result<(), SessionStoreError> {
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

    let actual: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(seq), 0) FROM journal_entries WHERE session_id = ?",
    )
    .bind(session.as_str())
    .fetch_one(&mut **tx)
    .await?;
    Err(SessionStoreError::JournalHeadConflict {
        expected: expected.get(),
        actual,
    })
}
