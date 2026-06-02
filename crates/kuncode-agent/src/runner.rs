//! Agent loop entry point.

use kuncode_core::{
    completion::{
        AssistantContent, CompletionModel, CompletionRequest, CompletionRequestBuilder, Message,
        ReasoningEffort, ToolCall, ToolChoice, ToolResult, ToolResultContent, Usage, UserContent,
    },
    non_empty_vec::NonEmptyVec,
};

use crate::{error::AgentError, registry::ToolRegistry};

const DEFAULT_MAX_ITERATIONS: usize = 10;

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
    /// System prompt injected as the first message of every request. `None`
    /// sends no system prompt; it is request-only and never stored in the
    /// transcript ([`AgentRun::messages`]).
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

/// Result of a completed agent run.
#[derive(Clone, Debug)]
pub struct AgentRun {
    /// Full transcript including tool-call and tool-result turns.
    pub messages: Vec<Message>,
    /// Final assistant content produced after no further tool calls were
    /// requested.
    pub final_content: NonEmptyVec<AssistantContent>,
    /// Aggregated provider usage across all model calls in the loop.
    pub usage: Usage,
    /// Number of model calls performed.
    pub iterations: usize,
}

impl AgentRun {
    /// Concatenates visible text blocks from the final assistant message.
    pub fn final_text(&self) -> String {
        self.final_content
            .iter()
            .filter_map(|content| match content {
                AssistantContent::Text(text) => Some(text.text_ref()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
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

    /// Runs the loop from a single user prompt.
    pub async fn run(&self, prompt: impl Into<String>) -> Result<AgentRun, AgentError> {
        self.run_messages(vec![Message::user(prompt)]).await
    }

    /// Runs the loop from an existing transcript.
    pub async fn run_messages(&self, mut messages: Vec<Message>) -> Result<AgentRun, AgentError> {
        if messages.is_empty() {
            return Err(AgentError::EmptyTranscript);
        }

        let mut usage = Usage::default();

        for iteration in 0..self.config.max_iterations {
            let request = self.build_request(&messages)?;
            let response = self.model.completion(request).await?;

            usage += response.usage;

            let assistant_content = response.choice.clone();
            messages.push(Message::Assistant {
                id: response.message_id,
                content: assistant_content.clone(),
            });

            let tool_calls = tool_calls(&assistant_content);
            if tool_calls.is_empty() {
                return Ok(AgentRun {
                    messages,
                    final_content: assistant_content,
                    usage,
                    iterations: iteration + 1,
                });
            }

            for tool_call in tool_calls {
                let output = self
                    .registry
                    .call(
                        &tool_call.function.name,
                        tool_call.function.arguments.clone(),
                    )
                    .await
                    .map_err(|source| AgentError::Tool {
                        name: tool_call.function.name.clone(),
                        source,
                    })?;

                messages.push(tool_result_message(&tool_call, output.to_model_content()));
            }
        }

        Err(AgentError::MaxIterations {
            max_iterations: self.config.max_iterations,
            messages,
            usage,
        })
    }

    fn build_request(&self, messages: &[Message]) -> Result<CompletionRequest, AgentError> {
        let Some((prompt, history)) = messages.split_last() else {
            return Err(AgentError::EmptyTranscript);
        };

        let mut builder = CompletionRequestBuilder::new(prompt.clone());
        if let Some(system) = &self.config.system_prompt {
            builder = builder.message(Message::system(system.clone()));
        }

        Ok(builder
            .messages(history.iter().cloned())
            .tools(self.registry.definition())
            .max_tokens(self.config.max_tokens)
            .reasoning(self.config.reasoning)
            .tool_choice(self.config.tool_choice.clone())
            .build())
    }
}

fn tool_calls(content: &NonEmptyVec<AssistantContent>) -> Vec<ToolCall> {
    content
        .iter()
        .filter_map(|content| match content {
            AssistantContent::ToolCall(tool_call) => Some(tool_call.clone()),
            _ => None,
        })
        .collect()
}

fn tool_result_message(tool_call: &ToolCall, content: String) -> Message {
    Message::User {
        content: NonEmptyVec::new(UserContent::ToolResult(ToolResult {
            id: tool_call.id.clone(),
            call_id: tool_call.call_id.clone(),
            content: NonEmptyVec::new(ToolResultContent::text(content)),
        })),
    }
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
    use crate::{error::AgentError, registry::ToolRegistry, tool::bash::Bash};

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

        let run = runner
            .run("inspect the workspace")
            .await
            .expect("agent run should complete");

        assert_eq!(run.final_text(), "done");
        assert_eq!(run.iterations, 2);
        assert_eq!(run.usage.total_tokens, 6);
        assert_eq!(run.messages.len(), 4);

        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].tools[0].name, "bash");
        assert_eq!(requests[1].tools[0].name, "bash");
        assert_eq!(requests[1].chat_history.len(), 3);

        match &run.messages[2] {
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

        let err = runner
            .run("keep using tools")
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

        let run = runner.run("hello").await.expect("run completes");

        // The system prompt is request-only, never part of the transcript.
        assert!(!matches!(
            run.messages.first(),
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

        let err = runner
            .run_messages(Vec::new())
            .await
            .expect_err("empty transcript is invalid");

        assert!(matches!(err, AgentError::EmptyTranscript));
    }
}
