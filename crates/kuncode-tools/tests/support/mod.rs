use std::{fs, path::Path, process::Command};

use futures_util::{StreamExt, pin_mut};
use kuncode_core::{EventId, RunId, ToolCapability};
use kuncode_events::{
    ArtifactRecord, EventEnvelope, EventKind, EventLogReader, FileArtifactStore, JsonlEventSink, RunDir,
};
use kuncode_tools::{Tool, ToolContext, ToolError, ToolInput, ToolLimits, ToolResult, ToolRuntime};
use kuncode_workspace::{ExecutionLane, Workspace, WorkspaceConfig};
use tempfile::{TempDir, tempdir};
use tokio::fs as tokio_fs;
use tokio_util::sync::CancellationToken;

pub struct ToolFixture {
    _temp: TempDir,
    pub run_id: RunId,
    pub run_dir: RunDir,
    pub workspace: Workspace,
    pub lane: ExecutionLane,
    pub sink: JsonlEventSink,
    artifact_store: FileArtifactStore,
}

impl ToolFixture {
    pub async fn new() -> Self {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("workspace");
        let home = temp.path().join("home");
        fs::create_dir(&root).expect("create workspace");
        fs::create_dir(&home).expect("create home");
        let workspace = Workspace::open(&root, WorkspaceConfig::default()).await.expect("workspace");
        let lane = ExecutionLane::main(&workspace);
        let run_id = RunId::new();
        let run_dir = RunDir::create(&home, run_id).await.expect("run_dir");
        let sink = JsonlEventSink::start(run_dir.clone()).await.expect("sink");
        let artifact_store = FileArtifactStore::new(run_dir.clone());
        Self { _temp: temp, run_id, run_dir, workspace, lane, sink, artifact_store }
    }

    pub fn root(&self) -> &Path {
        self.workspace.root()
    }

    pub fn context(&self, cancel: CancellationToken, limits: ToolLimits) -> ToolContext<'_> {
        ToolContext {
            run_id: self.run_id,
            agent_id: None,
            turn_id: None,
            source_event_id: EventId::new(),
            workspace: &self.workspace,
            lane: &self.lane,
            event_sink: self.sink.handle(),
            artifact_store: Some(&self.artifact_store),
            cancel_token: cancel,
            limits,
        }
    }

    pub async fn run_tool<T: Tool + 'static>(
        &self,
        tool: T,
        input: ToolInput,
        capabilities: &[ToolCapability],
        limits: ToolLimits,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let mut runtime = ToolRuntime::new();
        runtime.register(Box::new(tool)).expect("register tool");
        runtime.execute(input, self.context(cancel, limits), capabilities).await
    }

    pub async fn drain(self) -> Vec<EventEnvelope> {
        self.sink.shutdown().await.expect("shutdown");
        let reader = EventLogReader::for_run_dir(&self.run_dir);
        let stream = reader.stream();
        pin_mut!(stream);
        let mut out = Vec::new();
        while let Some(item) = stream.next().await {
            out.push(item.expect("event"));
        }
        out
    }

    #[allow(dead_code)]
    pub async fn artifact_records(&self) -> Vec<ArtifactRecord> {
        let content = tokio_fs::read_to_string(self.run_dir.artifacts_index_path()).await.expect("artifact index");
        content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("artifact record"))
            .collect()
    }
}

pub fn tool_kinds(events: &[EventEnvelope]) -> Vec<EventKind> {
    events
        .iter()
        .filter(|event| {
            matches!(
                event.kind,
                EventKind::ToolStarted | EventKind::ToolCompleted | EventKind::ToolFailed | EventKind::ToolCancelled
            )
        })
        .map(|event| event.kind)
        .collect()
}

#[allow(dead_code)]
pub fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "KunCode Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "KunCode Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?} failed\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
