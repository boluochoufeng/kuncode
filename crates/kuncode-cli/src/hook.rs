//! Out-of-process loop hooks driven by a shell command.
//!
//! Mirrors the [`TerminalApprover`](crate::approver::TerminalApprover) layering:
//! the agent defines the [`Hook`] trait, the CLI implements a configurable one.
//! Each [`CommandHook`] is bound to a single trigger point and runs its command
//! per fire, feeding `cx.payload()` on stdin and reading the decision back from
//! the exit code.
//!
//! Exit-code protocol, per point:
//! - `0` — go ahead (`Proceed`/`Allow`); non-empty stdout becomes the additive
//!   contribution where the point has one (`AddContext` / `AddFeedback`).
//! - `2` — the point's veto (`Deny` / `Block` / `Continue`), stdout the message.
//!   `PostToolUse` has no veto, so `2` is treated as a command failure.
//! - anything else, a spawn failure, or a timeout — a *command failure*, which
//!   converges to a safe default per point (see [`HookConfig::fail_closed`]).

use std::{process::Stdio, sync::Arc, time::Duration};

use async_trait::async_trait;
use kuncode_agent::hook::{
    Hook, Hooks, PostToolCx, PostToolOutcome, PreToolCx, PreToolOutcome, PromptCx, PromptOutcome,
    StopCx, StopOutcome,
};
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{ChildStdout, Command},
};

/// How long a hook command may run before it is killed and treated as failed.
const HOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// Cap on how much hook stdout we capture. A hook's decision message or injected
/// context should be small; beyond this we truncate (and warn) rather than let a
/// runaway command balloon memory and pollute every later model call by riding
/// the transcript as `AddContext`/`AddFeedback`. Overflow is drained, not left
/// in the pipe, so the child still finishes instead of blocking on a full pipe.
const MAX_HOOK_STDOUT: usize = 16 * 1024;

/// Which loop seam a [`CommandHook`] is bound to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookPoint {
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    Stop,
}

/// One validated hook configuration, produced by settings parsing.
///
/// `fail_closed` is already resolved against the per-point default (true for
/// `PreToolUse`, false elsewhere) and validated (rejected for `PostToolUse`,
/// which has no veto outcome), so the runtime side never re-derives policy.
#[derive(Clone, Debug)]
pub struct HookConfig {
    /// The seam this hook fires on.
    pub point: HookPoint,
    /// Shell command line, run via `sh -c`.
    pub command: String,
    /// Tool-name matcher for `PreToolUse`/`PostToolUse` (`|`-separated,
    /// case-insensitive exact match); `None` matches every call. Ignored for
    /// point-only seams.
    pub matcher: Option<String>,
    /// On command failure, take the point's veto outcome instead of proceeding.
    pub fail_closed: bool,
}

/// Builds a [`Hooks`] set from validated configs, preserving order.
pub fn build_hooks(configs: Vec<HookConfig>) -> Hooks {
    let mut hooks = Hooks::new();
    for config in configs {
        hooks.push(Arc::new(CommandHook::new(config)));
    }
    hooks
}

/// A [`Hook`] backed by a subprocess. Bound to one [`HookPoint`]; the other
/// trait methods stay no-ops.
pub struct CommandHook {
    point: HookPoint,
    command: String,
    matcher: Option<String>,
    fail_closed: bool,
    timeout: Duration,
}

/// What a hook command's run resolved to.
enum CommandResult {
    /// Exit 0 — proceed, with trimmed stdout if any.
    GoAhead(Option<String>),
    /// Exit 2 — a deliberate veto, with stdout as the message.
    Veto(String),
    /// Spawn failure, timeout, or any other exit code — a command failure
    /// (distinct from a deliberate veto).
    Failed,
}

impl CommandHook {
    /// Builds a hook from a validated config with the default timeout.
    pub fn new(config: HookConfig) -> Self {
        Self {
            point: config.point,
            command: config.command,
            matcher: config.matcher,
            fail_closed: config.fail_closed,
            timeout: HOOK_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Whether this hook's matcher applies to `tool`. Point-only seams never
    /// reach here.
    ///
    /// Case-insensitive exact match against the registered tool name (`bash`,
    /// `read_file`, `write_file`, `edit_file`, `glob`), with `|`-separated
    /// alternatives. Case folding is a courtesy for users porting Claude Code
    /// configs (`Bash` → `bash`); it does not bridge name *differences* like
    /// `Edit` vs `edit_file`, so the documented names are the real ones.
    fn matches(&self, tool: &str) -> bool {
        match &self.matcher {
            None => true,
            Some(matcher) => matcher
                .split('|')
                .any(|name| name.trim().eq_ignore_ascii_case(tool)),
        }
    }

    /// Runs the command with `payload` on stdin and classifies the result.
    ///
    /// Never panics: a spawn error, a non-zero/odd exit, or a timeout all map to
    /// [`CommandResult::Failed`]. On *every* exit path — timeout, cancel, or a
    /// clean exit — the whole process *group* is killed (see [`KillGroupOnDrop`]),
    /// so anything the shell forked — a backgrounded process, a pipeline, a
    /// script's children — is reaped too, not just the direct `sh`. Stdout is
    /// capped at [`MAX_HOOK_STDOUT`].
    async fn run(&self, payload: &Value) -> CommandResult {
        let input = payload.to_string();
        let mut child = match Command::new("sh")
            .arg("-c")
            .arg(&self.command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            // Put the shell in its own process group so we can kill the whole
            // tree below; `kill_on_drop` alone only SIGKILLs the direct `sh`.
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(err) => {
                tracing::warn!(command = %self.command, error = %err, "hook command failed to spawn");
                return CommandResult::Failed;
            }
        };

        // The leader pid is the new group's pgid; we kill the *group* on the way
        // out. The guard covers the cancel path (if our future is dropped, Drop
        // still tears the group down); the normal and timeout paths kill
        // explicitly too — on EVERY path, so a hook that backgrounds a process
        // (`cmd & exit 0`) can't leak it past its own decision.
        let pgid = child.id().map(|pid| pid as i32);
        let _group = KillGroupOnDrop(pgid);

        // Feed stdin from its own task so the timeout below actually covers it.
        // A child that never reads stdin would otherwise block `write_all` on a
        // full pipe forever — *before* `timeout` starts — and large payloads
        // (e.g. a `write_file` body in `args`) hit that easily. Running the
        // write concurrently with draining stdout also avoids a both-pipes-full
        // deadlock.
        let stdin = child.stdin.take();
        let writer = tokio::spawn(async move {
            if let Some(mut stdin) = stdin {
                // Best-effort: a hook that ignores stdin must not fail the call.
                let _ = stdin.write_all(input.as_bytes()).await;
                // Dropping closes the pipe so the child sees EOF.
            }
        });

        // Read stdout concurrently with waiting for the *shell* to exit, and key
        // completion on the shell — NOT on stdout EOF, which a backgrounded child
        // can hold open forever (`cmd & exit 0`). The select is *fair* (not
        // biased): a background process flooding stdout (`yes & exit 0`) keeps the
        // read arm perpetually ready, and a biased read arm would starve the wait
        // arm and never observe the clean exit, timing out as a false failure.
        // Once the shell exits we kill the group (releasing any background hold on
        // the pipe) and drain the now-bounded remainder, so we don't lose what the
        // shell itself wrote before exiting. The future moves the child in, so a
        // timeout drops (and kills) it; the group guard finishes the teardown.
        let mut stdout = child.stdout.take();
        let collect = async move {
            let mut bytes = Vec::new();
            let mut truncated = false;
            let mut chunk = [0u8; 4096];
            let wait = child.wait();
            tokio::pin!(wait);
            let status = loop {
                tokio::select! {
                    // Drain stdout while it flows; once it ends (EOF/error) stop
                    // reading but keep waiting for the shell to exit.
                    read = read_chunk(stdout.as_mut(), &mut chunk), if stdout.is_some() => {
                        match read {
                            Ok(0) | Err(_) => stdout = None,
                            Ok(n) => append_capped(&mut bytes, &mut truncated, &chunk[..n]),
                        }
                    }
                    status = &mut wait => break status,
                }
            };
            // Shell exited. Kill the group so any background fd-holder releases the
            // stdout write end, then drain the now-bounded remainder to EOF so the
            // shell's own output isn't lost to the race with its exit.
            if let Some(pgid) = pgid {
                // SAFETY: see `KillGroupOnDrop::drop`.
                unsafe { libc::killpg(pgid, libc::SIGKILL) };
            }
            if let Some(mut out) = stdout {
                while let Ok(n) = out.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }
                    append_capped(&mut bytes, &mut truncated, &chunk[..n]);
                }
            }
            (status, bytes, truncated)
        };

        let result = tokio::time::timeout(self.timeout, collect).await;
        // Stop the writer: on success it has already finished (the child exited),
        // so this is a no-op; on timeout/error it may still be blocked on a full
        // pipe, so abort it rather than leak the task.
        writer.abort();

        let (status, bytes, truncated) = match result {
            Ok((Ok(status), bytes, truncated)) => (status, bytes, truncated),
            Ok((Err(err), _, _)) => {
                // Wait failed; the group guard still kills the tree on return.
                tracing::warn!(command = %self.command, error = %err, "hook command failed");
                return CommandResult::Failed;
            }
            Err(_) => {
                // Timed out: `collect` is dropped (killing the direct child); the
                // group guard kills any descendants when this frame returns.
                tracing::warn!(command = %self.command, "hook command timed out");
                return CommandResult::Failed;
            }
        };

        let mut stdout = String::from_utf8_lossy(&bytes).trim().to_string();
        if truncated {
            tracing::warn!(command = %self.command, limit = MAX_HOOK_STDOUT, "hook stdout truncated");
            stdout.push_str("\n…[truncated]");
        }
        match status.code() {
            Some(0) => CommandResult::GoAhead((!stdout.is_empty()).then_some(stdout)),
            Some(2) => CommandResult::Veto(stdout),
            other => {
                tracing::warn!(command = %self.command, code = ?other, "hook command returned a failure code");
                CommandResult::Failed
            }
        }
    }

    /// Message attached to a veto synthesized from a *command failure* (as
    /// opposed to a deliberate exit-2 veto, which carries the command's stdout).
    fn failure_message(&self) -> String {
        format!("hook command failed (fail-closed): {}", self.command)
    }
}

/// Reads one chunk from `reader`, or never resolves when there is none. The
/// caller's `select!` arm is guarded by `reader.is_some()`, so the pending
/// branch is unreachable; it only gives the arm a concrete future to name.
async fn read_chunk(reader: Option<&mut ChildStdout>, buf: &mut [u8]) -> std::io::Result<usize> {
    match reader {
        Some(r) => r.read(buf).await,
        None => std::future::pending().await,
    }
}

/// Appends `data` to `buf` up to [`MAX_HOOK_STDOUT`]. Once the cap is hit it sets
/// `truncated` and drops further data — the caller keeps reading (and
/// discarding) so the child can finish writing instead of blocking on a full
/// pipe.
fn append_capped(buf: &mut Vec<u8>, truncated: &mut bool, data: &[u8]) {
    if *truncated {
        return;
    }
    let room = MAX_HOOK_STDOUT - buf.len();
    if data.len() > room {
        buf.extend_from_slice(&data[..room]);
        *truncated = true;
    } else {
        buf.extend_from_slice(data);
    }
}

/// Kills a child's whole process group on drop — on every exit path.
///
/// `kill_on_drop(true)` only SIGKILLs the direct `sh`; anything it forked — a
/// backgrounded process (`cmd &`), a pipeline stage, a script's children —
/// would outlive it. Spawning the shell into its own group (`process_group(0)`)
/// and killing the *group* on drop reaps those descendants on a timeout, a
/// cancel, AND a clean exit alike: a hook is a synchronous decision point, not a
/// way to launch a daemon, so it must not leak a process past its own return.
///
/// There is deliberately no `disarm`. Killing the *group* (not a bare pid) is
/// why pid reuse is a non-issue: the pgid stays valid while any member lives,
/// and a reused leader pid lands in its new parent's group — never back in this
/// one — so `killpg` only ever reaches our own descendants. An empty group is a
/// harmless `ESRCH`.
struct KillGroupOnDrop(Option<i32>);

impl Drop for KillGroupOnDrop {
    fn drop(&mut self) {
        if let Some(pgid) = self.0 {
            // SAFETY: a plain libc call. `pgid` is our own child's pid, used as
            // its process-group id via `process_group(0)`. SIGKILL to a group
            // with no live members is a harmless `ESRCH`, which we ignore.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
    }
}

#[async_trait]
impl Hook for CommandHook {
    async fn user_prompt_submit(&self, cx: &PromptCx<'_>) -> PromptOutcome {
        if self.point != HookPoint::UserPromptSubmit {
            return PromptOutcome::Proceed;
        }
        match self.run(&cx.payload()).await {
            CommandResult::GoAhead(stdout) => {
                stdout.map_or(PromptOutcome::Proceed, PromptOutcome::AddContext)
            }
            CommandResult::Veto(message) => PromptOutcome::Block { reason: message },
            CommandResult::Failed => {
                if self.fail_closed {
                    PromptOutcome::Block {
                        reason: self.failure_message(),
                    }
                } else {
                    PromptOutcome::Proceed
                }
            }
        }
    }

    async fn pre_tool_use(&self, cx: &PreToolCx<'_>) -> PreToolOutcome {
        if self.point != HookPoint::PreToolUse || !self.matches(&cx.request.tool) {
            return PreToolOutcome::Proceed;
        }
        match self.run(&cx.payload()).await {
            CommandResult::GoAhead(_) => PreToolOutcome::Proceed,
            CommandResult::Veto(message) => PreToolOutcome::Deny { message },
            CommandResult::Failed => {
                if self.fail_closed {
                    PreToolOutcome::Deny {
                        message: self.failure_message(),
                    }
                } else {
                    PreToolOutcome::Proceed
                }
            }
        }
    }

    async fn post_tool_use(&self, cx: &PostToolCx<'_>) -> PostToolOutcome {
        if self.point != HookPoint::PostToolUse || !self.matches(cx.tool) {
            return PostToolOutcome::Proceed;
        }
        // PostToolUse has no veto: exit 2 is meaningless here and falls through
        // `run`'s "other code" arm to `Failed`, which (no fail-closed) proceeds.
        match self.run(&cx.payload()).await {
            CommandResult::GoAhead(stdout) => {
                stdout.map_or(PostToolOutcome::Proceed, PostToolOutcome::AddFeedback)
            }
            CommandResult::Veto(_) | CommandResult::Failed => PostToolOutcome::Proceed,
        }
    }

    async fn stop(&self, cx: &StopCx<'_>) -> StopOutcome {
        if self.point != HookPoint::Stop {
            return StopOutcome::Allow;
        }
        match self.run(&cx.payload()).await {
            CommandResult::GoAhead(_) => StopOutcome::Allow,
            CommandResult::Veto(message) => StopOutcome::Continue { message },
            CommandResult::Failed => {
                if self.fail_closed {
                    StopOutcome::Continue {
                        message: self.failure_message(),
                    }
                } else {
                    StopOutcome::Allow
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kuncode_agent::permission::{PermissionAction, PermissionRequest};
    use kuncode_core::completion::Message;

    fn config(point: HookPoint, command: &str, fail_closed: bool) -> HookConfig {
        HookConfig {
            point,
            command: command.to_string(),
            matcher: None,
            fail_closed,
        }
    }

    fn prompt_cx<'a>(messages: &'a [Message]) -> PromptCx<'a> {
        PromptCx {
            prompt: "hi",
            messages,
        }
    }

    fn request() -> PermissionRequest {
        PermissionRequest::new("bash", PermissionAction::Execute, None, "run")
    }

    #[tokio::test]
    async fn prompt_exit_zero_with_stdout_adds_context() {
        let hook = CommandHook::new(config(HookPoint::UserPromptSubmit, "echo CTX", false));
        let messages = Vec::new();
        let outcome = hook.user_prompt_submit(&prompt_cx(&messages)).await;
        assert!(matches!(outcome, PromptOutcome::AddContext(text) if text == "CTX"));
    }

    #[tokio::test]
    async fn pre_tool_exit_two_denies_with_stdout_message() {
        let hook = CommandHook::new(config(HookPoint::PreToolUse, "echo nope; exit 2", false));
        let request = request();
        let messages = Vec::new();
        let cx = PreToolCx {
            request: &request,
            args: &Value::Null,
            messages: &messages,
            iteration: 0,
        };
        let outcome = hook.pre_tool_use(&cx).await;
        assert!(matches!(outcome, PreToolOutcome::Deny { message } if message == "nope"));
    }

    #[tokio::test]
    async fn stop_exit_two_continues() {
        let hook = CommandHook::new(config(HookPoint::Stop, "echo more; exit 2", false));
        let messages = Vec::new();
        let cx = StopCx {
            messages: &messages,
            iteration: 0,
        };
        assert!(matches!(
            hook.stop(&cx).await,
            StopOutcome::Continue { message } if message == "more"
        ));
    }

    #[tokio::test]
    async fn pre_tool_failure_defaults_fail_closed() {
        // Exit 1 is a command failure, not a veto; PreToolUse defaults fail-closed.
        let hook = CommandHook::new(config(HookPoint::PreToolUse, "exit 1", true));
        let request = request();
        let messages = Vec::new();
        let cx = PreToolCx {
            request: &request,
            args: &Value::Null,
            messages: &messages,
            iteration: 0,
        };
        assert!(matches!(
            hook.pre_tool_use(&cx).await,
            PreToolOutcome::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn prompt_failure_is_fail_open_by_default() {
        let hook = CommandHook::new(config(HookPoint::UserPromptSubmit, "exit 1", false));
        let messages = Vec::new();
        assert!(matches!(
            hook.user_prompt_submit(&prompt_cx(&messages)).await,
            PromptOutcome::Proceed
        ));
    }

    #[tokio::test]
    async fn prompt_failure_with_fail_closed_blocks() {
        let hook = CommandHook::new(config(HookPoint::UserPromptSubmit, "exit 1", true));
        let messages = Vec::new();
        assert!(matches!(
            hook.user_prompt_submit(&prompt_cx(&messages)).await,
            PromptOutcome::Block { .. }
        ));
    }

    #[tokio::test]
    async fn post_tool_exit_two_is_a_failure_not_a_veto() {
        use kuncode_agent::tool::ToolOutput;
        // PostToolUse has no veto; exit 2 converges to a fail-open Proceed.
        let hook = CommandHook::new(config(HookPoint::PostToolUse, "exit 2", false));
        let output = ToolOutput::success(serde_json::json!({"ok": true}));
        let messages = Vec::new();
        let cx = PostToolCx {
            tool: "bash",
            output: &output,
            messages: &messages,
            iteration: 0,
        };
        assert!(matches!(
            hook.post_tool_use(&cx).await,
            PostToolOutcome::Proceed
        ));
    }

    #[tokio::test]
    async fn timeout_is_a_failure_and_kills_the_child() {
        let hook = CommandHook::new(config(HookPoint::PreToolUse, "sleep 30", true))
            .with_timeout(Duration::from_millis(100));
        let request = request();
        let messages = Vec::new();
        let cx = PreToolCx {
            request: &request,
            args: &Value::Null,
            messages: &messages,
            iteration: 0,
        };
        // A slow command is killed on timeout and treated as a (fail-closed) deny.
        assert!(matches!(
            hook.pre_tool_use(&cx).await,
            PreToolOutcome::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn large_payload_to_a_stdin_ignoring_child_still_times_out() {
        // Regression for the stdin-outside-timeout bug: a child that never reads
        // stdin plus a payload larger than the pipe buffer would wedge
        // `write_all` forever if the write ran before the timeout. With the
        // write moved into its own task the timeout still fires. (On the old
        // code this test would hang.)
        let hook = CommandHook::new(config(HookPoint::PreToolUse, "sleep 30", true))
            .with_timeout(Duration::from_millis(100));
        let request = request();
        let big = serde_json::json!({ "content": "x".repeat(200_000) });
        let messages = Vec::new();
        let cx = PreToolCx {
            request: &request,
            args: &big,
            messages: &messages,
            iteration: 0,
        };
        assert!(matches!(
            hook.pre_tool_use(&cx).await,
            PreToolOutcome::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn matcher_limits_which_tools_fire() {
        let hook = CommandHook::new(HookConfig {
            point: HookPoint::PreToolUse,
            command: "exit 2".to_string(),
            matcher: Some("edit|write".to_string()),
            fail_closed: false,
        });
        let request = request(); // tool = "bash", not in the matcher
        let messages = Vec::new();
        let cx = PreToolCx {
            request: &request,
            args: &Value::Null,
            messages: &messages,
            iteration: 0,
        };
        // bash is not matched, so the hook proceeds without running the command.
        assert!(matches!(
            hook.pre_tool_use(&cx).await,
            PreToolOutcome::Proceed
        ));
    }

    #[tokio::test]
    async fn matcher_is_case_insensitive() {
        // A Claude-Code-style `Bash` must still fire against the real `bash`.
        let hook = CommandHook::new(HookConfig {
            point: HookPoint::PreToolUse,
            command: "exit 2".to_string(),
            matcher: Some("Bash".to_string()),
            fail_closed: false,
        });
        let request = request(); // tool = "bash"
        let messages = Vec::new();
        let cx = PreToolCx {
            request: &request,
            args: &Value::Null,
            messages: &messages,
            iteration: 0,
        };
        assert!(matches!(
            hook.pre_tool_use(&cx).await,
            PreToolOutcome::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn timeout_kills_the_whole_process_group() {
        // The shell backgrounds a heartbeat loop (its own child) and then `wait`s,
        // so `sh` itself does not exit and our timeout fires. `kill_on_drop` would
        // reap only `sh`, leaving the loop appending to the heartbeat file; the
        // process-group kill must take it down too. We assert by checking the file
        // stops growing after the timeout.
        let beat = std::env::temp_dir().join(format!("kuncode-hook-hb-{}", std::process::id()));
        let _ = std::fs::remove_file(&beat);
        let command = format!(
            "( while true; do echo x >> {0}; sleep 0.02; done ) & wait",
            beat.display()
        );
        let hook = CommandHook::new(config(HookPoint::PreToolUse, &command, true))
            .with_timeout(Duration::from_millis(200));
        let request = request();
        let messages = Vec::new();
        let cx = PreToolCx {
            request: &request,
            args: &Value::Null,
            messages: &messages,
            iteration: 0,
        };
        assert!(matches!(
            hook.pre_tool_use(&cx).await,
            PreToolOutcome::Deny { .. }
        ));

        // Give any survivor a window to keep writing, then confirm it stopped.
        let size = |p: &std::path::Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        let before = size(&beat);
        assert!(
            before > 0,
            "heartbeat never started — test is not exercising the kill"
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        let after = size(&beat);
        let _ = std::fs::remove_file(&beat);
        assert_eq!(
            before, after,
            "backgrounded heartbeat survived the timeout (orphaned process group)"
        );
    }

    #[tokio::test]
    async fn clean_exit_still_kills_backgrounded_children() {
        // `cmd & exit 0`: the shell exits *cleanly* but leaves a backgrounded
        // child in its group. A decision point must not leak a long-running
        // process, so the group kill on drop must reap it even on success — not
        // just on timeout/cancel. (One synchronous write up front guarantees the
        // heartbeat file exists before sh exits, so the assertion isn't racing
        // the loop's first iteration.)
        let beat = std::env::temp_dir().join(format!("kuncode-hook-clean-{}", std::process::id()));
        let _ = std::fs::remove_file(&beat);
        let command = format!(
            "echo x >> {0}; ( while true; do echo x >> {0}; sleep 0.02; done ) & exit 0",
            beat.display()
        );
        // exit 0 with no stdout → GoAhead(None) → Proceed.
        let hook = CommandHook::new(config(HookPoint::UserPromptSubmit, &command, false));
        let messages = Vec::new();
        assert!(matches!(
            hook.user_prompt_submit(&prompt_cx(&messages)).await,
            PromptOutcome::Proceed
        ));

        let size = |p: &std::path::Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        let before = size(&beat);
        assert!(before > 0, "priming write missing — test setup is wrong");
        tokio::time::sleep(Duration::from_millis(150)).await;
        let after = size(&beat);
        let _ = std::fs::remove_file(&beat);
        assert_eq!(
            before, after,
            "backgrounded child survived a clean hook exit (leaked process)"
        );
    }

    #[tokio::test]
    async fn background_stdout_flood_does_not_starve_clean_exit() {
        // `yes & exit 0`: the shell exits cleanly while a backgrounded `yes`
        // floods stdout, keeping the read arm perpetually ready. A biased read
        // arm would starve the wait arm and time out — and with `fail_closed`
        // that surfaces as `Block`. The fair select must instead observe the real
        // exit 0, promptly. The outcome is then `Proceed` or `AddContext`
        // depending on whether `yes` wrote before the group kill raced it — both
        // fine; `Block` (a timeout) is the regression we're guarding against.
        let hook = CommandHook::new(config(HookPoint::UserPromptSubmit, "yes & exit 0", true))
            .with_timeout(Duration::from_secs(2));
        let messages = Vec::new();
        let outcome = hook.user_prompt_submit(&prompt_cx(&messages)).await;
        assert!(
            matches!(
                outcome,
                PromptOutcome::Proceed | PromptOutcome::AddContext(_)
            ),
            "clean exit was starved by the stdout flood: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn stdout_is_capped() {
        // A hook that floods stdout (100 KiB) must be captured at the cap, drained
        // to let the child exit, and marked truncated — not collected unbounded.
        let hook = CommandHook::new(config(
            HookPoint::UserPromptSubmit,
            "yes | head -c 100000",
            false,
        ));
        let messages = Vec::new();
        match hook.user_prompt_submit(&prompt_cx(&messages)).await {
            PromptOutcome::AddContext(text) => {
                assert!(
                    text.len() <= MAX_HOOK_STDOUT + 16,
                    "stdout not capped: {} bytes",
                    text.len()
                );
                assert!(text.contains("truncated"), "missing truncation marker");
            }
            other => panic!("expected AddContext, got {other:?}"),
        }
    }
}
