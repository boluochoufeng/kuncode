use std::{process::Stdio, time::Duration};

use async_trait::async_trait;
use kuncode_core::completion::ToolDefinition;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::{process::Command, time::timeout};

use crate::{
    tool::{ToolErrorPayload, ToolOutput, TypedTool, definition_for},
    workspace::{Workspace, WorkspaceError},
};

const OUTPUT_LIMIT_BYTES: usize = 20_000;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const DANGEROUS_COMMAND_PATTERNS: &[&str] = &["rm -rf /", "sudo", "shutdown", "reboot", "> /dev/"];

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
    type Output = BashOutput;

    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn run(&self, args: BashArgs) -> ToolOutput<BashOutput> {
        let cmd = args.cmd;

        if cmd.trim().is_empty() {
            return ToolOutput::failure("invalid_arguments", "`cmd` must not be empty");
        }

        if let Some(pattern) = blocked_pattern(&cmd) {
            return ToolOutput::failure(
                "dangerous_command",
                format!("command contains blocked pattern `{pattern}`"),
            );
        }

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
                    kind: "non_zero_exit".to_string(),
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

fn blocked_pattern(cmd: &str) -> Option<&'static str> {
    DANGEROUS_COMMAND_PATTERNS
        .iter()
        .copied()
        .find(|pattern| cmd.contains(pattern))
}

#[cfg(test)]
mod tests {
    use super::{Bash, blocked_pattern};
    use crate::{tool::Tool, workspace::Workspace};

    async fn bash() -> Bash {
        Bash::from_current_dir()
            .await
            .expect("current directory should be a valid workspace")
    }

    #[tokio::test]
    async fn call_erases_typed_output_for_the_model() {
        let out = bash()
            .await
            .call(serde_json::json!({ "cmd": "printf hello" }))
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
        let out = bash()
            .await
            .call(serde_json::json!({}))
            .await
            .expect("bad arguments are model-recoverable, not a harness error");

        assert!(!out.ok);
        assert_eq!(out.error.expect("error payload").kind, "invalid_arguments");
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

    #[test]
    fn detects_dangerous_commands() {
        for cmd in [
            "rm -rf /",
            "sudo ls",
            "shutdown now",
            "reboot",
            "echo test > /dev/sda",
        ] {
            assert!(
                blocked_pattern(cmd).is_some(),
                "expected `{cmd}` to be blocked"
            );
        }
    }

    #[test]
    fn allows_regular_commands() {
        for cmd in ["ls -la", "cargo check", "rg ToolOutput crates"] {
            assert!(
                blocked_pattern(cmd).is_none(),
                "expected `{cmd}` to be allowed"
            );
        }
    }

    #[tokio::test]
    async fn truncates_oversized_output_with_a_visible_marker() {
        let workspace = Workspace::new(std::env::current_dir().expect("current directory exists"))
            .await
            .expect("workspace should be valid");
        // Emit well over OUTPUT_LIMIT_BYTES of pure `x` on stdout.
        let out = Bash::new(workspace)
            .call(serde_json::json!({ "cmd": "printf 'x%.0s' {1..30000}" }))
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
        let out = Bash::new(workspace)
            .call(serde_json::json!({ "cmd": "pwd" }))
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
}
