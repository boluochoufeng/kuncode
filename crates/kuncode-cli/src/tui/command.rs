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
    Quit,
}

/// One registry row: what `/help` prints and what the name resolves to.
struct CommandSpec {
    name: &'static str,
    description: &'static str,
    command: Command,
}

const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "help",
        description: "list available commands",
        command: Command::Help,
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
    /// Everything after the name, leading whitespace stripped. Unused by
    /// `/help` and `/quit`; the grammar carries it now so an argument-taking
    /// command (`/model <name>`) is a table entry + match arm, not a parser
    /// change.
    #[allow(dead_code)] // read only by tests until the first argument-taking command lands
    args: &'a str,
}

/// Whether a submitted line was consumed by the command layer.
pub(super) enum Dispatch {
    /// A command attempt (valid or not): handled, nothing goes to the model.
    Handled,
    /// Not a command; the caller submits it to the model unchanged.
    Prompt,
}

/// Routes one submitted line. Command attempts execute against `app` (echo +
/// output as [`Item::Notice`](super::app::Item::Notice)); anything else is
/// the caller's prompt.
///
/// Effects are App-only today. A command needing the event loop's resources
/// (session/runner — e.g. a future `/model`) grows a payload variant on
/// [`Dispatch`] that `handle_idle_key` forwards to its caller; the single
/// call site keeps that migration compiler-guided.
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
