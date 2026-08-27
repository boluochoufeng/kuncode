//! Slash-command registry, parsing, and dispatch for the TUI prompt.
//!
//! Any submission whose first non-whitespace char is `/` is a command attempt
//! and never reaches the model; feedback lands in the transcript as
//! [`Item::Notice`](super::app::Item::Notice). Unit-testable without a
//! terminal.

use super::app::App;

/// The closed set of built-in commands. Adding one means: a variant here, an
/// entry in [`COMMANDS`], and an arm in [`dispatch`] — `/help` derives from
/// [`COMMANDS`], so it can never drift from what actually dispatches.
#[derive(Clone, Copy)]
enum Command {
    Help,
    Model,
    Resume,
    Compact,
    Quit,
}

/// One registry row: what `/help` and the completion menu print, and what the
/// name resolves to.
pub(super) struct CommandSpec {
    pub(super) name: &'static str,
    pub(super) description: &'static str,
    command: Command,
}

const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "help",
        description: "list available commands",
        command: Command::Help,
    },
    CommandSpec {
        name: "model",
        description: "switch the completion model (bare opens a picker; /model <name> is direct)",
        command: Command::Model,
    },
    CommandSpec {
        name: "resume",
        description: "switch to another stored session of this project",
        command: Command::Resume,
    },
    CommandSpec {
        name: "compact",
        description: "summarize the context now instead of waiting for the budget threshold",
        command: Command::Compact,
    },
    CommandSpec {
        name: "quit",
        description: "quit kuncode (typing `exit` also works)",
        command: Command::Quit,
    },
];

/// A parsed command attempt: `/name` plus whatever followed it.
struct Invocation<'a> {
    name: &'a str,
    /// Everything after the name, leading whitespace stripped.
    args: &'a str,
}

/// Whether a submitted line was consumed by the command layer.
pub(super) enum Dispatch {
    /// A command attempt (valid or not): handled, nothing goes to the model.
    Handled,
    /// Not a command; the caller submits it to the model unchanged.
    Prompt,
    /// `/model <name>`: the switch needs the event loop's runner + switcher.
    SwitchModel(String),
    /// Bare `/resume`: listing the stored sessions needs the event loop's
    /// store handle (and is async), so the picker opens there.
    PickSession,
    /// `/compact`: compacting needs the runner + session, and streams events
    /// like a turn, so the event loop drives it.
    Compact,
}

/// Routes one submitted line. Command attempts execute against `app` (echo +
/// output as [`Item::Notice`](super::app::Item::Notice)); anything else is
/// the caller's prompt.
///
/// Commands needing the event loop's resources (session/runner/store) return
/// a payload variant on [`Dispatch`] that `handle_idle_key` forwards to its
/// caller; the single call site keeps that contract compiler-guided.
pub(super) fn dispatch(app: &mut App, input: &str) -> Dispatch {
    let Some(invocation) = parse(input) else {
        return Dispatch::Prompt;
    };
    // Echo the attempt first so transcript feedback reads in submission order.
    app.push_notice(input.trim().to_string());
    match COMMANDS.iter().find(|spec| spec.name == invocation.name) {
        Some(CommandSpec {
            command: Command::Help,
            ..
        }) => app.push_notice(help_text()),
        Some(CommandSpec {
            command: Command::Model,
            ..
        }) => {
            let name = invocation.args.trim();
            if name.is_empty() {
                // The menu's Enter always dispatches the bare name, so this
                // is also what selecting /model from the popup does.
                app.open_model_picker();
            } else if name.contains(char::is_whitespace) {
                app.push_notice("usage: /model <name> — one model name, no spaces".to_string());
            } else {
                return Dispatch::SwitchModel(name.to_string());
            }
        }
        Some(CommandSpec {
            command: Command::Resume,
            ..
        }) => {
            if invocation.args.is_empty() {
                return Dispatch::PickSession;
            }
            // Unlike `--resume=<ID>`, the in-TUI form is picker-only: ids are
            // long and unmemorable mid-session, and the panel shows them all.
            app.push_notice("usage: /resume — no arguments; pick from the list".to_string());
        }
        Some(CommandSpec {
            command: Command::Compact,
            ..
        }) => {
            if invocation.args.is_empty() {
                return Dispatch::Compact;
            }
            app.push_notice("usage: /compact — no arguments".to_string());
        }
        Some(CommandSpec {
            command: Command::Quit,
            ..
        }) => app.should_quit = true,
        None => app.push_notice(format!(
            "unknown command: /{} — type /help",
            invocation.name
        )),
    }
    Dispatch::Handled
}

/// The registry rows whose names extend the command name being typed, for the
/// completion menu. `None` when the buffer is not in the name position — not
/// `/`-led, or whitespace after the name means arguments (or a prompt line)
/// have begun and the menu must close. `Some` but empty (no name matches the
/// prefix) also renders no menu; the distinction lets Enter fall through to
/// ordinary unknown-command dispatch.
pub(super) fn completions(input: &str) -> Option<Vec<&'static CommandSpec>> {
    // trim_start only: a trailing space is the user finishing the name, which
    // must close the menu rather than keep completing it.
    let prefix = input.trim_start().strip_prefix('/')?;
    if prefix.contains(char::is_whitespace) {
        return None;
    }
    Some(
        COMMANDS
            .iter()
            .filter(|spec| spec.name.starts_with(prefix))
            .collect(),
    )
}

/// `None` when `input` is not a command attempt (first non-whitespace char is
/// not `/`). A bare `/` or `/ args` parses with an empty name and falls out
/// of [`dispatch`] as an unknown command.
fn parse(input: &str) -> Option<Invocation<'_>> {
    let rest = input.trim().strip_prefix('/')?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    Some(Invocation {
        name,
        args: args.trim_start(),
    })
}

/// One line per registry entry, in table order.
fn help_text() -> String {
    COMMANDS
        .iter()
        .map(|spec| format!("/{} — {}", spec.name, spec.description))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use kuncode_agent::permission::PermissionMode;

    use super::super::app::Item;
    use super::*;

    fn app() -> App {
        App::new("m", PermissionMode::Default)
    }

    fn notice_texts(app: &App) -> Vec<&str> {
        app.conversation
            .iter()
            .map(|item| match item {
                Item::Notice(text) => text.as_str(),
                _ => panic!("expected only notices in the transcript"),
            })
            .collect()
    }

    #[test]
    fn parses_a_bare_command() {
        let invocation = parse("/help").expect("a command attempt");
        assert_eq!(invocation.name, "help");
        assert_eq!(invocation.args, "");
    }

    #[test]
    fn parses_arguments_verbatim_past_the_name() {
        let invocation = parse("/model deepseek-v4 --fast").expect("a command attempt");
        assert_eq!(invocation.name, "model");
        assert_eq!(invocation.args, "deepseek-v4 --fast");
    }

    #[test]
    fn surrounding_whitespace_still_counts_as_a_command() {
        let invocation = parse("  /help  ").expect("a command attempt");
        assert_eq!(invocation.name, "help");
        assert_eq!(invocation.args, "");
    }

    #[test]
    fn non_leading_slash_is_not_a_command() {
        assert!(parse("hello").is_none());
        assert!(parse("say /help").is_none());
    }

    #[test]
    fn a_bare_slash_parses_with_an_empty_name() {
        assert_eq!(parse("/").expect("a command attempt").name, "");
        let spaced = parse("/ foo").expect("a command attempt");
        assert_eq!(spaced.name, "");
        assert_eq!(spaced.args, "foo");
    }

    fn completion_names(input: &str) -> Option<Vec<&'static str>> {
        completions(input).map(|menu| menu.iter().map(|spec| spec.name).collect())
    }

    #[test]
    fn a_lone_slash_offers_every_command_in_table_order() {
        assert_eq!(
            completion_names("/"),
            Some(vec!["help", "model", "resume", "compact", "quit"])
        );
    }

    #[test]
    fn slash_compact_hands_the_work_to_the_event_loop() {
        let mut app = app();
        assert!(matches!(dispatch(&mut app, "/compact"), Dispatch::Compact));

        // Arguments are a typo, not a payload: refuse rather than compact.
        assert!(matches!(
            dispatch(&mut app, "/compact everything"),
            Dispatch::Handled
        ));
        assert!(
            notice_texts(&app)
                .last()
                .is_some_and(|text| text.starts_with("usage: /compact")),
            "{:?}",
            notice_texts(&app),
        );
    }

    #[test]
    fn a_name_prefix_narrows_the_menu() {
        assert_eq!(completion_names("/h"), Some(vec!["help"]));
        assert_eq!(completion_names("/q"), Some(vec!["quit"]));
        assert_eq!(completion_names("  /h"), Some(vec!["help"]));
    }

    #[test]
    fn an_unmatched_prefix_is_an_empty_menu_not_a_prompt() {
        assert_eq!(completion_names("/x"), Some(vec![]));
    }

    #[test]
    fn ordinary_text_has_no_menu() {
        assert_eq!(completion_names(""), None);
        assert_eq!(completion_names("hello"), None);
        assert_eq!(completion_names("say /help"), None);
    }

    #[test]
    fn whitespace_after_the_name_closes_the_menu() {
        // A finished name (trailing space) or begun arguments leave the name
        // position; so does a newline from multi-line input.
        assert_eq!(completion_names("/help "), None);
        assert_eq!(completion_names("/model deepseek"), None);
        assert_eq!(completion_names("/he\nllo"), None);
    }

    #[test]
    fn quit_sets_should_quit_and_echoes() {
        let mut app = app();

        assert!(matches!(dispatch(&mut app, "/quit"), Dispatch::Handled));

        assert!(app.should_quit, "/quit should quit the TUI");
        assert_eq!(notice_texts(&app), vec!["/quit"]);
    }

    #[test]
    fn help_lists_every_registered_command() {
        let mut app = app();

        assert!(matches!(dispatch(&mut app, "/help"), Dispatch::Handled));

        assert!(!app.should_quit);
        let notices = notice_texts(&app);
        assert_eq!(notices[0], "/help");
        for spec in COMMANDS {
            assert!(
                notices[1].contains(&format!("/{} — {}", spec.name, spec.description)),
                "help must list /{}",
                spec.name
            );
        }
    }

    #[test]
    fn model_without_args_opens_the_picker() {
        let mut app = app();
        app.available_models = vec!["m".to_string(), "other".to_string()];

        assert!(matches!(dispatch(&mut app, "/model"), Dispatch::Handled));

        assert!(!app.should_quit);
        // Only the echo notice; the selection happens in the picker.
        assert_eq!(notice_texts(&app), vec!["/model"]);
        let picker = app.model_picker.as_ref().expect("picker should be open");
        assert_eq!(picker.options, vec!["m", "other"]);
        assert_eq!(picker.selected, 0, "the active model starts highlighted");
    }

    #[test]
    fn model_with_a_name_returns_the_switch_payload() {
        let mut app = app();

        assert!(matches!(
            dispatch(&mut app, "/model deepseek-v4-pro"),
            Dispatch::SwitchModel(name) if name == "deepseek-v4-pro"
        ));

        // Only the echo notice: the switch itself happens in the event loop.
        assert_eq!(notice_texts(&app), vec!["/model deepseek-v4-pro"]);
    }

    #[test]
    fn model_with_extra_words_is_a_usage_notice() {
        let mut app = app();

        assert!(matches!(
            dispatch(&mut app, "/model a b"),
            Dispatch::Handled
        ));

        assert!(notice_texts(&app)[1].contains("usage"));
    }

    #[test]
    fn resume_without_args_returns_the_pick_payload() {
        let mut app = app();

        assert!(matches!(
            dispatch(&mut app, "/resume"),
            Dispatch::PickSession
        ));

        // Only the echo notice: the listing + picker happen in the event loop.
        assert_eq!(notice_texts(&app), vec!["/resume"]);
    }

    #[test]
    fn resume_with_args_is_a_usage_notice() {
        let mut app = app();

        assert!(matches!(
            dispatch(&mut app, "/resume some-id"),
            Dispatch::Handled
        ));

        assert!(notice_texts(&app)[1].contains("usage"));
    }

    #[test]
    fn unknown_command_notices_and_points_at_help() {
        let mut app = app();

        assert!(matches!(
            dispatch(&mut app, "/frobnicate"),
            Dispatch::Handled
        ));

        assert!(!app.should_quit);
        let notices = notice_texts(&app);
        assert!(notices[1].contains("/frobnicate"));
        assert!(notices[1].contains("/help"));
    }

    #[test]
    fn ordinary_text_is_a_prompt_with_no_side_effects() {
        let mut app = app();

        assert!(matches!(dispatch(&mut app, "hello"), Dispatch::Prompt));

        assert!(!app.should_quit);
        assert!(app.conversation.is_empty(), "prompts must not be echoed");
    }

    #[test]
    fn an_absolute_path_prompt_is_still_a_command_attempt() {
        // The decided rule: any leading `/` is a command attempt, matching
        // peer CLIs. A pasted path as the whole prompt gets an unknown-command
        // notice rather than a model turn.
        let mut app = app();

        assert!(matches!(dispatch(&mut app, "/home/x"), Dispatch::Handled));

        assert!(notice_texts(&app)[1].contains("unknown command"));
    }

    #[test]
    fn command_names_match_exactly() {
        let mut app = app();

        assert!(matches!(dispatch(&mut app, "/HELP"), Dispatch::Handled));

        assert!(notice_texts(&app)[1].contains("unknown command"));
    }

    #[test]
    fn registry_names_stay_lowercase_nonempty_and_unique() {
        for (index, spec) in COMMANDS.iter().enumerate() {
            assert!(!spec.name.is_empty(), "blank command name");
            assert_eq!(
                spec.name,
                spec.name.to_lowercase(),
                "registry names are lowercase"
            );
            assert!(
                COMMANDS[..index]
                    .iter()
                    .all(|prior| prior.name != spec.name),
                "duplicate command name {}",
                spec.name
            );
        }
    }
}
