//! Domain models and provider abstractions for kuncode.
//!
//! [`completion`] defines provider-agnostic conversation, request, and
//! response types plus the [`completion::CompletionModel`] trait. [`providers`]
//! contains concrete integrations that map those domain types to each
//! provider's HTTP JSON. [`non_empty_vec`] and [`json_utils`] provide shared
//! support types used by those mappings.

pub mod completion;
pub mod json_utils;
pub mod non_empty_vec;
pub mod providers;
