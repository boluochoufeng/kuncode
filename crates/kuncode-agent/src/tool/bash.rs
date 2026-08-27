//! Executes approved shell commands with bounded output and descendant cleanup.

use std::{
    num::NonZeroU64,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use kuncode_core::completion::ToolDefinition;
use kuncode_core::non_empty_vec::NonEmptyVec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
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
const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 600;
/// How long to keep reading after the command ends before abandoning a stream
/// that has not reached EOF.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Arguments accepted by the [`Bash`] tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BashArgs {
    /// The shell command to run, e.g. `cargo test --workspace`.
    cmd: String,
    /// Timeout in seconds. Defaults to 120; values above 600 are capped to
    /// 600. When the timeout trips, the whole process group is killed and the
    /// output received up to that point is returned.
    #[schemars(range(max = 600))]
    timeout_secs: Option<NonZeroU64>,
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
        // `timeout_secs` must survive canonicalization: an approval rewrite
        // replays this JSON through `prepare`, so a field left out here would
        // silently reset to the default on the rewritten call.
        let mut canonical = serde_json::Map::new();
        canonical.insert("cmd".into(), args.cmd.clone().into());
        if let Some(timeout_secs) = args.timeout_secs {
            canonical.insert("timeout_secs".into(), timeout_secs.get().into());
        }
        let canonical_input = CanonicalToolInput::new(serde_json::Value::Object(canonical));
        Ok(TypedPreparation::new(
            args,
            canonical_input,
            checks,
            ToolDisplay::new(format!("Run shell command: {program}")),
        ))
    }

    async fn run_prepared(&self, prepared: BashArgs, _ctx: &ToolContext) -> ToolOutput<BashOutput> {
        let BashArgs { cmd, timeout_secs } = prepared;
        let limit = effective_timeout(timeout_secs);

        let mut command = Command::new("bash");
        command
            .arg("-lc")
            .arg(&cmd)
            .current_dir(self.workspace.root())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Make bash the leader of its own process group so a timeout can kill
        // its grandchildren too: they inherit the pipe write ends, and until
        // every holder is dead the pumps never see EOF.
        #[cfg(unix)]
        command.process_group(0);

        let child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                return ToolOutput::failure("execution", format!("failed to run command: {err}"));
            }
        };
        let mut child = ManagedChild::new(child);
        let stdout_pump = Pump::start(child.take_stdout());
        let stderr_pump = Pump::start(child.take_stderr());

        let (status, timed_out) = match timeout(limit, child.wait()).await {
            Ok(Ok(status)) => {
                child.disarm();
                (Some(status), false)
            }
            Ok(Err(err)) => {
                child.terminate_and_reap().await;
                return ToolOutput::failure("execution", format!("failed to run command: {err}"));
            }
            Err(_) => {
                child.terminate_and_reap().await;
                (None, true)
            }
        };

        // Drained together so two never-closing streams cost one grace
        // period, not two.
        let (stdout_capture, stderr_capture) =
            tokio::join!(stdout_pump.drain(), stderr_pump.drain());
        let (stdout, stdout_truncated) = output_text("stdout", stdout_capture);
        let (stderr, stderr_truncated) = output_text("stderr", stderr_capture);
        let ok = !timed_out && status.is_some_and(|status| status.success());
        let exit_code = status.and_then(|status| status.code());

        ToolOutput {
            ok,
            data: Some(BashOutput {
                cmd,
                exit_code,
                stdout,
                stderr,
            }),
            error: if timed_out {
                Some(ToolErrorPayload {
                    kind: "timeout".into(),
                    message: format!(
                        "command killed after exceeding its {}s timeout; stdout/stderr \
                         received up to the kill are included",
                        limit.as_secs()
                    ),
                })
            } else if ok {
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
            truncated: stdout_truncated || stderr_truncated || timed_out,
        }
    }
}

/// Keeps process-group cleanup armed until the direct child has been reaped.
///
/// The guard's synchronous [`Drop`] path covers task cancellation, where there
/// is no opportunity to await explicit cleanup.
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

    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }

    fn signal_termination(&mut self) {
        #[cfg(unix)]
        if let Some(process_group) = self.process_group {
            // SAFETY: spawn configured the child as leader of this isolated
            // process group; killpg only reads the scalar group id and signal.
            let _ = unsafe { libc::killpg(process_group, libc::SIGKILL) };
        }

        // Also covers non-Unix targets and a failed process-group signal.
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

/// Resolves the effective timeout: default 120s, capped at 600s. The cap is a
/// clamp rather than a rejection — the schema advertises the maximum, so an
/// oversized request costs a shorter wait, not a wasted round-trip.
fn effective_timeout(timeout_secs: Option<NonZeroU64>) -> Duration {
    Duration::from_secs(timeout_secs.map_or(DEFAULT_TIMEOUT_SECS, |secs| {
        secs.get().min(MAX_TIMEOUT_SECS)
    }))
}

/// What a [`Pump`] captured from one stream: the retained head, the total byte
/// count, and whether the stream actually closed.
#[derive(Debug, Default)]
struct StreamCapture {
    head: Vec<u8>,
    total: usize,
    closed: bool,
}

/// Background reader for one output stream.
///
/// Bytes past `OUTPUT_LIMIT_BYTES` are counted and dropped as they arrive, so
/// the pipe never backpressures the child and memory stays bounded no matter
/// how much a command prints. The capture lives behind a shared handle rather
/// than in the task, so whatever arrived stays reachable even when the reader
/// has to be abandoned.
struct Pump {
    capture: Arc<Mutex<StreamCapture>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Pump {
    fn start(stream: Option<impl AsyncRead + Send + Unpin + 'static>) -> Self {
        let capture = Arc::new(Mutex::new(StreamCapture::default()));
        let task = stream.map(|mut stream| {
            let capture = Arc::clone(&capture);
            tokio::spawn(async move {
                let mut chunk = vec![0u8; 8 * 1024];
                loop {
                    match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            let mut capture = capture.lock().expect("pump mutex");
                            capture.total += read;
                            let room = OUTPUT_LIMIT_BYTES.saturating_sub(capture.head.len());
                            capture.head.extend_from_slice(&chunk[..read.min(room)]);
                        }
                    }
                }
                capture.lock().expect("pump mutex").closed = true;
            })
        });
        Self { capture, task }
    }

    /// Waits briefly for EOF, then returns whatever was captured.
    ///
    /// The grace period covers the normal case — the command is dead, the
    /// write ends are closed, EOF is instants away — while bounding the wait
    /// when an escaped process still holds a write end (a daemon that left the
    /// process group, or a backgrounded child that outlived a successful exit).
    async fn drain(self) -> StreamCapture {
        if let Some(task) = self.task {
            let abort = task.abort_handle();
            if timeout(DRAIN_GRACE, task).await.is_err() {
                abort.abort();
            }
        }
        let mut capture = self.capture.lock().expect("pump mutex");
        std::mem::take(&mut *capture)
    }
}

/// Renders a captured stream, appending a visible marker for anything the text
/// alone would misrepresent. Bash output may not be valid UTF-8, so decoding is
/// intentionally lossy (`from_utf8_lossy`).
///
/// Two conditions earn a marker. Bytes past `OUTPUT_LIMIT_BYTES` were dropped
/// as they arrived — the marker names the stream and the byte scale, so the
/// model knows it holds only the head and must not assume it saw everything;
/// how to get the rest (filter, redirect, re-run) is left to the model — bash
/// is a general shell. And a stream that never closed means some process the
/// kill could not reach may still be writing — the marker says the text is only
/// what had arrived by the time the pump was abandoned.
fn output_text(stream: &str, capture: StreamCapture) -> (String, bool) {
    // The head is cut at an arbitrary byte index, which never splits a `char`
    // (that is a `str` concern); `from_utf8_lossy` turns any partial trailing
    // sequence into U+FFFD, so the result is always valid UTF-8.
    let mut text = String::from_utf8_lossy(&capture.head).into_owned();
    let mut truncated = false;
    if capture.total > capture.head.len() {
        text.push_str(&format!(
            "\n…⟨kuncode: {stream} truncated — showed first {shown} of {total} bytes⟩",
            shown = capture.head.len(),
            total = capture.total,
        ));
        truncated = true;
    }
    if !capture.closed {
        text.push_str(&format!(
            "\n…⟨kuncode: {stream} did not close — a surviving process may still \
             be holding it; shown is what had arrived so far⟩"
        ));
        truncated = true;
    }
    (text, truncated)
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

    #[cfg(unix)]
    use std::time::Duration;

    use super::{Bash, simple_command_chain};
    #[cfg(unix)]
    use crate::test_support::TestDir;
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
        // The timeout cap must be advertised so the model doesn't have to
        // discover it by clamping.
        assert_eq!(params["properties"]["timeout_secs"]["maximum"], 600);
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

    #[cfg(unix)]
    #[tokio::test]
    async fn drains_large_stdout_and_stderr_concurrently() {
        let out = execute_for_test(
            Arc::new(bash().await),
            serde_json::json!({
                "cmd": "(head -c 2000000 /dev/zero | tr '\\000' o) & \
                         (head -c 2000000 /dev/zero | tr '\\000' e >&2) & wait"
            }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness-level error");

        assert!(out.ok);
        assert!(out.truncated);
        let data = out.data.expect("data present");
        let stdout = data["stdout"].as_str().expect("stdout is a string");
        let stderr = data["stderr"].as_str().expect("stderr is a string");
        assert!(stdout.starts_with(&"o".repeat(super::OUTPUT_LIMIT_BYTES)));
        assert!(stdout.contains("of 2000000 bytes"));
        assert!(stderr.starts_with(&"e".repeat(super::OUTPUT_LIMIT_BYTES)));
        assert!(stderr.contains("of 2000000 bytes"));
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

    #[tokio::test]
    async fn timeout_returns_partial_output_instead_of_discarding_it() {
        let out = execute_for_test(
            Arc::new(bash().await),
            serde_json::json!({ "cmd": "echo started; sleep 30", "timeout_secs": 1 }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness-level error");

        assert!(!out.ok);
        assert!(out.truncated);
        let error = out.error.expect("error payload");
        assert_eq!(error.kind.as_str(), "timeout");
        // The output produced before the kill must survive it.
        let data = out.data.expect("data present");
        assert_eq!(data["exit_code"], serde_json::Value::Null);
        assert!(
            data["stdout"]
                .as_str()
                .expect("stdout is a string")
                .contains("started")
        );
    }

    #[tokio::test]
    async fn timeout_kills_grandchildren_holding_the_pipes() {
        // The backgrounded sleep inherits stdout. If the kill only reached
        // bash, the pump would wait out the drain grace and flag the stream
        // as never closed.
        let out = execute_for_test(
            Arc::new(bash().await),
            serde_json::json!({ "cmd": "sleep 30 & echo bg; wait", "timeout_secs": 1 }),
            &ToolContext::new(),
        )
        .await
        .expect("no harness-level error");

        assert!(!out.ok);
        let data = out.data.expect("data present");
        let stdout = data["stdout"].as_str().expect("stdout is a string");
        assert!(stdout.contains("bg"));
        assert!(!stdout.contains("did not close"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_terminates_descendants() {
        let tmp = TestDir::new();
        let bash = Arc::new(Bash::new(tmp.workspace().await));
        let task = tokio::spawn({
            let bash = Arc::clone(&bash);
            async move {
                execute_for_test(
                    bash,
                    serde_json::json!({
                        "cmd": "(printf ready > started; \
                                 while [ ! -e release ]; do sleep 0.01; done; \
                                 printf survived > descendant-survived) & wait"
                    }),
                    &ToolContext::new(),
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
    async fn flags_a_stream_left_open_by_a_surviving_background_process() {
        // bash exits at once, but the backgrounded sleep keeps the inherited
        // stdout open past the drain grace, so the pump is abandoned and the
        // output marked as possibly incomplete. stderr is redirected away so
        // only the stdout pump has to wait out the grace.
        let out = execute_for_test(
            Arc::new(bash().await),
            serde_json::json!({ "cmd": "sleep 5 2>/dev/null & echo hi" }),
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
        assert!(stdout.contains("hi"));
        assert!(stdout.contains("did not close"));
    }

    #[tokio::test]
    async fn canonical_input_preserves_the_timeout_for_approval_rewrites() {
        let preparation = Arc::new(bash().await)
            .prepare(
                serde_json::json!({ "cmd": "true", "timeout_secs": 30 }),
                &crate::tool::PreparationContext::new(),
            )
            .await
            .expect("prepares cleanly");

        assert_eq!(
            preparation.canonical_input().as_value(),
            &serde_json::json!({ "cmd": "true", "timeout_secs": 30 })
        );
    }

    #[test]
    fn timeout_defaults_and_caps() {
        use std::num::NonZeroU64;

        use super::effective_timeout;

        assert_eq!(effective_timeout(None).as_secs(), 120);
        assert_eq!(effective_timeout(NonZeroU64::new(30)).as_secs(), 30);
        assert_eq!(effective_timeout(NonZeroU64::new(9_999)).as_secs(), 600);
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
