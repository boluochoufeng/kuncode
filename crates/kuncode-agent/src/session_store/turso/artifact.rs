//! Idempotent artifact persistence tied to a strictly decoded journal fact.

use std::{collections::BTreeSet, fmt};

use ::turso::{Connection, transaction::TransactionBehavior};
use serde::de::{self, Deserialize, Deserializer, MapAccess, Visitor};
use tokio::sync::Mutex;

use super::{compare_and_lock, next_seq, timestamp, touch_session};
use crate::session_store::{
    CommittedArtifact, JournalKind, NewToolArtifact, Seq, SessionId, SessionStoreError,
    ToolArtifactRef,
    artifact::{artifact_source, validate_artifact_content, validate_artifact_id},
};

pub(super) async fn put(
    connection: &Mutex<Connection>,
    session: &SessionId,
    expected_journal_head: Seq,
    artifact: NewToolArtifact,
) -> Result<CommittedArtifact, SessionStoreError> {
    artifact.validate_identity()?;
    let mut connection = connection.lock().await;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await?;
    let outcome = async {
        compare_and_lock(&tx, session, expected_journal_head).await?;
        let now = timestamp();
        // The content-derived id makes retries idempotent, but `OR IGNORE` may also
        // conceal a conflicting row. Durable state is therefore revalidated below.
        let affected = tx
            .execute(
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
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                "#,
                ::turso::params![
                    session.as_str(),
                    artifact.artifact_id(),
                    artifact.content_hash(),
                    artifact.bytes(),
                    artifact.preview(),
                    artifact.payload_text(),
                    artifact.storage_ref(),
                    now.as_str(),
                ],
            )
            .await?;

        if affected > 0 {
            let seq = next_seq(&tx, session).await?;
            let payload_json = serde_json::json!({
                "artifact_id": artifact.artifact_id(),
                "content_hash": artifact.content_hash(),
                "bytes": artifact.bytes(),
                "preview": artifact.preview(),
            });
            tx.execute(
                r#"
                INSERT INTO journal_entries (session_id, seq, kind, payload_json, created_at)
                VALUES (?1, ?2, ?3, ?4, ?5)
                "#,
                ::turso::params![
                    session.as_str(),
                    seq.get(),
                    JournalKind::ToolArtifact.as_str(),
                    serde_json::to_string(&payload_json)?,
                    now.as_str(),
                ],
            )
            .await?;
            touch_session(&tx, session, &now).await?;
        }

        // Receipts are derived only from the row and journal fact observed inside
        // this transaction, never from the candidate that attempted the write.
        let stored = load_artifact(&tx, session, artifact.artifact_id()).await?;
        stored.validate_identity().map_err(|error| {
            SessionStoreError::ToolArtifactStoredIntegrity {
                session_id: session.as_str().to_string(),
                artifact_id: stored.artifact_id.clone(),
                message: error.to_string(),
            }
        })?;
        if !stored.matches(&artifact) {
            return Err(SessionStoreError::ToolArtifactConflict {
                session_id: session.as_str().to_string(),
                artifact_id: artifact.artifact_id().to_string(),
            });
        }
        let journal_seq = load_journal_seq(&tx, session, &stored).await?;
        Ok(CommittedArtifact::new(
            session.clone(),
            stored.into_reference(),
            journal_seq,
        ))
    }
    .await;

    match outcome {
        Ok(committed) => {
            tx.commit().await.map_err(|error| {
                SessionStoreError::commit_outcome_unknown("put tool artifact", error)
            })?;
            Ok(committed)
        }
        Err(error) => {
            tx.rollback().await?;
            Err(error)
        }
    }
}

async fn load_artifact(
    connection: &Connection,
    session: &SessionId,
    artifact_id: &str,
) -> Result<StoredArtifact, SessionStoreError> {
    let mut rows = connection
        .query(
            r#"
            SELECT artifact_id, content_hash, bytes, preview, payload_text, storage_ref
            FROM tool_artifacts
            WHERE session_id = ?1 AND artifact_id = ?2
            "#,
            (session.as_str(), artifact_id),
        )
        .await?;
    let row = rows
        .next()
        .await?
        .ok_or(::turso::Error::QueryReturnedNoRows)?;

    Ok(StoredArtifact {
        artifact_id: row
            .get(0)
            .map_err(|error| stored_decode_error(session, artifact_id, error))?,
        content_hash: row
            .get(1)
            .map_err(|error| stored_decode_error(session, artifact_id, error))?,
        bytes: row
            .get(2)
            .map_err(|error| stored_decode_error(session, artifact_id, error))?,
        preview: row
            .get(3)
            .map_err(|error| stored_decode_error(session, artifact_id, error))?,
        payload_text: row
            .get(4)
            .map_err(|error| stored_decode_error(session, artifact_id, error))?,
        storage_ref: row
            .get(5)
            .map_err(|error| stored_decode_error(session, artifact_id, error))?,
    })
}

fn stored_decode_error(
    session: &SessionId,
    artifact_id: &str,
    error: ::turso::Error,
) -> SessionStoreError {
    SessionStoreError::ToolArtifactStoredIntegrity {
        session_id: session.as_str().to_string(),
        artifact_id: artifact_id.to_string(),
        message: error.to_string(),
    }
}

struct StoredArtifact {
    artifact_id: String,
    content_hash: String,
    bytes: i64,
    preview: String,
    payload_text: Option<String>,
    storage_ref: Option<String>,
}

impl StoredArtifact {
    fn validate_identity(&self) -> Result<(), SessionStoreError> {
        validate_artifact_id(&self.artifact_id, &self.content_hash)?;
        let source = artifact_source(self.payload_text.as_deref(), self.storage_ref.as_deref())?;
        validate_artifact_content(&self.content_hash, self.bytes, source)
    }

    fn matches(&self, candidate: &NewToolArtifact) -> bool {
        self.artifact_id == candidate.artifact_id()
            && self.content_hash == candidate.content_hash()
            && self.bytes == candidate.bytes()
            && self.preview == candidate.preview()
            && self.payload_text.as_deref() == candidate.payload_text()
            && self.storage_ref.as_deref() == candidate.storage_ref()
    }

    fn into_reference(self) -> ToolArtifactRef {
        ToolArtifactRef {
            artifact_id: self.artifact_id,
            content_hash: self.content_hash,
            bytes: self.bytes,
            preview: self.preview,
        }
    }
}

async fn load_journal_seq(
    connection: &Connection,
    session: &SessionId,
    artifact: &StoredArtifact,
) -> Result<Seq, SessionStoreError> {
    let mut rows = connection
        .query(
            "SELECT seq, payload_json FROM journal_entries \
             WHERE session_id = ?1 AND kind = ?2 ORDER BY seq ASC",
            (session.as_str(), JournalKind::ToolArtifact.as_str()),
        )
        .await?;
    let mut matched = None;
    // Decode every artifact fact rather than selecting the requested id in SQL,
    // so a malformed fact elsewhere in the stream cannot be hidden by selection.
    while let Some(row) = rows.next().await? {
        let seq = row
            .get(0)
            .map(Seq::new)
            .map_err(|error| journal_integrity(session, error.to_string()))?;
        if seq <= Seq::ZERO {
            return Err(journal_integrity(
                session,
                format!(
                    "artifact journal sequence must be positive, found {}",
                    seq.get()
                ),
            ));
        }
        let payload: String = row
            .get(1)
            .map_err(|error| journal_integrity(session, error.to_string()))?;
        let fact: ArtifactJournalFact = serde_json::from_str(&payload)
            .map_err(|error| journal_integrity(session, error.to_string()))?;
        validate_artifact_id(&fact.artifact_id, &fact.content_hash)
            .map_err(|error| journal_integrity(session, error.to_string()))?;
        if fact.artifact_id == artifact.artifact_id && matched.replace((seq, fact)).is_some() {
            return Err(journal_integrity(
                session,
                "duplicate artifact journal fact",
            ));
        }
    }
    let Some((seq, fact)) = matched else {
        return Err(SessionStoreError::ToolArtifactJournalMissing {
            session_id: session.as_str().to_string(),
            artifact_id: artifact.artifact_id.clone(),
        });
    };
    if fact.content_hash != artifact.content_hash
        || fact.bytes != artifact.bytes
        || fact.preview != artifact.preview
    {
        return Err(SessionStoreError::ToolArtifactJournalMismatch {
            session_id: session.as_str().to_string(),
            artifact_id: artifact.artifact_id.clone(),
        });
    }
    Ok(seq)
}

fn journal_integrity(session: &SessionId, message: impl Into<String>) -> SessionStoreError {
    SessionStoreError::ToolArtifactJournalIntegrity {
        session_id: session.as_str().to_string(),
        message: message.into(),
    }
}

struct ArtifactJournalFact {
    artifact_id: String,
    content_hash: String,
    bytes: i64,
    preview: String,
}

impl<'de> Deserialize<'de> for ArtifactJournalFact {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(ArtifactJournalVisitor)
    }
}

struct ArtifactJournalVisitor;

impl<'de> Visitor<'de> for ArtifactJournalVisitor {
    type Value = ArtifactJournalFact;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a strict tool-artifact journal object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut seen = BTreeSet::new();
        let mut artifact_id = None;
        let mut content_hash = None;
        let mut bytes = None;
        let mut preview = None;
        while let Some(key) = map.next_key::<String>()? {
            // Duplicate or unknown keys make the serialized proof ambiguous even
            // if one interpretation would happen to match the artifact row.
            if !seen.insert(key.clone()) {
                return Err(de::Error::custom(format!("duplicate field `{key}`")));
            }
            match key.as_str() {
                "artifact_id" => artifact_id = Some(map.next_value()?),
                "content_hash" => content_hash = Some(map.next_value()?),
                "bytes" => bytes = Some(map.next_value()?),
                "preview" => preview = Some(map.next_value()?),
                _ => return Err(de::Error::unknown_field(&key, ARTIFACT_JOURNAL_FIELDS)),
            }
        }
        Ok(ArtifactJournalFact {
            artifact_id: artifact_id.ok_or_else(|| de::Error::missing_field("artifact_id"))?,
            content_hash: content_hash.ok_or_else(|| de::Error::missing_field("content_hash"))?,
            bytes: bytes.ok_or_else(|| de::Error::missing_field("bytes"))?,
            preview: preview.ok_or_else(|| de::Error::missing_field("preview"))?,
        })
    }
}

const ARTIFACT_JOURNAL_FIELDS: &[&str] = &["artifact_id", "content_hash", "bytes", "preview"];
