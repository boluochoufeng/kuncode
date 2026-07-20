//! Terminal approval resolver.

use std::io::{IsTerminal, Write};

use async_trait::async_trait;
use kuncode_agent::permission::{
    ApprovalChallenge, ApprovalResolution, ApprovalResolver, PolicyEffect,
    PolicyMutationTemplateId, PolicyScope,
};

/// Resolves challenge options through a blocking terminal prompt.
pub struct TerminalApprover;

#[async_trait]
impl ApprovalResolver for TerminalApprover {
    async fn resolve(&self, challenge: &ApprovalChallenge) -> ApprovalResolution {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            return ApprovalResolution::Deny { persistence: None };
        }

        let summary = challenge.request_snapshot().display().summary().to_string();
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
            prompt(&summary, &targets, prompt_allow, prompt_deny)
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

fn prompt(summary: &str, targets: &[String], allow_always: bool, deny_always: bool) -> String {
    let mut out = std::io::stdout();
    let _ = write!(
        out,
        "{}",
        prompt_text(summary, targets, allow_always, deny_always)
    );
    let _ = out.flush();

    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
        return "n".to_string();
    }
    line.trim().to_lowercase()
}

fn prompt_text(summary: &str, targets: &[String], allow_always: bool, deny_always: bool) -> String {
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
    format!(
        "\n\u{26a0}  Permission required: {summary}\n{targets}\n  [y] allow once{allow}  [n] no{deny}  [c] cancel > "
    )
}

#[cfg(test)]
mod tests {
    use super::prompt_text;

    #[test]
    fn prompt_lists_every_pending_target_and_available_scopes() {
        let text = prompt_text(
            "Run shell command: cargo",
            &[
                "Bash(cargo test)".to_string(),
                "Read(Cargo.toml)".to_string(),
            ],
            true,
            true,
        );
        assert!(text.contains("Bash(cargo test)"));
        assert!(text.contains("Read(Cargo.toml)"));
        assert!(text.contains("allow session"));
        assert!(text.contains("deny session"));
    }
}
