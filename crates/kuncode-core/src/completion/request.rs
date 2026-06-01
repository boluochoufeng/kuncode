//! Request, response, and builder types for invoking a [`CompletionModel`].

use std::ops::{Add, AddAssign};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    completion::{
        CompletionError,
        message::{AssistantContent, Message, ToolChoice},
        streaming::CompletionStream,
    },
    json_utils,
    non_empty_vec::NonEmptyVec,
};

/// A fully-specified completion request ready to send to a provider.
///
/// Construct via [`CompletionRequestBuilder`] rather than directly so that
/// the non-empty `chat_history` invariant is upheld.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CompletionRequest {
    /// Optional model override; if `None`, the [`CompletionModel`] uses the
    /// identifier it was constructed with.
    pub model: Option<String>,
    /// Full conversation context, guaranteed to contain at least one message.
    pub chat_history: NonEmptyVec<Message>,
    /// Tools the model may call. An empty list disables tool calling.
    pub tools: Vec<ToolDescriptor>,
    /// Sampling temperature, when the provider supports it.
    pub temperature: Option<f64>,
    /// Nucleus sampling cutoff; an alternative to `temperature`.
    pub top_p: Option<f64>,
    /// Maximum number of output tokens to generate.
    pub max_tokens: Option<u64>,
    /// Sequences that halt generation when produced. `None` or an empty list
    /// disables custom stop sequences.
    pub stop: Option<Vec<String>>,
    /// How hard the model should reason before answering. `None` follows the
    /// model's default (which may be adaptive); see [`ReasoningEffort`].
    pub reasoning: Option<ReasoningEffort>,
    /// Tool-call policy for this request; `None` lets the provider default.
    pub tool_choice: Option<ToolChoice>,
    /// Provider-specific parameters merged into the outgoing payload.
    pub additional_params: Option<serde_json::Value>,
    /// JSON Schema describing the desired structured output, if any.
    pub output_schema: Option<serde_json::Value>,
}

/// Cross-provider reasoning/thinking effort, normalized to a qualitative
/// level.
///
/// The vocabulary mirrors OpenAI's effort scale, which is the most granular
/// and which DeepSeek explicitly accepts (mapping unsupported levels onto its
/// own). Providers approximate when their native scale is coarser; e.g.
/// DeepSeek collapses `Minimal`/`Low`/`Medium`/`High` to `high` and `Xhigh`
/// to `max`. Budget-based providers (Anthropic, Gemini) translate the level to
/// an approximate token budget. Callers needing exact native control should
/// use [`CompletionRequest::additional_params`] instead.
///
/// A `None` `reasoning` on the request means "use the model default"; `Off` is
/// an explicit request to disable thinking on models that reason by default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    /// Disable thinking entirely.
    Off,
    /// Emit very few or no reasoning tokens; fastest time-to-first-token.
    Minimal,
    /// Spend a small amount of effort on reasoning.
    Low,
    /// Balanced reasoning effort.
    Medium,
    /// Strong reasoning effort.
    High,
    /// Maximum reasoning depth.
    Xhigh,
}

/// The decoded result of a completion call.
///
/// `T` is the provider's raw response type, kept available for callers that
/// need fields beyond the normalized projection.
#[derive(Debug)]
pub struct CompletionResponse<T> {
    /// Content blocks produced by the model, guaranteed non-empty.
    pub choice: NonEmptyVec<AssistantContent>,
    /// Token accounting returned by the provider, normalized across models.
    pub usage: Usage,
    /// Untouched provider response for escape-hatch access.
    pub raw_response: T,
    /// Provider message id, when the API exposes one distinct from the
    /// completion-call id.
    pub message_id: Option<String>,
}

/// Function-style tool the model can call.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolDescriptor {
    /// Function name exposed to the model.
    pub name: String,
    /// Human-readable guidance the model uses to decide when to call the tool.
    pub description: String,
    /// JSON Schema describing the tool's argument object.
    pub parameters: serde_json::Value,
}

/// Provider-defined builtin tool (e.g. web search, code interpreter) that is
/// configured rather than implemented locally.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ProviderToolDescriptor {
    /// Provider-specific tool kind, serialized as the `type` field.
    #[serde(rename = "type")]
    pub kind: String,
    /// Tool-specific configuration; flattened into the parent object and
    /// omitted entirely when empty.
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub config: serde_json::Map<String, serde_json::Value>,
}

/// Token accounting reported alongside a completion response.
///
/// Implements [`Add`] / [`AddAssign`] so usage from multiple calls (e.g.
/// streaming chunks or agentic loops) can be aggregated ergonomically.
#[derive(Default, Debug, PartialEq, Eq, Clone, Copy, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens consumed by prompt/context input.
    pub input_tokens: u64,
    /// Tokens generated for the assistant response.
    pub output_tokens: u64,
    /// Provider-reported total tokens for the request.
    pub total_tokens: u64,
    /// Input tokens served from provider context cache.
    pub cached_input_tokens: u64,
    /// Input tokens written into provider context cache.
    pub cache_creation_input_tokens: u64,
    /// Output tokens spent on reasoning/thinking.
    pub reasoning_tokens: u64,
}

impl Add for Usage {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self {
            input_tokens: self.input_tokens + rhs.input_tokens,
            output_tokens: self.output_tokens + rhs.output_tokens,
            total_tokens: self.total_tokens + rhs.total_tokens,
            cached_input_tokens: self.cached_input_tokens + rhs.cached_input_tokens,
            cache_creation_input_tokens: self.cache_creation_input_tokens
                + rhs.cache_creation_input_tokens,
            reasoning_tokens: self.reasoning_tokens + rhs.reasoning_tokens,
        }
    }
}

impl AddAssign for Usage {
    fn add_assign(&mut self, rhs: Self) {
        self.input_tokens += rhs.input_tokens;
        self.output_tokens += rhs.output_tokens;
        self.total_tokens += rhs.total_tokens;
        self.cached_input_tokens += rhs.cached_input_tokens;
        self.cache_creation_input_tokens += rhs.cache_creation_input_tokens;
        self.reasoning_tokens += rhs.reasoning_tokens;
    }
}

/// Abstraction implemented by each LLM provider integration.
///
/// `Response` is the provider's native response payload; it must be
/// serializable so callers can persist or replay it. `Client` is the
/// provider-specific HTTP/SDK client used to construct model instances.
pub trait CompletionModel: Clone + Send + Sync {
    /// Provider-native response payload retained in [`CompletionResponse`].
    type Response: Send + Sync + Serialize + DeserializeOwned;
    /// Provider-specific client type used to construct model handles.
    type Client;

    /// Constructs a model handle bound to `client` and the given model
    /// identifier.
    fn make(client: &Self::Client, model: impl Into<String>) -> Self;

    /// Performs a single completion call, returning the whole answer at once.
    fn completion(
        &self,
        request: CompletionRequest,
    ) -> impl std::future::Future<
        Output = Result<CompletionResponse<Self::Response>, CompletionError>,
    > + Send;

    /// Performs a streaming completion call.
    ///
    /// The returned future resolves once the connection is established (and may
    /// fail there); the [`CompletionStream`] then yields events as tokens
    /// arrive, ending with [`StreamEvent::Completed`](crate::completion::StreamEvent::Completed).
    fn stream(
        &self,
        request: CompletionRequest,
    ) -> impl std::future::Future<Output = Result<CompletionStream, CompletionError>> + Send;
}

/// Fluent builder for [`CompletionRequest`].
///
/// The terminal user `prompt` is supplied up front and appended last by
/// [`build`](Self::build), which guarantees `chat_history` is never empty
/// regardless of how the caller orders the other builder methods.
pub struct CompletionRequestBuilder {
    prompt: Message,
    request_model: Option<String>,
    chat_history: Vec<Message>,
    tools: Vec<ToolDescriptor>,
    temperature: Option<f64>,
    top_p: Option<f64>,
    max_tokens: Option<u64>,
    stop: Option<Vec<String>>,
    reasoning: Option<ReasoningEffort>,
    tool_choice: Option<ToolChoice>,
    additional_params: Option<serde_json::Value>,
    output_schema: Option<serde_json::Value>,
}

impl CompletionRequestBuilder {
    /// Starts a builder whose final message will be `prompt`.
    pub fn new(prompt: impl Into<Message>) -> Self {
        CompletionRequestBuilder {
            prompt: prompt.into(),
            request_model: None,
            chat_history: Vec::new(),
            tools: Vec::new(),
            temperature: None,
            top_p: None,
            max_tokens: None,
            stop: None,
            reasoning: None,
            tool_choice: None,
            additional_params: None,
            output_schema: None,
        }
    }

    /// Overrides the model identifier for this request only.
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.request_model = Some(model.into());
        self
    }

    /// Appends a single message to the chat history (before the prompt).
    pub fn message(mut self, message: Message) -> Self {
        self.chat_history.push(message);
        self
    }

    /// Appends multiple messages to the chat history.
    pub fn messages(mut self, messages: impl IntoIterator<Item = Message>) -> Self {
        self.chat_history.extend(messages);
        self
    }

    /// Registers a single tool the model may call.
    pub fn tool(mut self, tool: ToolDescriptor) -> Self {
        self.tools.push(tool);
        self
    }

    /// Registers multiple tools the model may call.
    pub fn tools(mut self, tools: impl IntoIterator<Item = ToolDescriptor>) -> Self {
        self.tools.extend(tools);
        self
    }

    /// Sets sampling temperature for this request.
    pub fn temperature(mut self, temperature: Option<f64>) -> Self {
        self.temperature = temperature;
        self
    }

    /// Sets nucleus sampling cutoff for this request.
    pub fn top_p(mut self, top_p: Option<f64>) -> Self {
        self.top_p = top_p;
        self
    }

    /// Sets the maximum number of output tokens for this request.
    pub fn max_tokens(mut self, max_tokens: Option<u64>) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Sets stop sequences; an empty vector is treated as unset by providers.
    pub fn stop(mut self, stop: Option<Vec<String>>) -> Self {
        self.stop = stop;
        self
    }

    /// Sets cross-provider reasoning effort for this request.
    pub fn reasoning(mut self, reasoning: Option<ReasoningEffort>) -> Self {
        self.reasoning = reasoning;
        self
    }

    /// Sets tool-call policy for this request.
    pub fn tool_choice(mut self, tool_choice: Option<ToolChoice>) -> Self {
        self.tool_choice = tool_choice;
        self
    }

    /// Shallow-merges `additional_params` into any value already set.
    ///
    /// On a key collision the new value wins; non-object values are kept as
    /// the left-hand side (see [`crate::json_utils::merge`]).
    pub fn additional_params_merge(mut self, additional_params: serde_json::Value) -> Self {
        match self.additional_params {
            Some(params) => {
                self.additional_params = Some(json_utils::merge(params, additional_params))
            }
            None => self.additional_params = Some(additional_params),
        }
        self
    }

    /// Replaces any previously-set additional parameters wholesale.
    pub fn additional_params(mut self, additional_params: Option<serde_json::Value>) -> Self {
        self.additional_params = additional_params;
        self
    }

    /// Sets the structured-output JSON Schema for this request.
    pub fn output_schema(mut self, schema: Option<serde_json::Value>) -> Self {
        self.output_schema = schema;
        self
    }

    /// Finalizes the builder into a [`CompletionRequest`].
    ///
    /// The prompt is appended after the accumulated history so it is always
    /// the last message; if no history was supplied, the resulting request
    /// contains the prompt alone.
    pub fn build(self) -> CompletionRequest {
        let chat_history = if let Ok(mut chat_history) =
            TryInto::<NonEmptyVec<Message>>::try_into(self.chat_history)
        {
            chat_history.push(self.prompt);
            chat_history
        } else {
            NonEmptyVec::new(self.prompt)
        };

        CompletionRequest {
            model: self.request_model,
            chat_history,
            tools: self.tools,
            temperature: self.temperature,
            top_p: self.top_p,
            max_tokens: self.max_tokens,
            stop: self.stop,
            reasoning: self.reasoning,
            tool_choice: self.tool_choice,
            additional_params: self.additional_params,
            output_schema: self.output_schema,
        }
    }
}
