//! Typed identifiers for runtime primitives.
//!
//! Every ID is a `Uuid` v7 newtype: time-ordered, serializable as a string,
//! and not interchangeable across domains.

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! define_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            #[inline]
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            #[inline]
            pub fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            #[inline]
            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }
    };
}

define_id!(RunId, "Identifier for a single `Run`.");
define_id!(AgentId, "Identifier for an `Agent` within a run.");
define_id!(TurnId, "Identifier for a model `Turn`.");
define_id!(EventId, "Identifier for an entry in the event log.");
define_id!(ToolRequestId, "Identifier for a `ToolRequest`.");
define_id!(ArtifactId, "Identifier for a stored `Artifact`.");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique() {
        let a = RunId::new();
        let b = RunId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn json_round_trip() {
        let id = AgentId::new();
        let json = serde_json::to_string(&id).expect("serialize");
        let back: AgentId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, back);
        // The serialized form must be a bare UUID string.
        assert!(json.starts_with('"') && json.ends_with('"'));
    }

    #[test]
    fn display_matches_uuid() {
        let id = TurnId::new();
        assert_eq!(id.to_string(), id.as_uuid().to_string());
    }
}
