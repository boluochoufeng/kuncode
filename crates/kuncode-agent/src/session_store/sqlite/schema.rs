//! SQLite schema creation and literal-only compatibility migrations.

use sqlx::{AssertSqlSafe, Row, SqlitePool};

use crate::session_store::SessionStoreError;

pub(super) async fn migrate(pool: &SqlitePool) -> Result<(), SessionStoreError> {
    sqlx::query(
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
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS journal_entries (
          session_id   TEXT NOT NULL,
          seq          INTEGER NOT NULL,
          kind         TEXT NOT NULL,
          payload_json TEXT NOT NULL,
          created_at   TEXT NOT NULL,
          PRIMARY KEY (session_id, seq),
          FOREIGN KEY (session_id) REFERENCES sessions(id)
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
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
        )
        "#,
    )
    .execute(pool)
    .await?;
    add_column_if_missing(
        pool,
        "active_context_checkpoints",
        "source_seq_start",
        "source_seq_start INTEGER",
    )
    .await?;
    add_column_if_missing(
        pool,
        "active_context_checkpoints",
        "source_seq_end",
        "source_seq_end INTEGER",
    )
    .await?;
    add_column_if_missing(pool, "active_context_checkpoints", "model", "model TEXT").await?;
    add_column_if_missing(
        pool,
        "active_context_checkpoints",
        "token_usage_json",
        "token_usage_json TEXT",
    )
    .await?;

    // SQL rejects source-less artifacts. Exact-one source semantics and content
    // integrity remain application checks because SQLite cannot verify the payload.
    sqlx::query(
        r#"
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
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS journal_entries_by_session_desc
          ON journal_entries(session_id, seq DESC)
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS sessions_by_project_updated
          ON sessions(project_root, updated_at DESC)
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS tool_artifacts_by_hash
          ON tool_artifacts(content_hash)
        "#,
    )
    .execute(pool)
    .await?;

    Ok(())
}

async fn add_column_if_missing(
    pool: &SqlitePool,
    table: &'static str,
    column: &'static str,
    column_ddl: &'static str,
) -> Result<(), SessionStoreError> {
    // Every argument is a private compile-time literal. `AssertSqlSafe` must not
    // be generalized to runtime or user-controlled schema identifiers.
    let pragma = format!("PRAGMA table_info({table})");
    let rows = sqlx::query(AssertSqlSafe(pragma)).fetch_all(pool).await?;
    for row in rows {
        let name: String = row.try_get("name")?;
        if name == column {
            return Ok(());
        }
    }

    let alter = format!("ALTER TABLE {table} ADD COLUMN {column_ddl}");
    sqlx::query(AssertSqlSafe(alter)).execute(pool).await?;
    Ok(())
}
