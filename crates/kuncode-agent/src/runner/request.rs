//! Provider request assembly and assistant-text projection.
//!
//! Compaction freezes all provider-visible request components except active
//! messages, preventing token decisions from comparing different envelopes.

mod runtime_state;

use kuncode_core::{
    completion::{
        AssistantContent, CompletionModel, CompletionRequest, CompletionRequestBuilder, Message,
        ReasoningEffort, ToolChoice, ToolDefinition,
    },
    non_empty_vec::NonEmptyVec,
};

use crate::{
    compaction::{
        CompactionRequestProjector, RequestProjectionError,
        budget::CompactionMode,
        summary::{COMPACTED_CONTEXT_SYSTEM_INSTRUCTION, is_compacted_context_message},
    },
    error::AgentError,
    session::AgentSession,
    system_prompt::PromptContext,
};

use runtime_state::{HARNESS_RUNTIME_STATE_SYSTEM_INSTRUCTION, project as project_runtime_state};

use super::AgentRunner;

impl<M> AgentRunner<M>
where
    M: CompletionModel,
{
    /// Captures one owned request envelope for compaction accounting and dispatch.
    ///
    /// Harness runtime state is projected as a final user-role envelope but is
    /// never human-authored lineage and never enters the durable journal.
    ///
    /// # Errors
    /// Returns [`AgentError::RequestEncoding`] when request-only runtime state
    /// cannot be serialized.
    pub(super) fn freeze_request_projector(
        &self,
        session: &AgentSession,
    ) -> Result<FrozenRequestProjector, AgentError> {
        let messages = session.messages();
        let tools = self.registry.definition();
        let mut system = self
            .system_prompt
            .assemble(&PromptContext { tools: &tools });
        let protects_compacted_context = self
            .config
            .compaction
            .as_ref()
            .is_some_and(|runtime| runtime.policy.mode() == CompactionMode::Enabled)
            || messages.iter().any(is_compacted_context_message);
        let todos = session.todos_snapshot();
        let projects_runtime_state = protects_compacted_context || !todos.is_empty();
        if projects_runtime_state {
            match system.as_mut() {
                Some(system) => {
                    if protects_compacted_context {
                        system.push_str("\n\n");
                        system.push_str(COMPACTED_CONTEXT_SYSTEM_INSTRUCTION);
                    }
                    system.push_str("\n\n");
                    system.push_str(HARNESS_RUNTIME_STATE_SYSTEM_INSTRUCTION);
                }
                None => {
                    let mut guards = Vec::with_capacity(2);
                    if protects_compacted_context {
                        guards.push(COMPACTED_CONTEXT_SYSTEM_INSTRUCTION);
                    }
                    guards.push(HARNESS_RUNTIME_STATE_SYSTEM_INSTRUCTION);
                    system = Some(guards.join("\n\n"));
                }
            }
        }
        let runtime_state = projects_runtime_state
            .then(|| project_runtime_state(&todos))
            .transpose()
            .map_err(|error| AgentError::RequestEncoding(error.to_string()))?;
        Ok(FrozenRequestProjector {
            tools,
            system,
            max_tokens: self.config.max_tokens,
            reasoning: self.config.reasoning,
            tool_choice: self.config.tool_choice.clone(),
            runtime_state,
        })
    }
}

/// Owned request shape reused for baseline, candidates, and final dispatch.
///
/// Reuse ensures that changes in tools, system text, generation options, or
/// runtime state cannot masquerade as compaction savings.
pub(super) struct FrozenRequestProjector {
    tools: Vec<ToolDefinition>,
    system: Option<String>,
    max_tokens: Option<u64>,
    reasoning: Option<ReasoningEffort>,
    tool_choice: Option<ToolChoice>,
    runtime_state: Option<Message>,
}

impl FrozenRequestProjector {
    /// Projects active messages into the frozen provider-visible envelope.
    ///
    /// # Errors
    /// Returns [`AgentError::EmptyTranscript`] when no active message is supplied.
    pub(super) fn project_agent(
        &self,
        messages: &[Message],
    ) -> Result<CompletionRequest, AgentError> {
        if messages.is_empty() {
            return Err(AgentError::EmptyTranscript);
        }

        let mut chat_history = Vec::with_capacity(
            messages.len()
                + usize::from(self.system.is_some())
                + usize::from(self.runtime_state.is_some()),
        );
        if let Some(system) = &self.system {
            chat_history.push(Message::system(system.clone()));
        }
        chat_history.extend(messages.iter().cloned());
        if let Some(runtime_state) = &self.runtime_state {
            chat_history.push(runtime_state.clone());
        }

        Ok(CompletionRequestBuilder::from_messages(
            NonEmptyVec::try_from(chat_history).map_err(|_| AgentError::EmptyTranscript)?,
        )
        .tools(self.tools.clone())
        .max_tokens(self.max_tokens)
        .reasoning(self.reasoning)
        .tool_choice(self.tool_choice.clone())
        .build())
    }
}

impl CompactionRequestProjector for FrozenRequestProjector {
    fn project(&self, messages: &[Message]) -> Result<CompletionRequest, RequestProjectionError> {
        self.project_agent(messages)
            .map_err(|error| RequestProjectionError::new(error.to_string()))
    }
}

fn assistant_content_at(
    messages: &[Message],
    index: usize,
) -> Option<&NonEmptyVec<AssistantContent>> {
    match messages.get(index) {
        Some(Message::Assistant { content, .. }) => Some(content),
        _ => None,
    }
}

pub(super) fn final_text_at(messages: &[Message], index: usize) -> String {
    assistant_content_at(messages, index)
        .map(assistant_text)
        .unwrap_or_default()
}

pub(super) fn assistant_text(content: &NonEmptyVec<AssistantContent>) -> String {
    content
        .iter()
        .filter_map(|content| match content {
            AssistantContent::Text(text) => Some(text.text_ref()),
            AssistantContent::Refusal(refusal) => Some(refusal.text_ref()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}
