//! Concrete LLM provider integrations.
//!
//! Each provider owns the mapping from the provider-agnostic
//! [`crate::completion`] types to that provider's HTTP API. DeepSeek remains
//! the default, while [`openai`] implements the official OpenAI protocol.

pub mod any_chat;
pub(crate) mod chat_completions;
pub mod deepseek;
pub mod openai;
