//! Fresh Turso schema initialization for durable session storage.

use ::turso::{Connection, transaction::TransactionBehavior};

use crate::session_store::SessionStoreError;

pub(super) async fn initialize(connection: &mut Connection) -> Result<(), SessionStoreError> {
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await?;
    let result = tx
        .execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS sessions (
              id                TEXT PRIMARY KEY,
              project_root      TEXT NOT NULL,
              project_slug      TEXT NOT NULL,
              title             TEXT,
              status            TEXT NOT NULL,
              parent_session_id TEXT,
              created_at        TEXT NOT NULL,
              updated_at        TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS journal_entries (
              session_id   TEXT NOT NULL,
              seq          INTEGER NOT NULL,
              kind         TEXT NOT NULL,
              payload_json TEXT NOT NULL,
              created_at   TEXT NOT NULL,
              PRIMARY KEY (session_id, seq),
              FOREIGN KEY (session_id) REFERENCES sessions(id)
            );

            CREATE TABLE IF NOT EXISTS active_context_checkpoints (
              session_id           TEXT NOT NULL,
              checkpoint_seq       INTEGER NOT NULL,
              covers_through_seq   INTEGER NOT NULL,
              source_seq_start     INTEGER,
              source_seq_end       INTEGER,
              active_messages_json TEXT NOT NULL,
              summary_json         TEXT,
              model                TEXT,
              token_usage_json     TEXT,
              created_at           TEXT NOT NULL,
              PRIMARY KEY (session_id, checkpoint_seq),
              FOREIGN KEY (session_id) REFERENCES sessions(id)
            );

            -- SQL rejects source-less artifacts. Exact-one source semantics and
            -- content integrity remain application checks because the database
            -- cannot verify the referenced payload.
            CREATE TABLE IF NOT EXISTS tool_artifacts (
              session_id   TEXT NOT NULL,
              artifact_id  TEXT NOT NULL,
              content_hash TEXT NOT NULL,
              bytes        INTEGER NOT NULL,
              preview      TEXT NOT NULL,
              payload_text TEXT,
              storage_ref  TEXT,
              created_at   TEXT NOT NULL,
              PRIMARY KEY (session_id, artifact_id),
              FOREIGN KEY (session_id) REFERENCES sessions(id),
              CHECK (payload_text IS NOT NULL OR storage_ref IS NOT NULL)
            );

            CREATE INDEX IF NOT EXISTS journal_entries_by_session_desc
              ON journal_entries(session_id, seq DESC);

            CREATE INDEX IF NOT EXISTS sessions_by_project_updated
              ON sessions(project_root, updated_at DESC);

            CREATE INDEX IF NOT EXISTS tool_artifacts_by_hash
              ON tool_artifacts(content_hash);
            "#,
        )
        .await;
    match result {
        Ok(()) => tx.commit().await.map_err(Into::into),
        Err(error) => {
            tx.rollback().await?;
            Err(error.into())
        }
    }
}
