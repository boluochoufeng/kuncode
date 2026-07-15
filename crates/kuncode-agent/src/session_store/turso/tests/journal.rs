use crate::{
    session_store::{
        JournalKind, NewJournalEntry, NewSession, Seq, SessionStore, SessionStoreError,
        turso::TursoSessionStore,
    },
    test_support::TestDir,
};
use kuncode_core::completion::Message;

#[tokio::test]
async fn append_assigns_monotonic_seq_per_session() {
    let root = TestDir::new();
    let store = TursoSessionStore::open(root.path().join("sessions.db"))
        .await
        .expect("store should open");
    let session = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");

    let first = store
        .append(
            &session,
            NewJournalEntry::message(&Message::user("one")).expect("message"),
        )
        .await
        .expect("first append should commit");
    let second = store
        .append(
            &session,
            NewJournalEntry::message(&Message::user("two")).expect("message"),
        )
        .await
        .expect("second append should commit");

    assert_eq!(first, Seq::new(1));
    assert_eq!(second, Seq::new(2));
}

#[tokio::test]
async fn message_journal_uses_versioned_store_payload() {
    let root = TestDir::new();
    let store = TursoSessionStore::open(root.path().join("sessions.db"))
        .await
        .expect("store should open");
    let session = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");

    store
        .append(
            &session,
            NewJournalEntry::message(&Message::user("persist me")).expect("message"),
        )
        .await
        .expect("append should commit");

    let entries = store
        .replay_after(&session, Seq::ZERO)
        .await
        .expect("journal should replay");
    let payload = &entries[0].payload_json;

    assert_eq!(payload["schema_version"], 1);
    assert_eq!(payload["message"]["role"], "user");
    assert_eq!(payload["message"]["content"][0]["kind"], "text");
    assert_eq!(payload["message"]["content"][0]["text"], "persist me");
    assert!(payload.get("role").is_none());
}

#[tokio::test]
async fn journal_snapshot_returns_only_requested_facts_and_the_real_head() {
    let root = TestDir::new();
    let store = TursoSessionStore::open(root.path().join("sessions.db"))
        .await
        .expect("store should open");
    let session = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    let mut seqs = Vec::new();
    for index in 0..300 {
        let seq = store
            .append(
                &session,
                NewJournalEntry::message(&Message::user(format!("message-{index}")))
                    .expect("message should encode"),
            )
            .await
            .expect("message should commit");
        seqs.push(seq);
    }
    let head = store
        .append(
            &session,
            NewJournalEntry::raw(
                JournalKind::SessionNote,
                serde_json::json!({"note": "latest"}),
            ),
        )
        .await
        .expect("latest fact should commit");

    let mut requested = seqs[..270].iter().rev().copied().collect::<Vec<_>>();
    requested.push(seqs[14]);
    requested.push(Seq::new(head.get() + 100));
    let snapshot = store
        .journal_snapshot(&session, &requested)
        .await
        .expect("snapshot should read exact rows");

    assert_eq!(snapshot.head(), head);
    assert_eq!(
        snapshot
            .entries()
            .iter()
            .map(|entry| entry.seq)
            .collect::<Vec<_>>(),
        seqs[..270]
    );
    let empty = store
        .journal_snapshot(&session, &[])
        .await
        .expect("head-only snapshot should succeed");
    assert_eq!(empty.head(), head);
    assert!(empty.entries().is_empty());
}

#[tokio::test]
async fn journal_snapshot_never_mixes_head_and_rows_across_concurrent_appends() {
    let root = TestDir::new();
    let store = std::sync::Arc::new(
        TursoSessionStore::open(root.path().join("sessions.db"))
            .await
            .expect("store should open"),
    );
    let session = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    for index in 0..300 {
        store
            .append(
                &session,
                NewJournalEntry::message(&Message::user(format!("seed-{index}")))
                    .expect("message should encode"),
            )
            .await
            .expect("seed should commit");
    }
    let requested = (1..=500).map(Seq::new).collect::<Vec<_>>();
    let writer_store = store.clone();
    let writer_session = session.clone();
    let writer = tokio::spawn(async move {
        for index in 300..500 {
            writer_store
                .append(
                    &writer_session,
                    NewJournalEntry::message(&Message::user(format!("append-{index}")))
                        .expect("message should encode"),
                )
                .await
                .expect("concurrent append should commit");
            tokio::task::yield_now().await;
        }
    });

    // Each read may choose a different point in the append stream, but its head
    // and rows must always describe the same transaction snapshot.
    for _ in 0..50 {
        let snapshot = store
            .journal_snapshot(&session, &requested)
            .await
            .expect("snapshot should remain readable during appends");
        assert_eq!(
            snapshot.entries().last().map(|entry| entry.seq),
            Some(snapshot.head())
        );
        assert_eq!(snapshot.entries().len(), snapshot.head().get() as usize);
        tokio::task::yield_now().await;
    }
    writer.await.expect("writer task should finish");
}

#[tokio::test]
async fn append_rejects_a_mistyped_durable_head() {
    let root = TestDir::new();
    let database = root.path().join("sessions.db");
    let store = TursoSessionStore::open(&database)
        .await
        .expect("store should open");
    let session = store
        .create_session(NewSession::new(root.path().to_path_buf()))
        .await
        .expect("session should be created");
    store
        .append(
            &session,
            NewJournalEntry::message(&Message::user("one")).expect("message"),
        )
        .await
        .expect("first append should commit");
    {
        let connection = store.connection_for_test().await;
        connection
            .execute(
                "UPDATE journal_entries SET seq = 'not-an-integer' WHERE session_id = ?1",
                [session.as_str()],
            )
            .await
            .expect("fixture should corrupt the durable head type");
    }

    let result = store
        .append(
            &session,
            NewJournalEntry::message(&Message::user("must not persist")).expect("message"),
        )
        .await;

    assert!(matches!(
        result,
        Err(SessionStoreError::JournalStoredIntegrity { .. })
    ));
}
