use super::*;
use kuncode_agent::observer::ToolFailure;
use kuncode_agent::tool::ToolErrorKind;

fn app() -> App {
    App::new("model", PermissionMode::Default)
}

fn tool_start(id: &str) -> EventKind {
    EventKind::ToolStart {
        tool_call_id: id.to_string(),
        tool: "bash".to_string(),
        summary: "run ls".to_string(),
    }
}

#[test]
fn tool_start_then_ok_updates_the_same_entry() {
    let mut app = app();
    app.apply_event(tool_start("1"));
    app.apply_event(EventKind::ToolEnd {
        tool_call_id: "1".to_string(),
        tool: "bash".to_string(),
        ok: true,
        truncated: true,
        error: None,
    });
    // One entry (not two), flipped to Ok with the truncation flag carried.
    match app.conversation.as_slice() {
        [
            Item::Tool {
                state: ToolState::Ok { truncated: true },
                ..
            },
        ] => {}
        other => panic!("unexpected log: {} items", other.len()),
    }
}

#[test]
fn denial_reads_apart_from_a_failure() {
    let mut app = app();
    app.apply_event(tool_start("1"));
    app.apply_event(EventKind::ToolEnd {
        tool_call_id: "1".to_string(),
        tool: "bash".to_string(),
        ok: false,
        truncated: false,
        error: Some(ToolFailure {
            kind: ToolErrorKind::PermissionDenied,
            message: "blocked".to_string(),
        }),
    });
    assert!(matches!(
        app.conversation.as_slice(),
        [Item::Tool {
            state: ToolState::Denied(_),
            ..
        }]
    ));
}

#[test]
fn tool_end_without_a_start_still_surfaces() {
    // The runner reports an unknown tool / bad arguments as a `ToolEnd` with no
    // preceding `ToolStart`; it must still show up, not vanish.
    let mut app = app();
    app.apply_event(EventKind::ToolEnd {
        tool_call_id: "1".to_string(),
        tool: "mystery".to_string(),
        ok: false,
        truncated: false,
        error: Some(ToolFailure {
            kind: ToolErrorKind::UnknownTool,
            message: "no such tool".to_string(),
        }),
    });
    match app.conversation.as_slice() {
        [
            Item::Tool {
                name,
                state: ToolState::Failed(_),
                ..
            },
        ] => assert_eq!(name, "mystery"),
        _ => panic!("orphan ToolEnd should surface as a tool entry"),
    }
}

fn todo(content: &str, status: kuncode_agent::todo::TodoStatus) -> TodoItem {
    TodoItem {
        content: content.to_string(),
        active_form: format!("{content}…"),
        status,
    }
}

#[test]
fn todo_update_replaces_the_live_plan_without_touching_the_log() {
    use kuncode_agent::todo::TodoStatus;
    let mut app = app();
    app.apply_event(EventKind::TodoUpdate {
        todos: vec![todo("a", TodoStatus::InProgress)],
    });
    // Intervening log activity must not move or duplicate the plan: it is a
    // sticky panel, not a conversation entry.
    app.push_user("keep going".to_string());
    app.apply_event(EventKind::TodoUpdate {
        todos: vec![
            todo("a", TodoStatus::Completed),
            todo("b", TodoStatus::InProgress),
        ],
    });
    // The plan field holds the latest snapshot wholesale.
    assert_eq!(app.plan.len(), 2);
    assert_eq!(app.plan[0].status, TodoStatus::Completed);
    assert_eq!(app.plan[1].content, "b");
    // The log only has the user message — no plan entry leaked into it.
    assert!(matches!(app.conversation.as_slice(), [Item::User(_)]));
}

#[test]
fn clearing_the_plan_empties_the_panel() {
    use kuncode_agent::todo::TodoStatus;
    let mut app = app();
    app.apply_event(EventKind::TodoUpdate {
        todos: vec![todo("a", TodoStatus::InProgress)],
    });
    // An empty plan clears it: the panel is hidden, not left as a stale list.
    app.apply_event(EventKind::TodoUpdate { todos: vec![] });
    assert!(app.plan.is_empty());
}

#[test]
fn call_free_assistant_event_is_not_logged() {
    // The final answer arrives via `push_assistant`; the reducer must ignore
    // the call-free `Assistant` event so it is not doubled.
    let mut app = app();
    app.apply_event(EventKind::Assistant {
        text: "done".to_string(),
        tool_calls: vec![],
    });
    assert!(app.conversation.is_empty());
}

#[test]
fn narration_alongside_calls_is_logged() {
    let mut app = app();
    app.apply_event(EventKind::Assistant {
        text: "let me check".to_string(),
        tool_calls: vec!["1".to_string()],
    });
    match app.conversation.as_slice() {
        [Item::Assistant(text)] => assert_eq!(text, "let me check"),
        _ => panic!("narration not logged"),
    }
}

#[test]
fn streamed_deltas_preview_then_finalize_without_duplication() {
    let mut app = app();
    app.apply_event(EventKind::ModelStart);
    app.apply_event(EventKind::TextDelta {
        text: "Hel".to_string(),
    });
    app.apply_event(EventKind::TextDelta {
        text: "lo".to_string(),
    });
    // Live preview accumulates; nothing committed to the log yet.
    assert_eq!(app.stream_answer, "Hello");
    assert!(app.conversation.is_empty());

    // The call-free `Assistant` event is ignored (preview kept to avoid a
    // blank frame); the turn driver commits the final answer.
    app.apply_event(EventKind::Assistant {
        text: "Hello".to_string(),
        tool_calls: vec![],
    });
    assert_eq!(app.stream_answer, "Hello", "preview survives until commit");

    app.push_assistant("Hello".to_string());
    assert!(app.stream_answer.is_empty(), "commit clears the preview");
    match app.conversation.as_slice() {
        [Item::Assistant(text)] => assert_eq!(text, "Hello"),
        _ => panic!("final answer should be the single committed item"),
    }
}

#[test]
fn reasoning_streams_into_its_own_buffer() {
    let mut app = app();
    app.apply_event(EventKind::ReasoningDelta {
        text: "think ".to_string(),
    });
    app.apply_event(EventKind::ReasoningDelta {
        text: "hard".to_string(),
    });
    assert_eq!(app.stream_reasoning, "think hard");
    assert!(
        app.stream_answer.is_empty(),
        "reasoning is a separate channel"
    );
}

#[test]
fn narration_event_clears_the_streamed_preview() {
    let mut app = app();
    app.apply_event(EventKind::TextDelta {
        text: "let me check".to_string(),
    });
    app.apply_event(EventKind::Assistant {
        text: "let me check".to_string(),
        tool_calls: vec!["1".to_string()],
    });
    // Narration commits as one item; the preview is gone (not double-shown).
    assert!(app.stream_answer.is_empty());
    match app.conversation.as_slice() {
        [Item::Assistant(text)] => assert_eq!(text, "let me check"),
        _ => panic!("narration not committed exactly once"),
    }
}

#[test]
fn model_start_clears_a_stale_preview() {
    let mut app = app();
    app.stream_answer = "leftover".to_string();
    app.stream_reasoning = "stale".to_string();
    app.apply_event(EventKind::ModelStart);
    assert!(app.stream_answer.is_empty() && app.stream_reasoning.is_empty());
}

#[test]
fn reveal_paces_at_the_rate_and_caps_at_received() {
    let mut app = app();
    app.apply_event(EventKind::TextDelta {
        text: "abcdef".to_string(),
    });
    // 120 cps over 33ms ≈ 4 chars per tick.
    assert!(app.advance_reveal(Duration::from_millis(33), 120));
    assert_eq!(&app.stream_answer[..app.answer_revealed], "abcd");
    assert!(app.advance_reveal(Duration::from_millis(33), 120));
    assert_eq!(&app.stream_answer[..app.answer_revealed], "abcdef");
    // Caught up: nothing left to reveal, so no redraw is requested.
    assert!(!app.advance_reveal(Duration::from_millis(33), 120));
    assert!(!app.has_pending_reveal());
}

#[test]
fn reveal_spends_budget_on_reasoning_before_the_answer() {
    let mut app = app();
    app.apply_event(EventKind::ReasoningDelta {
        text: "rr".to_string(),
    });
    app.apply_event(EventKind::TextDelta {
        text: "aaaa".to_string(),
    });
    // Budget of 3: 2 chars finish reasoning, the remaining 1 starts the answer.
    app.advance_reveal(Duration::from_secs(1), 3);
    assert_eq!(&app.stream_reasoning[..app.reasoning_revealed], "rr");
    assert_eq!(&app.stream_answer[..app.answer_revealed], "a");
}

#[test]
fn reveal_never_splits_a_multibyte_char() {
    let mut app = app();
    app.apply_event(EventKind::TextDelta {
        text: "héllo".to_string(), // 'é' is two bytes
    });
    // One char per tick, walking across the multi-byte boundary; the slice
    // must always stay valid UTF-8 (would panic otherwise).
    for _ in 0..6 {
        app.advance_reveal(Duration::from_millis(1), 1);
        let _shown = &app.stream_answer[..app.answer_revealed];
    }
    assert_eq!(&app.stream_answer[..app.answer_revealed], "héllo");
}

mod compaction;
mod input;
