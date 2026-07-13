//! Provider request assembly and assistant-text projection.

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
    system_prompt::PromptContext,
};

use super::AgentRunner;

impl<M> AgentRunner<M>
where
    M: CompletionModel,
{
    pub(super) fn freeze_request_projector(&self, messages: &[Message]) -> FrozenRequestProjector {
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
        if protects_compacted_context {
            match system.as_mut() {
                Some(system) => {
                    system.push_str("\n\n");
                    system.push_str(COMPACTED_CONTEXT_SYSTEM_INSTRUCTION);
                }
                None => system = Some(COMPACTED_CONTEXT_SYSTEM_INSTRUCTION.to_string()),
            }
        }
        FrozenRequestProjector {
            tools,
            system,
            max_tokens: self.config.max_tokens,
            reasoning: self.config.reasoning,
            tool_choice: self.config.tool_choice.clone(),
        }
    }
}

/// Owned request shape reused for baseline, candidates, and final dispatch.
pub(super) struct FrozenRequestProjector {
    tools: Vec<ToolDefinition>,
    system: Option<String>,
    max_tokens: Option<u64>,
    reasoning: Option<ReasoningEffort>,
    tool_choice: Option<ToolChoice>,
}

impl FrozenRequestProjector {
    pub(super) fn project_agent(
        &self,
        messages: &[Message],
    ) -> Result<CompletionRequest, AgentError> {
        if messages.is_empty() {
            return Err(AgentError::EmptyTranscript);
        }

        let mut chat_history =
            Vec::with_capacity(messages.len() + usize::from(self.system.is_some()));
        if let Some(system) = &self.system {
            chat_history.push(Message::system(system.clone()));
        }
        chat_history.extend(messages.iter().cloned());

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
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}
