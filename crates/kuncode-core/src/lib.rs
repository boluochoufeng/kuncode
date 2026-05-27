//! kuncode 的领域模型与 provider 抽象层。
//!
//! [`completion`] 定义 provider 无关的对话 / 请求 / 响应类型与
//! [`completion::CompletionModel`] trait；[`providers`] 下是具体 provider 实现
//! （当前 DeepSeek），负责把这些类型映射到 provider 的 HTTP JSON。
//! [`non_empty_vec`] 与 [`json_utils`] 是支撑工具。

pub mod completion;
pub mod json_utils;
pub mod non_empty_vec;
pub mod providers;
