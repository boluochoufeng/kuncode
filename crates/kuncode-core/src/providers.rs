//! 具体 LLM provider 的实现。
//!
//! 每个 provider 一个子模块，负责把 [`crate::completion`] 的 provider 无关类型
//! 映射到该 provider 的 HTTP API。当前只有 [`deepseek`]。

pub mod deepseek;
