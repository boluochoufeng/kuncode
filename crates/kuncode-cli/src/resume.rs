//! Resolves `--continue` / `--resume` into a concrete stored session.
//!
//! Selection stays out of [`CliRuntime`]: the runtime only knows how to list
//! and rebuild sessions, while this module owns the terminal conversation —
//! including the interactive picker printed before any UI starts.

use std::io::{self, IsTerminal, Write};

use kuncode_agent::session_store::{SessionId, SessionSummary};
use kuncode_core::completion::CompletionModel;

use crate::{Cli, runtime::CliRuntime};

/// Upper bound on sessions fetched for continue/picker/prefix resolution; far
/// beyond what a per-project picker can usefully show.
const LISTING_LIMIT: usize = 200;

/// What the resume flags asked for.
enum ResumeRequest {
    /// No resume flag: start a new session.
    None,
    /// `--continue`: the most recently updated session.
    Latest,
    /// `--resume`: pick interactively.
    Pick,
    /// `--resume <ID>`: an exact id or unique prefix.
    Id(String),
}

impl ResumeRequest {
    fn from_cli(cli: &Cli) -> Self {
        if cli.continue_latest {
            return Self::Latest;
        }
        match &cli.resume {
            None => Self::None,
            Some(None) => Self::Pick,
            Some(Some(id)) => Self::Id(id.clone()),
        }
    }
}

/// Applies the resume flags, installing the chosen target on the runtime.
///
/// A cancelled picker deliberately falls through to a fresh session; every
/// other unmet request is an error, because the user asked for a specific
/// history and must not silently lose it.
///
/// # Errors
/// Fails when the store is unavailable, no session matches the request, an id
/// prefix is ambiguous, or the picker cannot run without a terminal.
pub(crate) async fn apply_resume_flags<M: CompletionModel>(
    runtime: &mut CliRuntime<M>,
    cli: &Cli,
) -> Result<(), Box<dyn std::error::Error>> {
    let request = ResumeRequest::from_cli(cli);
    if matches!(request, ResumeRequest::None) {
        return Ok(());
    }
    let sessions = runtime.list_sessions(LISTING_LIMIT).await?;
    let selected = match request {
        ResumeRequest::None => unreachable!("handled above"),
        ResumeRequest::Latest => Some(
            sessions
                .first()
                .ok_or("no resumable sessions in this project")?
                .id
                .clone(),
        ),
        ResumeRequest::Id(id) => Some(match_session_id(&sessions, &id)?),
        ResumeRequest::Pick => pick_session(&sessions)?,
    };
    if let Some(id) = selected {
        runtime.set_resume_target(id);
    } else {
        eprintln!("kuncode: selection cancelled, starting a new session.");
    }
    Ok(())
}

/// Resolves an exact session id or a unique prefix against this project's list.
fn match_session_id(
    sessions: &[SessionSummary],
    id: &str,
) -> Result<SessionId, Box<dyn std::error::Error>> {
    if let Some(session) = sessions.iter().find(|session| session.id.as_str() == id) {
        return Ok(session.id.clone());
    }
    let mut matches = sessions
        .iter()
        .filter(|session| session.id.as_str().starts_with(id));
    match (matches.next(), matches.next()) {
        (Some(session), None) => Ok(session.id.clone()),
        (Some(_), Some(_)) => Err(format!("session id prefix is ambiguous: {id}").into()),
        (None, _) => Err(format!("no session in this project matches: {id}").into()),
    }
}

/// Prints a numbered listing and reads one selection from stdin.
///
/// Returns `Ok(None)` when the user submits an empty line, which the caller
/// treats as "start a new session instead".
fn pick_session(
    sessions: &[SessionSummary],
) -> Result<Option<SessionId>, Box<dyn std::error::Error>> {
    if sessions.is_empty() {
        return Err("no resumable sessions in this project".into());
    }
    if !io::stdin().is_terminal() {
        return Err(
            "interactive session picking needs a terminal; use --continue or --resume=<SESSION_ID>"
                .into(),
        );
    }
    eprintln!("Resumable sessions (most recent first):");
    for (index, session) in sessions.iter().enumerate() {
        eprintln!("{}", picker_row(index, session));
    }
    eprint!("Enter a number to resume, or press Enter to start a new session: ");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    let choice: usize = line
        .parse()
        .map_err(|_| format!("invalid selection: {line}"))?;
    let session = sessions
        .get(choice.checked_sub(1).ok_or("selection starts at 1")?)
        .ok_or_else(|| format!("selection out of range: {choice}"))?;
    Ok(Some(session.id.clone()))
}

fn picker_row(index: usize, session: &SessionSummary) -> String {
    format!("  {:>3}. {}", index + 1, session_label(session))
}

/// One-line session descriptor — local update time, message count, then
/// title/preview — shared by the stdin picker rows and the TUI's `/resume`
/// panel so both surfaces describe a session identically.
pub(crate) fn session_label(session: &SessionSummary) -> String {
    let title = session
        .title
        .as_deref()
        .or(session.preview.as_deref())
        .unwrap_or("(empty session)");
    format!(
        "{}  {:>4} msgs  {}",
        local_time(&session.updated_at),
        session.message_count,
        title,
    )
}

/// Renders the store's RFC 3339 instant in local time, falling back to the
/// raw string when it does not parse.
fn local_time(rfc3339: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .map(|instant| {
            instant
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| rfc3339.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(id: &str) -> SessionSummary {
        SessionSummary {
            id: SessionId::new(id),
            title: None,
            created_at: "2026-08-10T00:00:00.000Z".to_string(),
            updated_at: "2026-08-10T00:00:00.000Z".to_string(),
            message_count: 0,
            preview: None,
        }
    }

    #[test]
    fn exact_id_wins_even_when_it_prefixes_another() {
        let sessions = [summary("session-1"), summary("session-12")];

        let resolved = match_session_id(&sessions, "session-1").expect("exact match");

        assert_eq!(resolved.as_str(), "session-1");
    }

    #[test]
    fn unique_prefix_resolves_and_ambiguous_prefix_fails() {
        let sessions = [summary("session-abc"), summary("session-axe")];

        let resolved = match_session_id(&sessions, "session-ab").expect("unique prefix");
        assert_eq!(resolved.as_str(), "session-abc");

        let error = match_session_id(&sessions, "session-a").expect_err("ambiguous prefix");
        assert!(error.to_string().contains("ambiguous"));
    }

    #[test]
    fn unknown_id_reports_project_scope() {
        let sessions = [summary("session-abc")];

        let error = match_session_id(&sessions, "nope").expect_err("unknown id");

        assert!(error.to_string().contains("no session"));
    }
}
