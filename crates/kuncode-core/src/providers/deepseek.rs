//! DeepSeek provider：HTTP client 与 completion 模型。
//!
//! [`DeepSeekClient`] 持有鉴权与 `reqwest` 客户端；[`DeepSeekCompletionModel`]
//! 实现 [`CompletionModel`]。请求/响应与 [`crate::completion`] 领域类型之间的 wire
//! 映射在子模块 `protocol` 里。

use std::env::VarError;
use std::time::Duration;

use thiserror::Error;

use crate::{
    completion::{CompletionError, CompletionModel},
    json_utils,
    providers::deepseek::protocol::{DeepSeekCompletionRequest, DeepSeekCompletionResponse},
};

pub(crate) mod protocol;

const DEEPSEEK_API_BASE_URL: &str = "https://api.deepseek.com";
const DEEPSEEK_V4_PRO: &str = "deepseek-v4-pro";
const DEEPSEEK_V4_FLASH: &str = "deepseek-v4-flash";

#[derive(Debug, Error)]
pub enum Error {
    /// 底层 `reqwest` 客户端构建失败（如 TLS / 超时等配置无效）。
    #[error("Http client error: {0}")]
    ClientError(#[from] Box<dyn std::error::Error + Send + Sync + 'static>),

    /// 必需的环境变量缺失或无效。
    #[error("environment variable `{name}` is not set or is invalid")]
    EnvironmentVariable {
        name: String,
        #[source]
        source: VarError,
    },
}

#[derive(Clone)]
pub struct DeepSeekClient {
    http_client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl DeepSeekClient {
    pub fn new(api_key: impl Into<String>) -> Result<Self, Error> {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(360))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|err| Error::ClientError(err.into()))?;
        Ok(Self {
            http_client,
            api_key: api_key.into(),
            base_url: DEEPSEEK_API_BASE_URL.to_string(),
        })
    }

    pub fn from_env() -> Result<Self, Error> {
        let api_key =
            std::env::var("DEEPSEEK_API_KEY").map_err(|err| Error::EnvironmentVariable {
                name: "DEEPSEEK_API_KEY".to_string(),
                source: err,
            })?;

        Self::new(api_key)
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.http_client
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(&self.api_key)
    }
}

/// 绑定到某个 DeepSeek 模型 id 的 completion 模型句柄，实现 [`CompletionModel`]。
#[derive(Clone)]
pub struct DeepSeekCompletionModel {
    client: DeepSeekClient,
    model: String,
}

impl CompletionModel for DeepSeekCompletionModel {
    type Response = DeepSeekCompletionResponse;
    type Client = DeepSeekClient;

    fn make(client: &Self::Client, model: impl Into<String>) -> Self {
        Self {
            client: client.clone(),
            model: model.into(),
        }
    }

    async fn completion(
        &self,
        request: crate::completion::CompletionRequest,
    ) -> Result<
        crate::completion::CompletionResponse<Self::Response>,
        crate::completion::CompletionError,
    > {
        let mut request = request;
        request.model.get_or_insert_with(|| self.model.clone());

        let extra_params = request.additional_params.take();
        let request = DeepSeekCompletionRequest::try_from(request)?;
        let mut body = serde_json::to_value(&request)?;
        if let Some(extra) = extra_params {
            body = json_utils::merge(body, extra);
        }

        let response = self
            .client
            .post("/chat/completions")
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let message = response.text().await.unwrap_or_default();
            return Err(CompletionError::ApiError {
                status: status.as_u16(),
                message,
            });
        }

        let response_body = response.bytes().await?;
        let resp: DeepSeekCompletionResponse = serde_json::from_slice(&response_body)?;
        resp.try_into()
    }

    async fn stream(
        &self,
        request: crate::completion::CompletionRequest,
    ) -> Result<crate::completion::CompletionStream, crate::completion::CompletionError> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::{AssistantContent, CompletionRequestBuilder, Message, ToolDescriptor};

    #[tokio::test]
    #[ignore = "hits the real DeepSeek API; requires DEEPSEEK_API_KEY"]
    async fn completion_smoke() {
        // 从仓库里的 .env 加载 DEEPSEEK_API_KEY；
        dotenvy::dotenv().ok();

        let client = DeepSeekClient::from_env().expect("DEEPSEEK_API_KEY 未设置");
        let model = DeepSeekCompletionModel::make(&client, DEEPSEEK_V4_FLASH);

        let request = CompletionRequestBuilder::new(Message::user("你好，你的中国象棋水平怎么样"))
            .max_tokens(Some(1024))
            .build();

        let response = model
            .completion(request)
            .await
            .expect("completion 请求失败");

        println!("choice = {:#?}", response.choice);
        println!("usage  = {:?}", response.usage);

        // 真实往返必然有 token 计费，以此确认确实命中了 API 而非空响应。
        assert!(
            response.usage.total_tokens > 0,
            "usage 为空，可能没真正命中 API"
        );
    }

    /// 端到端验证**工具调用链路**：请求带工具 → 模型返回 tool_call → 本地执行 →
    /// 用 `tool_result` 回灌 → 模型据此续答。
    #[tokio::test]
    #[ignore = "hits the real DeepSeek API; requires DEEPSEEK_API_KEY"]
    async fn tool_call_round_trip() {
        dotenvy::dotenv().ok();

        let client = DeepSeekClient::from_env().expect("DEEPSEEK_API_KEY 未设置");
        let model = DeepSeekCompletionModel::make(&client, DEEPSEEK_V4_FLASH);

        // 一个模型无法凭空回答、必须调用的函数。
        let weather_tool = ToolDescriptor {
            name: "get_weather".to_string(),
            description: "查询某个城市的当前天气".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "city": { "type": "string", "description": "城市名，如 北京" }
                },
                "required": ["city"]
            }),
        };

        // —— 第 1 轮：用户提问，期望模型返回 tool_call ——
        let user_prompt = Message::user("北京现在天气怎么样？");
        let round1 = CompletionRequestBuilder::new(user_prompt.clone())
            .tool(weather_tool.clone())
            .max_tokens(Some(1024))
            .build();

        let resp1 = model.completion(round1).await.expect("round1 失败");
        println!("== round1 choice ==\n{:#?}", resp1.choice);

        let tool_call = resp1
            .choice
            .iter()
            .find_map(|c| match c {
                AssistantContent::ToolCall(tc) => Some(tc.clone()),
                _ => None,
            })
            .expect("模型未发起 tool_call（若频繁不触发，可改用 tool_choice=Required 强制）");

        assert_eq!(tool_call.function.name, "get_weather");
        let city = tool_call
            .function
            .arguments
            .get("city")
            .and_then(|v| v.as_str())
            .expect("tool_call 参数缺少 city");
        println!("== 解析出的工具调用 == name=get_weather city={city}");

        // —— 本地“执行”工具：返回固定结果 ——
        let tool_output = serde_json::json!({
            "city": city,
            "temperature": "22°C",
            "condition": "晴"
        })
        .to_string();

        // —— 第 2 轮：assistant(tool_call) + tool_result 回灌，期望模型据此总结 ——
        // build() 会把 prompt 追加到末尾，故把 tool_result 作为 prompt、前两条作为
        // history，最终顺序为 [user, assistant(tool_call), tool_result]。
        let assistant_turn = Message::Assistant {
            id: resp1.message_id.clone(),
            content: resp1.choice.clone(),
        };
        let result_turn = Message::tool_result(tool_call.id.clone(), tool_output);

        let round2 = CompletionRequestBuilder::new(result_turn)
            .messages([user_prompt, assistant_turn])
            .tool(weather_tool)
            .max_tokens(Some(1024))
            .build();

        let resp2 = model.completion(round2).await.expect("round2 失败");
        println!("== round2 choice ==\n{:#?}", resp2.choice);

        let final_text: String = resp2
            .choice
            .iter()
            .filter_map(|c| match c {
                AssistantContent::Text(t) => Some(t.text_ref()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        println!("== 最终回答 ==\n{final_text}");

        // 整条链路打通的标志：第 2 轮基于工具结果给出了非空的文本回答。
        assert!(!final_text.trim().is_empty(), "第 2 轮未产生文本回答");
    }
}
