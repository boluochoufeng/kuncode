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

        let (stdout, stdout_truncated) = output_text(&output.stdout);
        let (stderr, stderr_truncated) = output_text(&output.stderr);
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

fn output_text(bytes: &[u8]) -> (String, bool) {
    let truncated = bytes.len() > OUTPUT_LIMIT_BYTES;
    let bytes = if truncated {
        &bytes[..OUTPUT_LIMIT_BYTES]
    } else {
        bytes
    };

    (String::from_utf8_lossy(bytes).into_owned(), truncated)
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
