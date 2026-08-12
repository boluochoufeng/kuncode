use crate::{
    session_store::{NewJournalEntry, NewSession, SessionStore, turso::TursoSessionStore},
    test_support::TestDir,
};
use kuncode_core::completion::Message;

/// Journals one message so the session qualifies for listings.
async fn append_message(store: &TursoSessionStore, session: &crate::session_store::SessionId) {
    store
        .append(
            session,
            NewJournalEntry::message(&Message::user("hello")).expect("message"),
        )
        .await
        .expect("append should commit");
}

#[tokio::test]
async fn listing_is_scoped_to_the_exact_project_root() {
    let root = TestDir::new();
    let store = TursoSessionStore::open(root.path().join("sessions.db"))
        .await
        .expect("store should open");
    let here = store
        .create_session(NewSession::new(root.path().join("project-a")))
        .await
        .expect("session should be created");
    let elsewhere = store
        .create_session(NewSession::new(root.path().join("project-b")))
        .await
        .expect("session should be created");
    append_message(&store, &here).await;
    append_message(&store, &elsewhere).await;

    let sessions = store
        .list_sessions(&root.path().join("project-a"), 10)
        .await
        .expect("listing should succeed");

    assert_eq!(
        sessions.iter().map(|s| &s.id).collect::<Vec<_>>(),
        vec![&here]
    );
}

#[tokio::test]
async fn listing_orders_by_recency_and_carries_count_and_preview() {
    let root = TestDir::new();
    let store = TursoSessionStore::open(root.path().join("sessions.db"))
        .await
        .expect("store should open");
    let project = root.path().to_path_buf();
    let older = store
        .create_session(NewSession::new(project.clone()))
        .await
        .expect("session should be created");
    let newer = store
        .create_session(NewSession::new(project.clone()))
        .await
        .expect("session should be created");
    append_message(&store, &newer).await;
    // `updated_at` has millisecond precision, so adjacent appends can tie and
    // fall back to the id tiebreak; a real gap keeps the ordering assertion
    // about activity, not about id encoding.
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    // Appending to `older` last makes it the most recently updated session, so
    // the listing must order by activity rather than creation.
    for text in ["  \n  first line  \nsecond line", "reply"] {
        store
            .append(
                &older,
                NewJournalEntry::message(&Message::user(text)).expect("message"),
            )
            .await
            .expect("append should commit");
    }

    let sessions = store
        .list_sessions(&project, 10)
        .await
        .expect("listing should succeed");

    assert_eq!(
        sessions.iter().map(|s| &s.id).collect::<Vec<_>>(),
        vec![&older, &newer]
    );
    assert_eq!(sessions[0].message_count, 2);
    assert_eq!(sessions[0].preview.as_deref(), Some("first line"));
    assert_eq!(sessions[1].message_count, 1);
    assert_eq!(sessions[1].preview.as_deref(), Some("hello"));
}

#[tokio::test]
async fn listing_respects_the_row_limit() {
    let root = TestDir::new();
    let store = TursoSessionStore::open(root.path().join("sessions.db"))
        .await
        .expect("store should open");
    let project = root.path().to_path_buf();
    for _ in 0..3 {
        let session = store
            .create_session(NewSession::new(project.clone()))
            .await
            .expect("session should be created");
        append_message(&store, &session).await;
    }

    let sessions = store
        .list_sessions(&project, 2)
        .await
        .expect("listing should succeed");

    assert_eq!(sessions.len(), 2);
}

#[tokio::test]
async fn listing_omits_sessions_without_messages() {
    let root = TestDir::new();
    let store = TursoSessionStore::open(root.path().join("sessions.db"))
        .await
        .expect("store should open");
    let project = root.path().to_path_buf();
    // An empty row (e.g. left behind by a run that died between session
    // creation and its first append) must not surface as resumable — nor may
    // it consume a slot of the requested limit.
    store
        .create_session(NewSession::new(project.clone()))
        .await
        .expect("session should be created");
    let with_history = store
        .create_session(NewSession::new(project.clone()))
        .await
        .expect("session should be created");
    append_message(&store, &with_history).await;

    let sessions = store
        .list_sessions(&project, 1)
        .await
        .expect("listing should succeed");

    assert_eq!(
        sessions.iter().map(|s| &s.id).collect::<Vec<_>>(),
        vec![&with_history]
    );
}
