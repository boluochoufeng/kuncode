//! Strict artifact journal-fact decoding used to authorize durable receipts.
//!
//! The decoder treats malformed or ambiguous history as an integrity failure,
//! because a receipt is valid only when exactly one durable fact proves it.

use std::{collections::BTreeSet, fmt};

use serde::de::{self, Deserialize, Deserializer, MapAccess, Visitor};
use sqlx::Row;

use super::StoredArtifact;
use crate::session_store::{JournalKind, Seq, SessionId, SessionStoreError};

pub(super) async fn load_journal_seq(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session: &SessionId,
    artifact: &StoredArtifact,
) -> Result<Seq, SessionStoreError> {
    let rows = sqlx::query(
        "SELECT seq, payload_json FROM journal_entries \
         WHERE session_id = ? AND kind = ? ORDER BY seq ASC",
    )
    .bind(session.as_str())
    .bind(JournalKind::ToolArtifact.as_str())
    .fetch_all(&mut **tx)
    .await?;
    let mut matched = None;
    // Decode every artifact fact rather than selecting the requested id in SQL,
    // so a malformed fact elsewhere in the stream cannot be hidden by selection.
    for row in rows {
        let seq = row
            .try_get("seq")
            .map(Seq::new)
            .map_err(|error| integrity(session, error.to_string()))?;
        if seq <= Seq::ZERO {
            return Err(integrity(
                session,
                format!(
                    "artifact journal sequence must be positive, found {}",
                    seq.get()
                ),
            ));
        }
        let payload: Vec<u8> = row
            .try_get("payload_json")
            .map_err(|error| integrity(session, error.to_string()))?;
        let fact: ArtifactJournalFact = serde_json::from_slice(&payload)
            .map_err(|error| integrity(session, error.to_string()))?;
        crate::session_store::artifact::validate_artifact_id(&fact.artifact_id, &fact.content_hash)
            .map_err(|error| integrity(session, error.to_string()))?;
        if fact.artifact_id == artifact.artifact_id && matched.replace((seq, fact)).is_some() {
            return Err(integrity(session, "duplicate artifact journal fact"));
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

fn integrity(session: &SessionId, message: impl Into<String>) -> SessionStoreError {
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
                _ => return Err(de::Error::unknown_field(&key, FIELDS)),
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

const FIELDS: &[&str] = &["artifact_id", "content_hash", "bytes", "preview"];
