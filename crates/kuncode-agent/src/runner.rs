//! Agent loop entry point.

use kuncode_core::{
    completion::{
        AssistantContent, CompletionModel, CompletionRequest, CompletionRequestBuilder, Message,
        ReasoningEffort, ToolChoice, ToolResult, ToolResultContent, Usage, UserContent,
    },
    non_empty_vec::NonEmptyVec,
};

use crate::{error::AgentError, registry::ToolRegistry, session::AgentSession};

const DEFAULT_MAX_ITERATIONS: usize = 50;

/// Runtime knobs for one agent loop.
#[derive(Clone, Debug)]
pub struct AgentConfig {
    /// Maximum number of model calls before the loop aborts.
    pub max_iterations: usize,
    /// Output token cap passed to each completion request.
    pub max_tokens: Option<u64>,
    /// Reasoning effort passed through to the provider.
    pub reasoning: Option<ReasoningEffort>,
    /// Tool-call policy passed through to the provider.
    pub tool_choice: Option<ToolChoice>,
    /// System prompt injected as the first message of every request.
    ///
    /// It is request-only and never stored in [`AgentSession`].
    pub system_prompt: Option<String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: DEFAULT_MAX_ITERATIONS,
            max_tokens: Some(4096),
            reasoning: None,
            tool_choice: None,
            system_prompt: None,
        }
    }
}

/// Summary for one completed user turn appended to an existing transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgentTurn {
    /// Index of the final assistant message inside the caller-owned transcript.
    pub final_message_index: usize,
    /// Provider usage aggregated across this turn's model calls.
    pub usage: Usage,
    /// Number of model calls performed for this turn.
    pub iterations: usize,
}

impl AgentTurn {
    /// Concatenates visible text blocks from the final assistant message.
    pub fn final_text(&self, session: &AgentSession) -> String {
        final_text_at(session.messages(), self.final_message_index)
    }
}

/// Minimal agent loop for model/tool/model interaction.
#[derive(Clone)]
pub struct AgentRunner<M> {
    model: M,
    registry: ToolRegistry,
    config: AgentConfig,
}

impl<M> AgentRunner<M>
where
    M: CompletionModel,
{
    /// Creates a runner with default loop configuration.
    pub fn new(model: M, registry: ToolRegistry) -> Self {
        Self::with_config(model, registry, AgentConfig::default())
    }

    /// Creates a runner with explicit loop configuration.
    pub fn with_config(model: M, registry: ToolRegistry, config: AgentConfig) -> Self {
        Self {
            model,
            registry,
            config,
        }
    }

    /// Appends a user prompt, then advances the transcript until a final answer.
    pub async fn run_turn(
        &self,
        session: &mut AgentSession,
        prompt: impl Into<String>,
    ) -> Result<AgentTurn, AgentError> {
        session.push_user(prompt);
        self.continue_session(session).await
    }

    /// Advances an existing transcript in place until the model stops calling tools.
    pub async fn continue_session(
        &self,
        session: &mut AgentSession,
    ) -> Result<AgentTurn, AgentError> {
        if session.is_empty() {
            return Err(AgentError::EmptyTranscript);
        }

        let mut usage = Usage::default();

        for iteration in 0..self.config.max_iterations {
            let iteration_result = self.run_iteration(session).await?;
            usage += iteration_result.usage;

            if iteration_result.tool_calls.is_empty() {
                return Ok(AgentTurn {
                    final_message_index: iteration_result.assistant_message_index,
                    usage,
                    iterations: iteration + 1,
                });
            }

            self.execute_tool_calls(session, iteration_result.tool_calls)
                .await?;
        }

        Err(AgentError::MaxIterations {
            max_iterations: self.config.max_iterations,
            messages: session.messages().to_vec(),
            usage,
        })
    }

    async fn run_iteration(
        &self,
        session: &mut AgentSession,
    ) -> Result<IterationResult, AgentError> {
        let request = self.build_request(session)?;
        let response = self.model.completion(request).await?;

        let tool_calls = pending_tool_calls(&response.choice);
        let usage = response.usage;
        session.push(Message::Assistant {
            id: response.message_id,
            content: response.choice,
        });

        Ok(IterationResult {
            assistant_message_index: session.messages().len() - 1,
            usage,
            tool_calls,
        })
    }

    async fn execute_tool_calls(
        &self,
        session: &mut AgentSession,
        tool_calls: Vec<PendingToolCall>,
    ) -> Result<(), AgentError> {
        for tool_call in tool_calls {
            let PendingToolCall {
                id,
                call_id,
                name,
                arguments,
            } = tool_call;

            let output = match self.registry.call(&name, arguments).await {
                Ok(output) => output,
                Err(source) => return Err(AgentError::Tool { name, source }),
            };

            session.push(tool_result_message(id, call_id, output.to_model_content()));
        }

        Ok(())
    }

    fn build_request(&self, session: &AgentSession) -> Result<CompletionRequest, AgentError> {
        if session.is_empty() {
            return Err(AgentError::EmptyTranscript);
        }

        let mut chat_history = Vec::with_capacity(
            session.messages().len() + usize::from(self.config.system_prompt.is_some()),
        );
        if let Some(system) = &self.config.system_prompt {
            chat_history.push(Message::system(system.clone()));
        }
        chat_history.extend(session.messages().iter().cloned());

        Ok(CompletionRequestBuilder::from_messages(
            NonEmptyVec::try_from(chat_history).map_err(|_| AgentError::EmptyTranscript)?,
        )
        .tools(self.registry.definition())
        .max_tokens(self.config.max_tokens)
        .reasoning(self.config.reasoning)
        .tool_choice(self.config.tool_choice.clone())
        .build())
    }
}

#[derive(Debug)]
struct IterationResult {
    assistant_message_index: usize,
    usage: Usage,
    tool_calls: Vec<PendingToolCall>,
}

#[derive(Debug)]
struct PendingToolCall {
    id: String,
    call_id: Option<String>,
    name: String,
    arguments: serde_json::Value,
}

fn pending_tool_calls(content: &NonEmptyVec<AssistantContent>) -> Vec<PendingToolCall> {
    content
        .iter()
        .filter_map(|content| match content {
            AssistantContent::ToolCall(tool_call) => Some(PendingToolCall {
                id: tool_call.id.clone(),
                call_id: tool_call.call_id.clone(),
                name: tool_call.function.name.clone(),
                arguments: tool_call.function.arguments.clone(),
            }),
            _ => None,
        })
        .collect()
}

fn tool_result_message(id: String, call_id: Option<String>, content: String) -> Message {
    Message::User {
        content: NonEmptyVec::new(UserContent::ToolResult(ToolResult {
            id,
            call_id,
            content: NonEmptyVec::new(ToolResultContent::text(content)),
        })),
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

fn final_text_at(messages: &[Message], index: usize) -> String {
    assistant_content_at(messages, index)
        .map(assistant_text)
        .unwrap_or_default()
}

fn assistant_text(content: &NonEmptyVec<AssistantContent>) -> String {
    content
        .iter()
        .filter_map(|content| match content {
            AssistantContent::Text(text) => Some(text.text_ref()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use kuncode_core::completion::{
        AssistantContent, CompletionError, CompletionRequest, CompletionResponse, CompletionStream,
        Message, ToolResultContent, Usage, UserContent,
    };
    use kuncode_core::non_empty_vec::NonEmptyVec;
    use serde_json::Value;

    use super::{AgentConfig, AgentRunner};
    use crate::{
        error::AgentError, registry::ToolRegistry, session::AgentSession, tool::bash::Bash,
    };

    async fn bash() -> Bash {
        Bash::from_current_dir()
            .await
            .expect("current directory should be a valid workspace")
    }

    #[derive(Clone, Default)]
    struct FakeModel {
        responses: Arc<Mutex<VecDeque<CompletionResponse<Value>>>>,
        requests: Arc<Mutex<Vec<CompletionRequest>>>,
    }

    impl FakeModel {
        fn new(responses: impl IntoIterator<Item = CompletionResponse<Value>>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into_iter().collect())),
                requests: Arc::default(),
            }
        }

        fn requests(&self) -> Vec<CompletionRequest> {
            self.requests.lock().expect("requests lock").clone()
        }
    }

    impl kuncode_core::completion::CompletionModel for FakeModel {
        type Response = Value;
        type Client = ();

        fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
            Self::default()
        }

        async fn completion(
            &self,
            request: CompletionRequest,
        ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
            self.requests.lock().expect("requests lock").push(request);
            Ok(self
                .responses
                .lock()
                .expect("responses lock")
                .pop_front()
                .expect("fake response queued"))
        }

        async fn stream(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionStream, CompletionError> {
            unimplemented!("fake model does not stream")
        }
    }

    fn response(content: AssistantContent) -> CompletionResponse<Value> {
        CompletionResponse {
            choice: NonEmptyVec::new(content),
            usage: Usage {
                input_tokens: 1,
                output_tokens: 2,
                total_tokens: 3,
                cached_input_tokens: 0,
                cache_creation_input_tokens: 0,
                reasoning_tokens: 0,
            },
            raw_response: serde_json::json!({}),
            message_id: None,
        }
    }

    #[tokio::test]
    async fn runs_tool_call_then_final_answer() {
        let model = FakeModel::new([
            response(AssistantContent::tool_call(
                "call_1",
                "bash",
                serde_json::json!({ "cmd": "printf s01" }),
            )),
            response(AssistantContent::text("done")),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let runner = AgentRunner::new(model.clone(), registry);
        let mut session = AgentSession::new();

        let turn = runner
            .run_turn(&mut session, "inspect the workspace")
            .await
            .expect("agent run should complete");

        assert_eq!(turn.final_text(&session), "done");
        assert_eq!(turn.iterations, 2);
        assert_eq!(turn.usage.total_tokens, 6);
        assert_eq!(session.messages().len(), 4);

        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].tools[0].name, "bash");
        assert_eq!(requests[1].tools[0].name, "bash");
        assert_eq!(requests[1].chat_history.len(), 3);

        match &session.messages()[2] {
            Message::User { content } => {
                let UserContent::ToolResult(result) = content.first() else {
                    panic!("expected tool result content");
                };
                let ToolResultContent::Text(text) = result.content.first();
                assert!(text.text_ref().contains("\"stdout\":\"s01\""));
            }
            other => panic!("expected tool result user message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_turn_updates_transcript_in_place() {
        let model = FakeModel::new([response(AssistantContent::text("done"))]);
        let runner = AgentRunner::new(model, ToolRegistry::new());
        let mut session = AgentSession::new();

        let turn = runner
            .run_turn(&mut session, "finish this")
            .await
            .expect("agent turn should complete");

        assert_eq!(turn.final_text(&session), "done");
        assert_eq!(turn.final_message_index, 1);
        assert_eq!(session.messages().len(), 2);
    }

    #[tokio::test]
    async fn requests_keep_stable_prefix_between_tool_iterations() {
        let model = FakeModel::new([
            response(AssistantContent::tool_call(
                "call_1",
                "bash",
                serde_json::json!({ "cmd": "printf cache" }),
            )),
            response(AssistantContent::text("done")),
        ]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let runner = AgentRunner::with_config(
            model.clone(),
            registry,
            AgentConfig {
                system_prompt: Some("be stable".to_string()),
                ..AgentConfig::default()
            },
        );
        let mut session = AgentSession::new();

        runner
            .run_turn(&mut session, "inspect the workspace")
            .await
            .expect("agent run should complete");

        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].tools, requests[1].tools);
        assert!(
            requests[1]
                .chat_history
                .starts_with(&requests[0].chat_history)
        );
        assert_eq!(requests[0].chat_history.len(), 2);
        assert_eq!(requests[1].chat_history.len(), 4);
    }

    #[tokio::test]
    async fn stops_when_max_iterations_is_exhausted() {
        let model = FakeModel::new([response(AssistantContent::tool_call(
            "call_1",
            "bash",
            serde_json::json!({ "cmd": "printf loop" }),
        ))]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let runner = AgentRunner::with_config(
            model,
            registry,
            AgentConfig {
                max_iterations: 1,
                ..AgentConfig::default()
            },
        );
        let mut session = AgentSession::new();

        let err = runner
            .run_turn(&mut session, "keep using tools")
            .await
            .expect_err("run should stop at the iteration budget");

        let AgentError::MaxIterations {
            max_iterations,
            messages,
            usage,
        } = err
        else {
            panic!("expected MaxIterations, got {err:?}");
        };

        assert_eq!(max_iterations, 1);
        // The partial transcript is preserved: user prompt, assistant tool
        // call, and the tool result appended before the budget was hit.
        assert_eq!(messages.len(), 3);
        assert_eq!(usage.total_tokens, 3);
    }

    #[tokio::test]
    async fn injects_system_prompt_as_first_message() {
        let model = FakeModel::new([response(AssistantContent::text("hi"))]);
        let mut registry = ToolRegistry::new();
        registry.register(bash().await);
        let runner = AgentRunner::with_config(
            model.clone(),
            registry,
            AgentConfig {
                system_prompt: Some("be terse".to_string()),
                ..AgentConfig::default()
            },
        );
        let mut session = AgentSession::new();

        runner
            .run_turn(&mut session, "hello")
            .await
            .expect("run completes");

        // The system prompt is request-only, never part of the transcript.
        assert!(!matches!(
            session.messages().first(),
            Some(Message::System { .. })
        ));

        let request = &model.requests()[0];
        let Message::System { content } = request.chat_history.first() else {
            panic!("system prompt should be the first message sent to the model");
        };
        assert_eq!(content, "be terse");
    }

    #[tokio::test]
    async fn rejects_empty_transcript() {
        let runner = AgentRunner::new(FakeModel::default(), ToolRegistry::new());
        let mut session = AgentSession::new();

        let err = runner
            .continue_session(&mut session)
            .await
            .expect_err("empty transcript is invalid");

        assert!(matches!(err, AgentError::EmptyTranscript));
    }
}
