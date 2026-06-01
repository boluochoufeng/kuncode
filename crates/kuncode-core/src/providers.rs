//! Concrete LLM provider integrations.
//!
//! Each provider owns the mapping from the provider-agnostic
//! [`crate::completion`] types to that provider's HTTP API. Currently this
//! crate ships the [`deepseek`] integration.

pub mod deepseek;
