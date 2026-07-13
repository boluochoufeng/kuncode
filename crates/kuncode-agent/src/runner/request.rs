//! Provider request assembly and assistant-text projection.

use kuncode_core::{
    completion::{
        AssistantContent, CompletionModel, CompletionRequest, CompletionRequestBuilder, Message,
    },
    non_empty_vec::NonEmptyVec,
};

use crate::{error::AgentError, session::AgentSession, system_prompt::PromptContext};

use super::AgentRunner;

impl<M> AgentRunner<M>
where
    M: CompletionModel,
{
    pub(super) fn build_request(
        &self,
        session: &AgentSession,
    ) -> Result<CompletionRequest, AgentError> {
        if session.is_empty() {
            return Err(AgentError::EmptyTranscript);
        }

        // Assembled fresh each request. Request-only: never stored in the
        // transcript. Kept stable within a session (no volatile blocks) so it
        // stays a cacheable prefix.
        let tools = self.registry.definition();
        let system = self
            .system_prompt
            .assemble(&PromptContext { tools: &tools });

        let mut chat_history =
            Vec::with_capacity(session.messages().len() + usize::from(system.is_some()));
        if let Some(system) = system {
            chat_history.push(Message::system(system));
        }
        chat_history.extend(session.messages().iter().cloned());

        Ok(CompletionRequestBuilder::from_messages(
            NonEmptyVec::try_from(chat_history).map_err(|_| AgentError::EmptyTranscript)?,
        )
        .tools(tools)
        .max_tokens(self.config.max_tokens)
        .reasoning(self.config.reasoning)
        .tool_choice(self.config.tool_choice.clone())
        .build())
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
