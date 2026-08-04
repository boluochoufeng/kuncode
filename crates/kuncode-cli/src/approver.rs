//! Terminal approval resolver.

use std::io::{IsTerminal, Write};

use async_trait::async_trait;
use kuncode_agent::permission::{
    ApprovalChallenge, ApprovalResolution, ApprovalResolver, ChangePreview, PolicyEffect,
    PolicyMutationTemplateId, PolicyScope, PreviewLineKind,
};

/// Resolves challenge options through a blocking terminal prompt.
pub struct TerminalApprover;

#[async_trait]
impl ApprovalResolver for TerminalApprover {
    async fn resolve(&self, challenge: &ApprovalChallenge) -> ApprovalResolution {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            return ApprovalResolution::Deny { persistence: None };
        }

        let display = challenge.request_snapshot().display();
        let summary = display.summary().to_string();
        let preview = display.preview().cloned();
        let targets = challenge
            .pending_checks()
            .iter()
            .map(|check| check.target().to_string())
            .collect::<Vec<_>>();
        let allow = mutation_id(challenge, PolicyEffect::Allow);
        let deny = mutation_id(challenge, PolicyEffect::Deny);
        let prompt_allow = allow.is_some();
        let prompt_deny = deny.is_some();
        let answer = tokio::task::spawn_blocking(move || {
            prompt(
                &summary,
                &targets,
                preview.as_ref(),
                prompt_allow,
                prompt_deny,
            )
        })
        .await
        .unwrap_or_else(|_| "n".to_string());

        match answer.as_str() {
            "y" | "yes" => ApprovalResolution::Approve { persistence: None },
            "a" | "always" if allow.is_some() => ApprovalResolution::Approve { persistence: allow },
            "d" if deny.is_some() => ApprovalResolution::Deny { persistence: deny },
            "c" | "cancel" => ApprovalResolution::Cancel,
            _ => ApprovalResolution::Deny { persistence: None },
        }
    }
}

fn mutation_id(
    challenge: &ApprovalChallenge,
    effect: PolicyEffect,
) -> Option<PolicyMutationTemplateId> {
    let mut matches = challenge
        .mutation_options()
        .iter()
        .filter(|option| option.effect() == effect && option.scope() == PolicyScope::Session);
    let selected = matches.next()?;
    matches.next().is_none().then(|| selected.id().clone())
}

fn prompt(
    summary: &str,
    targets: &[String],
    preview: Option<&ChangePreview>,
    allow_always: bool,
    deny_always: bool,
) -> String {
    let mut out = std::io::stdout();
    let _ = write!(
        out,
        "{}",
        prompt_text(summary, targets, preview, allow_always, deny_always)
    );
    let _ = out.flush();

    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
        return "n".to_string();
    }
    line.trim().to_lowercase()
}

fn prompt_text(
    summary: &str,
    targets: &[String],
    preview: Option<&ChangePreview>,
    allow_always: bool,
    deny_always: bool,
) -> String {
    let targets = targets
        .iter()
        .map(|target| format!("  - {target}"))
        .collect::<Vec<_>>()
        .join("\n");
    let allow = if allow_always {
        "  [a] allow session"
    } else {
        ""
    };
    let deny = if deny_always {
        "  [d] deny session"
    } else {
        ""
    };
    let change = preview.map(render_preview).unwrap_or_default();
    format!(
        "\n\u{26a0}  Permission required: {summary}\n{targets}\n{change}  [y] allow once{allow}  [n] no{deny}  [c] cancel > "
    )
}

/// Renders the proposed change as a diff above the answer line.
///
/// The prefixes are written here, never taken from the preview: its lines carry
/// only text and a kind, so nothing in a file being edited can pass itself off
/// as a marker or as part of the prompt.
fn render_preview(preview: &ChangePreview) -> String {
    let mut out = format!(
        "  {} +{} -{}\n",
        "─".repeat(8),
        preview.added,
        preview.removed
    );
    for line in &preview.lines {
        let (marker, number) = match line.kind {
            PreviewLineKind::Added => ('+', line.number),
            PreviewLineKind::Removed => ('-', line.number),
            PreviewLineKind::Context => (' ', line.number),
        };
        out.push_str(&format!("  {marker} {number:>5} {}\n", line.text));
    }
    if preview.elided > 0 {
        out.push_str(&format!("  … {} more line(s) not shown\n", preview.elided));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::prompt_text;
    use kuncode_agent::permission::ChangePreview;

    #[test]
    fn prompt_lists_every_pending_target_and_available_scopes() {
        let text = prompt_text(
            "Run shell command: cargo",
            &[
                "Bash(cargo test)".to_string(),
                "Read(Cargo.toml)".to_string(),
            ],
            None,
            true,
            true,
        );
        assert!(text.contains("Bash(cargo test)"));
        assert!(text.contains("Read(Cargo.toml)"));
        assert!(text.contains("allow session"));
        assert!(text.contains("deny session"));
    }

    #[test]
    fn a_replacement_shows_what_would_be_lost_before_the_answer_line() {
        let preview =
            ChangePreview::between("keep me\ndelete me\n", "keep me\n").expect("a change");

        let text = prompt_text(
            "Write file: notes.txt",
            &["Edit(notes.txt)".to_string()],
            Some(&preview),
            false,
            false,
        );

        // The line about to disappear has to be on screen while the question is
        // still open — that is the entire point of previewing.
        assert!(text.contains("- "), "removed lines need a marker: {text}");
        assert!(text.contains("delete me"), "{text}");
        assert!(text.contains("+0 -1"), "{text}");
        let answer = text.find("[y] allow once").expect("the answer line");
        let removed = text.find("delete me").expect("the removed line");
        assert!(removed < answer, "the diff must come before the prompt");
    }

    #[test]
    fn preview_text_cannot_forge_its_own_diff_markers() {
        // A file whose content looks like diff output. The markers the user
        // reads are written by the renderer, so this line stays data.
        let preview = ChangePreview::between("a\n", "+ 99999 fake added line\n").expect("a change");

        let text = prompt_text("Write file: x", &[], Some(&preview), false, false);

        // Its own line is still rendered with the renderer's own `+` and the
        // real line number 1, not the numbers embedded in the text.
        assert!(text.contains("+     1 + 99999 fake added line"), "{text}");
    }
}
