use std::sync::Arc;

use kuncode_core::{
    completion::{AssistantContent, Message},
    non_empty_vec::NonEmptyVec,
};

use crate::{
    session::AgentSession,
    session_store::{
        NewJournalEntry, NewSession, Seq, SessionId, SessionStore, sqlite::SqliteSessionStore,
    },
    test_support::TestDir,
    tool::{ToolOutput, ToolResultRetention},
};

pub(crate) struct DurableFixture {
    _root: TestDir,
    pub(crate) store: Arc<SqliteSessionStore>,
    pub(crate) session_id: SessionId,
    pub(crate) session: AgentSession,
}

impl DurableFixture {
    pub(crate) async fn new(messages: Vec<(Message, bool)>) -> Self {
        Self::new_with_slimmable_results(messages, &[]).await
    }

    pub(crate) async fn new_with_slimmable_results(
        messages: Vec<(Message, bool)>,
        slimmable_result_indices: &[usize],
    ) -> Self {
        let root = TestDir::new();
        let store = Arc::new(
            SqliteSessionStore::open(root.path().join("sessions.sqlite3"))
                .await
                .expect("store should open"),
        );
        let session_id = store
            .create_session(NewSession::new(root.path().to_path_buf()))
            .await
            .expect("session should be created");
        let mut session = AgentSession::new();
        session
            .attach_session_id(session_id.clone())
            .expect("fresh session should attach");
        for (index, (message, human_authored)) in messages.into_iter().enumerate() {
            let seq = store
                .append(
                    &session_id,
                    NewJournalEntry::message(&message).expect("message should encode"),
                )
                .await
                .expect("message should commit");
            if slimmable_result_indices.contains(&index) {
                session.push_tool_result_with_journal_seq(
                    message,
                    Some(seq),
                    ToolResultRetention::Slimmable,
                );
            } else if human_authored {
                session.push_human_with_journal_seq(message, Some(seq));
            } else {
                session.push_with_journal_seq(message, Some(seq));
            }
        }
        Self {
            _root: root,
            store,
            session_id,
            session,
        }
    }

    pub(crate) async fn journal_len(&self) -> usize {
        self.store
            .replay_after(&self.session_id, Seq::ZERO)
            .await
            .expect("journal should replay")
            .len()
    }
}

pub(crate) fn ordinary_history() -> Vec<(Message, bool)> {
    vec![
        (Message::user("fix the failing test"), true),
        (
            Message::assistant("I inspected the old implementation"),
            false,
        ),
        (Message::assistant("recent response"), false),
    ]
}

pub(crate) fn artifact_history() -> Vec<(Message, bool)> {
    [
        tool_exchange("old", "bash", "old payload"),
        tool_exchange("recent", "read_file", "recent payload"),
    ]
    .concat()
    .into_iter()
    .map(|message| (message, false))
    .collect()
}

fn tool_exchange(id: &str, name: &str, body: &str) -> Vec<Message> {
    let output = ToolOutput::success(serde_json::json!({"body": body})).to_model_content();
    vec![
        Message::Assistant {
            id: None,
            content: NonEmptyVec::new(AssistantContent::tool_call(id, name, serde_json::json!({}))),
        },
        Message::tool_result(id, output),
    ]
}
