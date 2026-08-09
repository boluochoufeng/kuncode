//! Executes approved shell commands with bounded output and descendant cleanup.

use std::{
    io,
    process::{ExitStatus, Stdio},
    time::Duration,
};

use async_trait::async_trait;
use kuncode_core::completion::ToolDefinition;
use kuncode_core::non_empty_vec::NonEmptyVec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, ChildStderr, ChildStdout, Command},
    time::timeout,
};

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
    /// The shell command to run, e.g. `cargo test --workspace`.
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
            definition: definition_for::<BashArgs>(
                "bash",
                "Run a shell command in the workspace. Use it for commands that \
                 do something — building, testing, version control, package \
                 managers — rather than to inspect the workspace: read_file, \
                 ls, glob, and grep answer those questions without an approval \
                 prompt and without spilling unbounded output into the \
                 conversation.",
            ),
            workspace,
        }
    }

    pub async fn from_current_dir() -> Result<Self, WorkspaceError> {
        Ok(Self::new(Workspace::from_current_dir().await?))
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    async fn run_command(&self, cmd: String, command_timeout: Duration) -> ToolOutput<BashOutput> {
        let mut command = Command::new("bash");
        command
            .arg("-lc")
            .arg(&cmd)
            .current_dir(self.workspace.root())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let output = match capture_command(command, command_timeout).await {
            Ok(output) => output,
            Err(error) => {
                let kind = match error {
                    CommandExecutionError::Execution(_) => "execution",
                    CommandExecutionError::Timeout { .. } => "timeout",
                };
                return ToolOutput::failure(kind, error.to_string());
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
        self.run_command(prepared.cmd, COMMAND_TIMEOUT).await
    }
}

#[derive(Debug, Error)]
enum CommandExecutionError {
    #[error("failed to run command: {0}")]
    Execution(#[source] io::Error),
    #[error("command exceeded {seconds} seconds")]
    Timeout { seconds: u64 },
}

#[derive(Debug)]
struct CapturedCommand {
    status: ExitStatus,
    stdout: CapturedStream,
    stderr: CapturedStream,
}

#[derive(Debug)]
struct CapturedStream {
    prefix: Vec<u8>,
    total_bytes: u64,
}

impl CapturedStream {
    fn truncated(&self) -> bool {
        self.total_bytes > self.prefix.len() as u64
    }
}

struct ManagedChild {
    child: Child,
    cleanup_armed: bool,
    #[cfg(unix)]
    process_group: Option<libc::pid_t>,
}

impl ManagedChild {
    fn new(child: Child) -> Self {
        #[cfg(unix)]
        let process_group = child.id().and_then(|id| libc::pid_t::try_from(id).ok());

        Self {
            child,
            cleanup_armed: true,
            #[cfg(unix)]
            process_group,
        }
    }

    fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }

    fn signal_termination(&mut self) {
        #[cfg(unix)]
        if let Some(process_group) = self.process_group {
            // SAFETY: `process_group` is the positive PID assigned by the OS to
            // the child and configured as its PGID before spawn. `killpg` only
            // reads these scalar arguments and targets that isolated group.
            let _ = unsafe { libc::killpg(process_group, libc::SIGKILL) };
        }

        // Keep this fallback on Unix too: if the group signal races process
        // setup or fails, Tokio can still terminate the direct child.
        let _ = self.child.start_kill();
    }

    async fn terminate_and_reap(&mut self) {
        self.signal_termination();
        let _ = self.child.wait().await;
        self.cleanup_armed = false;
    }

    fn disarm(&mut self) {
        self.cleanup_armed = false;
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if self.cleanup_armed {
            self.signal_termination();
        }
    }
}

async fn capture_command(
    mut command: Command,
    command_timeout: Duration,
) -> Result<CapturedCommand, CommandExecutionError> {
    // A separate process group lets cancellation and timeout include shell
    // pipelines, background jobs, and grandchildren rather than only `bash`.
    #[cfg(unix)]
    command.process_group(0);

    let child = command.spawn().map_err(CommandExecutionError::Execution)?;
    let mut child = ManagedChild::new(child);
    let Some(stdout) = child.take_stdout() else {
        child.terminate_and_reap().await;
        return Err(CommandExecutionError::Execution(io::Error::other(
            "stdout pipe was not captured",
        )));
    };
    let Some(stderr) = child.take_stderr() else {
        child.terminate_and_reap().await;
        return Err(CommandExecutionError::Execution(io::Error::other(
            "stderr pipe was not captured",
        )));
    };

    let capture = async {
        // Leave the direct child unreaped until both pipes reach EOF. Its PID
        // therefore cannot be reused while cleanup still addresses the PGID.
        let (stdout, stderr) = tokio::try_join!(capture_stream(stdout), capture_stream(stderr))?;
        let status = child.wait().await?;
        Ok::<_, io::Error>(CapturedCommand {
            status,
            stdout,
            stderr,
        })
    };

    match timeout(command_timeout, capture).await {
        Ok(Ok(output)) => {
            child.disarm();
            Ok(output)
        }
        Ok(Err(error)) => {
            child.terminate_and_reap().await;
            Err(CommandExecutionError::Execution(error))
        }
        Err(_) => {
            child.terminate_and_reap().await;
            Err(CommandExecutionError::Timeout {
                seconds: command_timeout.as_secs(),
            })
        }
    }
}

async fn capture_stream<R>(mut reader: R) -> io::Result<CapturedStream>
where
    R: AsyncRead + Unpin,
{
    let mut prefix = Vec::with_capacity(OUTPUT_LIMIT_BYTES);
    let mut total_bytes = 0_u64;
    let mut chunk = [0_u8; 8 * 1024];

    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        total_bytes = total_bytes.saturating_add(read as u64);
        let retained = (OUTPUT_LIMIT_BYTES - prefix.len()).min(read);
        prefix.extend_from_slice(&chunk[..retained]);
    }

    Ok(CapturedStream {
        prefix,
        total_bytes,
    })
}

/// Decodes a stream whose retained prefix is capped at `OUTPUT_LIMIT_BYTES`.
/// Bash output may not be valid UTF-8, so decoding is intentionally lossy
/// (`from_utf8_lossy`).
///
/// When the cap trips, a visible marker is appended naming the stream and the
/// byte scale, so the model knows it holds only the head of the stream and must
/// not assume it saw everything. How to get the rest (filter, redirect, re-run)
/// is left to the model — bash is a general shell.
fn output_text(stream: &str, captured: &CapturedStream) -> (String, bool) {
    if !captured.truncated() {
        return (
            String::from_utf8_lossy(&captured.prefix).into_owned(),
            false,
        );
    }

    // The retained byte prefix may end inside a code point; `from_utf8_lossy`
    // turns that partial trailing sequence into U+FFFD.
    let mut text = String::from_utf8_lossy(&captured.prefix).into_owned();
    text.push_str(&format!(
        "\n…⟨kuncode: {stream} truncated — showed first {OUTPUT_LIMIT_BYTES} of {total} bytes⟩",
        total = captured.total_bytes,
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
    use std::{sync::Arc, time::Duration};

    use tokio::io::AsyncReadExt;

    use super::{
        Bash, CapturedStream, OUTPUT_LIMIT_BYTES, capture_stream, output_text, simple_command_chain,
    };
    use crate::{
        test_support::TestDir,
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
    async fn stream_capture_retains_only_the_bounded_prefix() {
        let total_bytes = (OUTPUT_LIMIT_BYTES as u64) * 50;
        let input = tokio::io::repeat(b'x').take(total_bytes);

        let captured = capture_stream(input)
            .await
            .expect("in-memory stream should be readable");

        assert_eq!(captured.prefix.len(), OUTPUT_LIMIT_BYTES);
        assert_eq!(captured.total_bytes, total_bytes);
        assert!(captured.prefix.iter().all(|byte| *byte == b'x'));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn drains_large_stdout_and_stderr_concurrently() {
        let bash = bash().await;
        let out = bash
            .run_command(
                "(head -c 2000000 /dev/zero | tr '\\000' o) & \
                 (head -c 2000000 /dev/zero | tr '\\000' e >&2) & wait"
                    .to_string(),
                super::COMMAND_TIMEOUT,
            )
            .await;

        assert!(out.ok);
        assert!(out.truncated);
        let data = out.data.expect("data present");
        assert!(data.stdout.starts_with(&"o".repeat(OUTPUT_LIMIT_BYTES)));
        assert!(data.stdout.contains("of 2000000 bytes"));
        assert!(data.stderr.starts_with(&"e".repeat(OUTPUT_LIMIT_BYTES)));
        assert!(data.stderr.contains("of 2000000 bytes"));
    }

    #[test]
    fn output_decoding_remains_lossy_without_rewriting_controls() {
        let captured = CapturedStream {
            prefix: vec![b'a', 0xff, 0x1b],
            total_bytes: 3,
        };

        assert_eq!(output_text("stdout", &captured), ("a�\u{1b}".into(), false));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_terminates_descendants() {
        let tmp = TestDir::new();
        let bash = Bash::new(tmp.workspace().await);
        let out = bash
            .run_command(
                "(printf ready > started; \
                 while [ ! -e release ]; do sleep 0.01; done; \
                 printf survived > descendant-survived) & wait"
                    .to_string(),
                Duration::from_secs(1),
            )
            .await;

        assert!(!out.ok);
        assert_eq!(out.error.expect("error payload").kind.as_str(), "timeout");
        assert!(tmp.path().join("started").exists());
        release_and_assert_descendant_stopped(&tmp).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_terminates_descendants() {
        let tmp = TestDir::new();
        let bash = Arc::new(Bash::new(tmp.workspace().await));
        let task = tokio::spawn({
            let bash = Arc::clone(&bash);
            async move {
                bash.run_command(
                    "(printf ready > started; \
                     while [ ! -e release ]; do sleep 0.01; done; \
                     printf survived > descendant-survived) & wait"
                        .to_string(),
                    Duration::from_secs(30),
                )
                .await
            }
        });

        wait_for_path(&tmp.path().join("started")).await;
        task.abort();
        assert!(
            task.await
                .expect_err("aborted execution should be cancelled")
                .is_cancelled()
        );
        release_and_assert_descendant_stopped(&tmp).await;
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

    #[cfg(unix)]
    async fn wait_for_path(path: &std::path::Path) {
        for _ in 0..200 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{} should have been created", path.display());
    }

    #[cfg(unix)]
    async fn release_and_assert_descendant_stopped(tmp: &TestDir) {
        std::fs::write(tmp.path().join("release"), b"go").expect("release gate should be created");
        let survivor = tmp.path().join("descendant-survived");
        for _ in 0..100 {
            assert!(!survivor.exists(), "descendant survived process-group kill");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!survivor.exists(), "descendant survived process-group kill");
    }
}
