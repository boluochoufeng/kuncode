use std::{path::Path, process::Command};

use ::turso::transaction::TransactionBehavior;
use kuncode_core::completion::Message;

use crate::{
    session_store::{
        NewJournalEntry, NewSession, Seq, SessionId, SessionStore, turso::TursoSessionStore,
    },
    test_support::TestDir,
};

const SCENARIO_ENV: &str = "KUNCODE_TURSO_CRASH_SCENARIO";
const DATABASE_ENV: &str = "KUNCODE_TURSO_CRASH_DATABASE";
const RECEIPT_ENV: &str = "KUNCODE_TURSO_CRASH_RECEIPT";
const SESSION_ENV: &str = "KUNCODE_TURSO_CRASH_SESSION";

#[tokio::test]
async fn abrupt_exit_preserves_commits_and_rolls_back_open_transactions() {
    let root = TestDir::new();
    let database = root.path().join("sessions.db");
    let receipt = root.path().join("committed-session-id");

    run_child("committed", &database, Some(&receipt), None);
    let committed_session = SessionId::new(
        std::fs::read_to_string(&receipt).expect("child should publish committed session id"),
    );
    let store = TursoSessionStore::open(&database)
        .await
        .expect("store should recover after abrupt committed exit");
    let committed = store
        .replay_after(&committed_session, Seq::ZERO)
        .await
        .expect("committed journal should replay");
    assert_eq!(committed.len(), 1);
    drop(store);

    run_child("uncommitted", &database, None, Some(&committed_session));
    let store = TursoSessionStore::open(&database)
        .await
        .expect("store should recover after abrupt uncommitted exit");
    let recovered = store
        .replay_after(&committed_session, Seq::ZERO)
        .await
        .expect("recovered journal should replay");
    assert_eq!(recovered, committed);
}

fn run_child(scenario: &str, database: &Path, receipt: Option<&Path>, session: Option<&SessionId>) {
    let mut command = Command::new(std::env::current_exe().expect("test executable should exist"));
    command
        .arg("--exact")
        .arg("session_store::turso::tests::crash::abrupt_exit_child")
        .arg("--nocapture")
        .env(SCENARIO_ENV, scenario)
        .env(DATABASE_ENV, database);
    if let Some(receipt) = receipt {
        command.env(RECEIPT_ENV, receipt);
    }
    if let Some(session) = session {
        command.env(SESSION_ENV, session.as_str());
    }
    let status = command.status().expect("crash child should start");
    assert!(status.success(), "crash child failed with {status}");
}

#[test]
fn abrupt_exit_child() {
    let Ok(scenario) = std::env::var(SCENARIO_ENV) else {
        return;
    };
    let database = std::env::var(DATABASE_ENV).expect("child database path should be provided");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("child runtime should build");
    runtime.block_on(async move {
        let store = TursoSessionStore::open(database)
            .await
            .expect("child store should open");
        match scenario.as_str() {
            "committed" => {
                let session = store
                    .create_session(NewSession::new(std::env::temp_dir()))
                    .await
                    .expect("child session should be created");
                store
                    .append(
                        &session,
                        NewJournalEntry::message(&Message::user("durable before crash"))
                            .expect("child message should encode"),
                    )
                    .await
                    .expect("child append should commit");
                let receipt = std::env::var(RECEIPT_ENV)
                    .expect("committed child receipt path should be provided");
                std::fs::write(receipt, session.as_str())
                    .expect("committed child should publish session id");
            }
            "uncommitted" => {
                let session = std::env::var(SESSION_ENV)
                    .expect("uncommitted child session should be provided");
                let mut connection = store.connection_for_test().await;
                let tx = connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .await
                    .expect("child transaction should begin");
                tx.execute(
                    "INSERT INTO journal_entries \
                     (session_id, seq, kind, payload_json, created_at) \
                     VALUES (?1, 2, 'session_note', '{}', 'abrupt-exit')",
                    [session.as_str()],
                )
                .await
                .expect("child uncommitted write should execute");
            }
            other => panic!("unknown crash scenario `{other}`"),
        }

        // Bypass Rust destructors to exercise WAL recovery rather than normal close.
        std::process::exit(0);
    });
}
