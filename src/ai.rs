use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use reqwest::{Client, StatusCode, header};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

const OPENROUTER_TITLE: &str = "Discord Learn Programming Bot";

#[derive(Debug, Clone, Serialize)]
pub struct PromptMessage {
    pub role: PromptRole,
    pub content: String,
}

impl PromptMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: PromptRole::System,
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: PromptRole::User,
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: PromptRole::Assistant,
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptRole {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub messages: Vec<PromptMessage>,
    pub max_tokens: u32,
    pub response_schema: Option<JsonResponseSchema>,
}

impl ModelRequest {
    pub fn text(messages: Vec<PromptMessage>, max_tokens: u32) -> Self {
        Self {
            messages,
            max_tokens,
            response_schema: None,
        }
    }

    pub fn json(
        messages: Vec<PromptMessage>,
        max_tokens: u32,
        name: impl Into<String>,
        schema: Value,
    ) -> Self {
        Self {
            messages,
            max_tokens,
            response_schema: Some(JsonResponseSchema {
                name: name.into(),
                schema,
            }),
        }
    }
}

#[derive(Debug, Clone)]
pub struct JsonResponseSchema {
    pub name: String,
    pub schema: Value,
}

#[async_trait]
pub trait LanguageModel: Send + Sync {
    async fn complete(&self, request: ModelRequest) -> Result<String>;
}

pub type SharedLanguageModel = Arc<dyn LanguageModel>;

#[derive(Clone)]
pub struct OpenRouterClient {
    client: Client,
    endpoint: String,
    api_key: String,
    model: String,
}

impl OpenRouterClient {
    pub fn new(base_url: &str, api_key: String, model: String) -> Result<Self> {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            "X-OpenRouter-Title",
            header::HeaderValue::from_static(OPENROUTER_TITLE),
        );
        let client = Client::builder()
            .default_headers(headers)
            .build()
            .context("failed to build OpenRouter HTTP client")?;

        Ok(Self {
            client,
            endpoint: format!("{}/chat/completions", base_url.trim_end_matches('/')),
            api_key,
            model,
        })
    }
}

#[async_trait]
impl LanguageModel for OpenRouterClient {
    async fn complete(&self, request: ModelRequest) -> Result<String> {
        let response_format = request.response_schema.map(|response_schema| {
            json!({
                "type": "json_schema",
                "json_schema": {
                    "name": response_schema.name,
                    "strict": true,
                    "schema": response_schema.schema,
                }
            })
        });
        let payload = ChatCompletionRequest {
            model: &self.model,
            messages: &request.messages,
            max_tokens: request.max_tokens,
            response_format,
        };

        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .json(&payload)
            .send()
            .await
            .context("OpenRouter request failed")?;
        let status = response.status();
        if !status.is_success() {
            return Err(openrouter_status_error(status));
        }

        let completion: ChatCompletionResponse = response
            .json()
            .await
            .context("invalid OpenRouter response")?;
        let content = completion
            .choices
            .into_iter()
            .next()
            .and_then(|choice| choice.message.content)
            .filter(|content| !content.trim().is_empty());

        content.context("OpenRouter returned no message content")
    }
}

pub async fn complete_json<T: DeserializeOwned>(
    model: &dyn LanguageModel,
    request: ModelRequest,
) -> Result<T> {
    let content = model.complete(request).await?;
    serde_json::from_str(&content).context("model returned invalid structured JSON")
}

fn openrouter_status_error(status: StatusCode) -> anyhow::Error {
    // Provider error bodies can echo request data. Do not include them in logs.
    anyhow!("OpenRouter returned HTTP {status}")
}

#[derive(Serialize)]
struct ChatCompletionRequest<'a> {
    model: &'a str,
    messages: &'a [PromptMessage],
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<Value>,
}

#[derive(Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
}
