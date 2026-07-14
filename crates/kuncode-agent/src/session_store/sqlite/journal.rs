//! Bounded, transactionally consistent journal snapshots for integrity audits.
//!
//! The head and requested facts share one SQLite read snapshot, preventing an
//! audit from combining an old head with rows committed by a concurrent writer.

use std::collections::BTreeSet;

use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};

use crate::session_store::{JournalSnapshot, Seq, SessionId, SessionStoreError};

// Leave ample room under SQLite's host-parameter limit after the session bind.
const SNAPSHOT_QUERY_CHUNK: usize = 256;

pub(super) async fn snapshot(
    pool: &SqlitePool,
    session: &SessionId,
    seqs: &[Seq],
) -> Result<JournalSnapshot, SessionStoreError> {
    let mut tx = pool.begin().await?;
    // The first read establishes SQLite's transaction snapshot before any
    // chunked row query can overlap with concurrent appends.
    let head = super::head::read(&mut tx, session).await?;
    let requested = seqs.iter().copied().collect::<BTreeSet<_>>();
    let requested = requested.into_iter().collect::<Vec<_>>();
    let mut entries = Vec::with_capacity(requested.len());
    for chunk in requested.chunks(SNAPSHOT_QUERY_CHUNK) {
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT seq, kind, payload_json FROM journal_entries WHERE session_id = ",
        );
        query.push_bind(session.as_str());
        query.push(" AND seq IN (");
        let mut separated = query.separated(", ");
        for seq in chunk {
            separated.push_bind(seq.get());
        }
        separated.push_unseparated(") ORDER BY seq ASC");
        let rows = query.build().fetch_all(&mut *tx).await?;
        for row in rows {
            let seq = row
                .try_get("seq")
                .map(Seq::new)
                .map_err(|error| stored_integrity(session, error))?;
            let kind = row
                .try_get("kind")
                .map_err(|error| stored_integrity(session, error))?;
            let payload: Vec<u8> = row
                .try_get("payload_json")
                .map_err(|error| stored_integrity(session, error))?;
            let payload_json = serde_json::from_slice(&payload).map_err(|error| {
                SessionStoreError::JournalStoredIntegrity {
                    session_id: session.as_str().to_string(),
                    message: error.to_string(),
                }
            })?;
            entries.push(crate::session_store::JournalEntry {
                seq,
                kind,
                payload_json,
            });
        }
    }
    tx.commit().await?;
    Ok(JournalSnapshot::new(head, entries))
}

fn stored_integrity(session: &SessionId, error: sqlx::Error) -> SessionStoreError {
    SessionStoreError::JournalStoredIntegrity {
        session_id: session.as_str().to_string(),
        message: error.to_string(),
    }
}
