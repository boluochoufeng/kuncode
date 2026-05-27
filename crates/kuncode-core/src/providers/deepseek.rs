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
    #[error("Http client error: {0}")]
    ClientError(#[from] Box<dyn std::error::Error + Send + Sync + 'static>),

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
    use crate::completion::{CompletionRequestBuilder, Message};

    #[tokio::test]
    #[ignore = "hits the real DeepSeek API; requires DEEPSEEK_API_KEY"]
    async fn completion_smoke() {
        // 从仓库里的 .env 加载 DEEPSEEK_API_KEY；缺文件时静默忽略（也允许直接用
        // 进程已有的环境变量）。dotenv 会从 cwd 向上逐层找 .env，放工作区根目录即可。
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
}
