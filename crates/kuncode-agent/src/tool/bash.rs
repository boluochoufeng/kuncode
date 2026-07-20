use std::{process::Stdio, time::Duration};

use async_trait::async_trait;
use kuncode_core::completion::ToolDefinition;
use kuncode_core::non_empty_vec::NonEmptyVec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::{process::Command, time::timeout};

use crate::{
    permission::{
        CanonicalCommand, CanonicalToolInput, CommandKind, PermissionCheckSpec, PermissionTarget,
        ToolDisplay,
    },
    tool::{
        PreparationContext, ToolContext, ToolErrorPayload, ToolOutput, TypedPreparation, TypedTool,
        definition_for,
    },
    workspace::{Workspace, WorkspaceError},
};

const OUTPUT_LIMIT_BYTES: usize = 20_000;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

/// Arguments accepted by the [`Bash`] tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BashArgs {
    /// The shell command to run, e.g. `ls -la .`
    cmd: String,
}

/// Structured result of a [`Bash`] invocation.
#[derive(Debug, Serialize)]
pub struct BashOutput {
    pub cmd: String,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug)]
pub struct Bash {
    definition: ToolDefinition,
    workspace: Workspace,
}

impl Bash {
    pub fn new(workspace: Workspace) -> Self {
        Self {
            definition: definition_for::<BashArgs>("bash", "Run a shell command"),
            workspace,
        }
    }

    pub async fn from_current_dir() -> Result<Self, WorkspaceError> {
        Ok(Self::new(Workspace::from_current_dir().await?))
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }
}

#[async_trait]
impl TypedTool for Bash {
    type Args = BashArgs;
    type Prepared = BashArgs;
    type Output = BashOutput;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn prepare_typed(
        &self,
        args: BashArgs,
        _canonical_input: CanonicalToolInput,
        _ctx: &PreparationContext,
    ) -> Result<TypedPreparation<Self::Prepared>, ToolOutput> {
        if args.cmd.trim().is_empty() {
            return Err(ToolOutput::failure(
                "invalid_arguments",
                "`cmd` must not be empty",
            ));
        }
        let program = args
            .cmd
            .split_whitespace()
            .next()
            .unwrap_or("command")
            .to_string();
        let checks = command_checks(&args.cmd)?;
        let canonical_input = CanonicalToolInput::new(serde_json::json!({
            "cmd": args.cmd,
        }));
        Ok(TypedPreparation::new(
            args,
            canonical_input,
            checks,
            ToolDisplay::new(format!("Run shell command: {program}")),
        ))
    }

    async fn run_prepared(&self, prepared: BashArgs, _ctx: &ToolContext) -> ToolOutput<BashOutput> {
        let cmd = prepared.cmd;

        let mut command = Command::new("bash");
        command
            .arg("-lc")
            .arg(&cmd)
            .current_dir(self.workspace.root())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let output = match timeout(COMMAND_TIMEOUT, command.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(err)) => {
                return ToolOutput::failure("execution", format!("failed to run command: {err}"));
            }
            Err(_) => {
                return ToolOutput::failure(
                    "timeout",
                    format!("command exceeded {} seconds", COMMAND_TIMEOUT.as_secs()),
                );
            }
        };

        let (stdout, stdout_truncated) = output_text("stdout", &output.stdout);
        let (stderr, stderr_truncated) = output_text("stderr", &output.stderr);
        let truncated = stdout_truncated || stderr_truncated;
        let ok = output.status.success();
        let exit_code = output.status.code();

        ToolOutput {
            ok,
            data: Some(BashOutput {
                cmd,
                exit_code,
                stdout,
                stderr,
            }),
            error: if ok {
                None
            } else {
                Some(ToolErrorPayload {
                    kind: "non_zero_exit".into(),
                    message: match exit_code {
                        Some(code) => format!("command exited with status {code}"),
                        None => "command terminated by signal".to_string(),
                    },
                })
            },
            truncated,
        }
    }
}

/// Decodes a captured stream, capping it at `OUTPUT_LIMIT_BYTES`. Bash output
/// may not be valid UTF-8, so decoding is intentionally lossy (`from_utf8_lossy`).
///
/// When the cap trips, a visible marker is appended naming the stream and the
/// byte scale, so the model knows it holds only the head of the stream and must
/// not assume it saw everything. How to get the rest (filter, redirect, re-run)
/// is left to the model — bash is a general shell.
fn output_text(stream: &str, bytes: &[u8]) -> (String, bool) {
    if bytes.len() <= OUTPUT_LIMIT_BYTES {
        return (String::from_utf8_lossy(bytes).into_owned(), false);
    }

    // Slicing a byte slice at an arbitrary index never splits a `char` (that is
    // a `str` concern); `from_utf8_lossy` turns any partial trailing sequence
    // into U+FFFD, so the result is always valid UTF-8.
    let mut text = String::from_utf8_lossy(&bytes[..OUTPUT_LIMIT_BYTES]).into_owned();
    text.push_str(&format!(
        "\n…⟨kuncode: {stream} truncated — showed first {OUTPUT_LIMIT_BYTES} of {total} bytes⟩",
        total = bytes.len(),
    ));
    (text, true)
}

fn command_checks(command: &str) -> Result<NonEmptyVec<PermissionCheckSpec>, ToolOutput> {
    let commands = simple_command_chain(command);
    let mut checks = Vec::new();
    if let Some(commands) = commands {
        for command in commands {
            let target = CanonicalCommand::new(command, CommandKind::Simple)
                .map_err(|error| ToolOutput::failure("invalid_arguments", error.to_string()))?;
            checks.push(PermissionCheckSpec::new(PermissionTarget::Bash(target)));
        }
    } else {
        let target = CanonicalCommand::new(command.to_string(), CommandKind::Opaque)
            .map_err(|error| ToolOutput::failure("invalid_arguments", error.to_string()))?;
        checks.push(PermissionCheckSpec::new(PermissionTarget::Bash(target)));
    }

    let Some(first) = checks.first().cloned() else {
        return Err(ToolOutput::failure(
            "invalid_arguments",
            "`cmd` must contain a command",
        ));
    };
    Ok(NonEmptyVec::from_first_rest(
        first,
        checks.into_iter().skip(1).collect(),
    ))
}

/// Splits only shell syntax whose command boundaries can be recognized without
/// interpreting expansions. Any uncertain construct falls back to one opaque
/// selector bound to the complete command text.
fn simple_command_chain(command: &str) -> Option<Vec<String>> {
    #[derive(Clone, Copy, Eq, PartialEq)]
    enum Quote {
        None,
        Single,
        Double,
    }

    let mut quote = Quote::None;
    let mut current = String::new();
    let mut commands = Vec::new();
    let mut chars = command.chars().peekable();
    while let Some(ch) = chars.next() {
        match quote {
            Quote::Single => {
                current.push(ch);
                if ch == '\'' {
                    quote = Quote::None;
                }
            }
            Quote::Double => {
                if matches!(ch, '$' | '`' | '\\') {
                    return None;
                }
                current.push(ch);
                if ch == '"' {
                    quote = Quote::None;
                }
            }
            Quote::None => match ch {
                '\'' => {
                    quote = Quote::Single;
                    current.push(ch);
                }
                '"' => {
                    quote = Quote::Double;
                    current.push(ch);
                }
                '$' | '`' | '\\' | '<' | '>' | '(' | ')' | '\n' | '\r' | '#' | '*' | '?' | '['
                | ']' | '{' | '}' | '~' => return None,
                '&' => {
                    chars.next_if_eq(&'&')?;
                    push_simple_command(&mut commands, &mut current)?;
                }
                '|' => {
                    let _ = chars.next_if_eq(&'|');
                    push_simple_command(&mut commands, &mut current)?;
                }
                ';' => push_simple_command(&mut commands, &mut current)?,
                _ => current.push(ch),
            },
        }
    }
    if quote != Quote::None {
        return None;
    }
    push_simple_command(&mut commands, &mut current)?;
    Some(commands)
}

fn push_simple_command(commands: &mut Vec<String>, current: &mut String) -> Option<()> {
    let command = normalize_unquoted_whitespace(current.trim())?;
    if command.is_empty() || is_dynamic_shell_command(&command) {
        return None;
    }
    commands.push(command);
    current.clear();
    Some(())
}

fn normalize_unquoted_whitespace(command: &str) -> Option<String> {
    let mut output = String::with_capacity(command.len());
    let mut quote = None;
    let mut pending_space = false;
    for ch in command.chars() {
        match quote {
            Some(delimiter) => {
                output.push(ch);
                if ch == delimiter {
                    quote = None;
                }
            }
            None if matches!(ch, '\'' | '"') => {
                if pending_space && !output.is_empty() {
                    output.push(' ');
                }
                pending_space = false;
                quote = Some(ch);
                output.push(ch);
            }
            None if ch.is_whitespace() => pending_space = true,
            None => {
                if pending_space && !output.is_empty() {
                    output.push(' ');
                }
                pending_space = false;
                output.push(ch);
            }
        }
    }
    quote.is_none().then_some(output)
}

fn is_dynamic_shell_command(command: &str) -> bool {
    let mut words = command.split_whitespace();
    let Some(program) = words.next() else {
        return true;
    };
    if matches!(program, "eval" | "source" | "." | "env") {
        return true;
    }
    matches!(program, "sh" | "bash" | "zsh" | "dash") && words.any(|word| word == "-c")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{Bash, simple_command_chain};
    use crate::{
        tool::{Tool, ToolContext, execute_for_test},
        workspace::Workspace,
    };

    async fn bash() -> Bash {
        Bash::from_current_dir()
            .await
            .expect("current directory should be a valid workspace")
    }

    #[tokio::test]
    async fn call_erases_typed_output_for_the_model() {
        let out = execute_for_test(
            Arc::new(bash().await),
            serde_json::json!({ "cmd": "printf hello" }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness-level error");

        assert!(out.ok);
        assert!(!out.truncated);
        let data = out.data.expect("data present");
        assert_eq!(data["stdout"], "hello");
        assert_eq!(data["exit_code"], 0);
    }

    #[tokio::test]
    async fn call_reports_bad_arguments_in_the_envelope() {
        let out = execute_for_test(
            Arc::new(bash().await),
            serde_json::json!({}),
            &ToolContext::new(),
        )
        .await
        .expect("bad arguments are model-recoverable, not a harness error");

        assert!(!out.ok);
        assert_eq!(
            out.error.expect("error payload").kind.as_str(),
            "invalid_arguments"
        );
    }

    #[tokio::test]
    async fn definition_schema_is_generated_from_args() {
        let bash = bash().await;
        let definition = Tool::definition(&bash);

        assert_eq!(definition.name, "bash");

        let params = &definition.parameters;
        // Meta keys must be stripped for function-calling APIs.
        assert!(params.get("$schema").is_none());
        assert!(params.get("title").is_none());
        // Schema must reflect the `BashArgs` type, not a hand-written copy.
        assert_eq!(params["type"], "object");
        assert_eq!(params["required"], serde_json::json!(["cmd"]));
        assert_eq!(params["properties"]["cmd"]["type"], "string");
    }

    #[tokio::test]
    async fn truncates_oversized_output_with_a_visible_marker() {
        let workspace = Workspace::new(std::env::current_dir().expect("current directory exists"))
            .await
            .expect("workspace should be valid");
        // Emit well over OUTPUT_LIMIT_BYTES of pure `x` on stdout.
        let out = execute_for_test(
            Arc::new(Bash::new(workspace)),
            serde_json::json!({ "cmd": "printf 'x%.0s' {1..30000}" }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness-level error");

        assert!(out.ok);
        assert!(out.truncated);
        let stdout = out.data.expect("data present")["stdout"]
            .as_str()
            .expect("stdout is a string")
            .to_string();
        // The capped prefix is preserved and a marker names the stream + scale.
        assert!(stdout.starts_with(&"x".repeat(super::OUTPUT_LIMIT_BYTES)));
        assert!(stdout.contains("stdout truncated"));
    }

    #[tokio::test]
    async fn runs_commands_from_workspace_root() {
        let workspace = Workspace::new(std::env::current_dir().expect("current directory exists"))
            .await
            .expect("workspace should be valid");
        let out = execute_for_test(
            Arc::new(Bash::new(workspace)),
            serde_json::json!({ "cmd": "pwd" }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness-level error");

        assert!(out.ok);
        let stdout = out.data.expect("data present")["stdout"]
            .as_str()
            .expect("stdout should be a string")
            .trim()
            .to_string();
        assert_eq!(
            stdout,
            std::env::current_dir()
                .expect("current directory exists")
                .canonicalize()
                .expect("current directory canonicalizes")
                .display()
                .to_string()
        );
    }

    #[test]
    fn parses_simple_chains_without_changing_quoted_whitespace() {
        assert_eq!(
            simple_command_chain("cargo  test && printf 'a  b' | wc -c"),
            Some(vec![
                "cargo test".to_string(),
                "printf 'a  b'".to_string(),
                "wc -c".to_string(),
            ])
        );
    }

    #[test]
    fn marks_expansion_redirection_and_nested_shell_as_opaque() {
        for command in [
            "echo $(whoami)",
            "echo $TOKEN",
            "cargo test > result.txt",
            "bash -c 'git status'",
            "eval 'git status'",
        ] {
            assert_eq!(simple_command_chain(command), None, "{command}");
        }
    }
}
