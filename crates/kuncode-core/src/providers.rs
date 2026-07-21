//! Concrete LLM provider integrations.
//!
//! Each provider owns the mapping from the provider-agnostic
//! [`crate::completion`] types to that provider's HTTP API. DeepSeek remains
//! the default, while [`openai_compatible`] supports OpenAI and compatible
//! `/chat/completions` endpoints.

pub mod any_chat;
pub mod deepseek;
pub mod openai_compatible;
