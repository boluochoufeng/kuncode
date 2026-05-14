//! `ToolRuntime`: the registry + dispatcher. See Phase 2 plan §11.
//!
//! The runtime is the only component that:
//!
//! 1. Validates `ToolDescriptor` and compiles its `input_schema` at
//!    registration time.
//! 2. Runs schema validation, capability gate and lifecycle event emission
//!    around every call (in this fixed order per §11.2).
//! 3. Owns the `tool.started`/`tool.completed`/`tool.failed`/`tool.cancelled`
//!    quartet — tools never emit these themselves.

use std::collections::HashMap;

use kuncode_core::{AgentId, RunId, ToolCapability, ToolRequestId, TurnId};
use kuncode_events::{
    EventEnvelope, EventKind, EventLogError, EventSink, EventSinkHandle, ToolCancelled, ToolCompleted, ToolFailed,
    ToolStarted,
};
use serde::Serialize;
use thiserror::Error;

use crate::{
    CompiledSchema, Tool, ToolContext, ToolDescriptor, ToolError, ToolInput, ToolResult, descriptor::DescriptorError,
    is_allowed, result::SUMMARY_MAX_CHARS, schema::SchemaError,
};

#[derive(Debug, Error)]
pub enum RegisterError {
    #[error("tool `{name}` is already registered")]
    DuplicateName { name: String },

    #[error("invalid descriptor for tool `{name}`: {source}")]
    Descriptor {
        /// May be empty when the descriptor itself is missing a name.
        name: String,
        #[source]
        source: DescriptorError,
    },

    #[error("input_schema for tool `{name}` failed to compile: {source}")]
    Schema {
        name: String,
        #[source]
        source: SchemaError,
    },
}

/// One registered tool + its precompiled input schema.
struct Entry {
    tool: Box<dyn Tool>,
    schema: CompiledSchema,
}

struct ToolRegistry {
    entries: Vec<Entry>,
    index_by_name: HashMap<String, usize>,
}

impl ToolRegistry {
    fn new() -> Self {
        Self { entries: Vec::new(), index_by_name: HashMap::new() }
    }

    fn contains_name(&self, name: &str) -> bool {
        self.index_by_name.contains_key(name)
    }

    fn insert(&mut self, name: String, entry: Entry) {
        let index = self.entries.len();
        self.entries.push(entry);
        self.index_by_name.insert(name, index);
    }

    fn get(&self, name: &str) -> Option<&Entry> {
        let index = self.index_by_name.get(name)?;
        self.entries.get(*index)
    }

    fn descriptors(&self) -> impl Iterator<Item = &ToolDescriptor> {
        self.entries.iter().map(|entry| entry.tool.descriptor())
    }
}

pub struct ToolRuntime {
    registry: ToolRegistry,
}

impl ToolRuntime {
    pub fn new() -> Self {
        Self { registry: ToolRegistry::new() }
    }

    /// Register a tool. Performs descriptor validation, schema compilation,
    /// duplicate-name rejection. See plan §11.1.
    pub fn register(&mut self, tool: Box<dyn Tool>) -> Result<(), RegisterError> {
        let descriptor = tool.descriptor();
        let tool_name = descriptor.name.clone();
        if self.registry.contains_name(&tool_name) {
            return Err(RegisterError::DuplicateName { name: tool_name.clone() });
        }

        descriptor.validate().map_err(|e| RegisterError::Descriptor { name: tool_name.clone(), source: e })?;

        let schema = CompiledSchema::compile(&descriptor.input_schema)
            .map_err(|e| RegisterError::Schema { name: tool_name.clone(), source: e })?;

        self.registry.insert(tool_name, Entry { tool, schema });

        Ok(())
    }

    /// Dispatch one `ToolInput`. The fixed execution order is:
    ///
    /// 1. Lookup by `input.name` → `UnknownTool` on miss.
    /// 2. Validate `input.payload` against the precompiled schema →
    ///    `InvalidInput` on mismatch.
    /// 3. Capability gate (`descriptor.default_capabilities` ∩ `granted` non-empty)
    ///    → `CapabilityDenied` on no overlap.
    /// 4. Emit `tool.started`; capture its `event_id` and write it into
    ///    `ctx.source_event_id` before calling `Tool::execute`.
    /// 5. On success: re-check `result.summary` ≤ `SUMMARY_MAX_CHARS`
    ///    (`ResultTooLarge` on overflow) and emit `tool.completed`.
    /// 6. On `ToolError::Cancelled` *or* fired `cancel_token`: emit
    ///    `tool.cancelled`.
    /// 7. On any other error: emit `tool.failed`.
    ///
    /// Steps 1–3 must **not** emit any event — failures before `tool.started`
    /// leave the event log untouched.
    pub async fn execute(
        &self,
        input: ToolInput,
        mut ctx: ToolContext<'_>,
        granted: &[ToolCapability],
    ) -> Result<ToolResult, ToolError> {
        let tool_name = input.name.clone();
        let request_id = input.request_id;
        let entry = self.registry.get(&input.name).ok_or(ToolError::UnknownTool { name: tool_name.clone() })?;
        entry
            .schema
            .validate(&input.payload)
            .map_err(|e| ToolError::InvalidInput { tool: tool_name.clone(), message: e.to_string() })?;

        let descriptor = entry.tool.descriptor();
        if !is_allowed(&descriptor.default_capabilities, granted) {
            return Err(ToolError::CapabilityDenied {
                tool: tool_name,
                required: descriptor.default_capabilities.iter().copied().map(capability_wire).collect(),
                granted: granted.iter().copied().map(capability_wire).collect(),
            });
        }

        let lifecycle = LifecycleContext::from_tool_context(&ctx);
        let cancel_token = ctx.cancel_token.clone();
        let risk_flags = entry.tool.risk_flags(&input);
        let started = lifecycle.envelope(
            EventKind::ToolStarted,
            ToolStarted {
                tool_request_id: request_id,
                tool_name: tool_name.clone(),
                effects: descriptor.effects.clone(),
                risk_flags,
            },
            &tool_name,
        )?;
        let source_event_id = started.event_id;
        lifecycle.emit(started, &tool_name).await?;
        // Artifacts created by the tool should point back to this exact
        // `tool.started` envelope, not the caller's placeholder context id.
        ctx.source_event_id = source_event_id;

        let execution = entry.tool.execute(input, ctx).await;

        match execution {
            Ok(_result) if cancel_token.is_cancelled() => {
                // Cancellation wins intentionally: if a tool races and returns
                // a successful payload after cancellation was requested, that
                // payload is discarded and the lifecycle records cancellation.
                let err = ToolError::Cancelled { tool: tool_name.clone() };
                emit_cancelled(&lifecycle, request_id, &tool_name, err.summary())
                    .await
                    .map_err(|emit| emit_after_original_error(&tool_name, &err, &emit))?;
                Err(err)
            }
            Ok(result) if result.summary.chars().count() > SUMMARY_MAX_CHARS => {
                let err = ToolError::ResultTooLarge {
                    tool: tool_name.clone(),
                    message: format!(
                        "summary exceeds {SUMMARY_MAX_CHARS} characters (got {})",
                        result.summary.chars().count()
                    ),
                };
                emit_failed(&lifecycle, request_id, &tool_name, &err)
                    .await
                    .map_err(|emit| emit_after_original_error(&tool_name, &err, &emit))?;
                Err(err)
            }
            Ok(result) => {
                emit_completed(&lifecycle, request_id, &tool_name, &result).await?;
                Ok(result)
            }
            Err(err) if matches!(err, ToolError::Cancelled { .. }) || cancel_token.is_cancelled() => {
                emit_cancelled(&lifecycle, request_id, &tool_name, err.summary())
                    .await
                    .map_err(|emit| emit_after_original_error(&tool_name, &err, &emit))?;
                Err(err)
            }
            Err(err) => {
                emit_failed(&lifecycle, request_id, &tool_name, &err)
                    .await
                    .map_err(|emit| emit_after_original_error(&tool_name, &err, &emit))?;
                Err(err)
            }
        }
    }

    /// Iterate every registered descriptor. Used by the Phase 3 render layer
    /// to build the per-agent tool list shown to the model.
    ///
    /// Iteration preserves registration order so provider prompt/tool-schema
    /// rendering remains stable across runs.
    pub fn descriptors(&self) -> impl Iterator<Item = &ToolDescriptor> {
        self.registry.descriptors()
    }

    /// Lookup a descriptor by name without consulting the schema/tool.
    pub fn descriptor(&self, name: &str) -> Option<&ToolDescriptor> {
        self.registry.get(name).map(|entry| entry.tool.descriptor())
    }
}

fn capability_wire(capability: ToolCapability) -> String {
    serde_json::to_value(capability)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| format!("{capability:?}"))
}

#[derive(Clone)]
struct LifecycleContext {
    run_id: RunId,
    agent_id: Option<AgentId>,
    turn_id: Option<TurnId>,
    event_sink: EventSinkHandle,
}

impl LifecycleContext {
    fn from_tool_context(ctx: &ToolContext<'_>) -> Self {
        Self { run_id: ctx.run_id, agent_id: ctx.agent_id, turn_id: ctx.turn_id, event_sink: ctx.event_sink.clone() }
    }

    fn envelope(&self, kind: EventKind, payload: impl Serialize, tool_name: &str) -> Result<EventEnvelope, ToolError> {
        let payload = serde_json::to_value(payload).map_err(|source| ToolError::Internal {
            tool: tool_name.to_owned(),
            message: format!("failed to encode lifecycle event payload: {source}"),
        })?;

        let mut envelope = EventEnvelope::new(self.run_id, kind, payload);
        envelope.agent_id = self.agent_id;
        envelope.turn_id = self.turn_id;
        Ok(envelope)
    }

    async fn emit(&self, envelope: EventEnvelope, tool_name: &str) -> Result<(), ToolError> {
        self.event_sink.emit(envelope).await.map_err(|err| emit_error(tool_name, &err))
    }
}

async fn emit_completed(
    lifecycle: &LifecycleContext,
    request_id: ToolRequestId,
    tool_name: &str,
    result: &ToolResult,
) -> Result<(), ToolError> {
    let envelope = lifecycle.envelope(
        EventKind::ToolCompleted,
        ToolCompleted {
            tool_request_id: request_id,
            tool_name: tool_name.to_owned(),
            summary: result.summary.clone(),
            content_ref: result.content_ref,
        },
        tool_name,
    )?;
    lifecycle.emit(envelope, tool_name).await
}

async fn emit_failed(
    lifecycle: &LifecycleContext,
    request_id: ToolRequestId,
    tool_name: &str,
    err: &ToolError,
) -> Result<(), ToolError> {
    let envelope = lifecycle.envelope(
        EventKind::ToolFailed,
        ToolFailed {
            tool_request_id: request_id,
            tool_name: tool_name.to_owned(),
            error_kind: err.kind(),
            summary: err.summary(),
        },
        tool_name,
    )?;
    lifecycle.emit(envelope, tool_name).await
}

async fn emit_cancelled(
    lifecycle: &LifecycleContext,
    request_id: ToolRequestId,
    tool_name: &str,
    summary: String,
) -> Result<(), ToolError> {
    let envelope = lifecycle.envelope(
        EventKind::ToolCancelled,
        ToolCancelled { tool_request_id: request_id, tool_name: tool_name.to_owned(), summary },
        tool_name,
    )?;
    lifecycle.emit(envelope, tool_name).await
}

fn emit_error(tool_name: &str, err: &EventLogError) -> ToolError {
    ToolError::Internal { tool: tool_name.to_owned(), message: format!("failed to emit lifecycle event: {err}") }
}

fn emit_after_original_error(tool_name: &str, original: &ToolError, emit: &ToolError) -> ToolError {
    // Once `tool.started` is in the log, terminal event emission is part of the
    // contract. If that emission fails, return the emit failure but keep the
    // original tool failure in the diagnostic string for debuggability.
    ToolError::Internal {
        tool: tool_name.to_owned(),
        message: format!("{}; original {:?}: {}", emit.summary(), original.kind(), original.summary()),
    }
}

impl Default for ToolRuntime {
    fn default() -> Self {
        Self::new()
    }
}
