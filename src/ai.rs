use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::{Client, StatusCode, header};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

const OPENROUTER_TITLE: &str = "Discord Learn Programming Bot";

#[derive(Debug, Clone, Serialize)]
pub struct PromptMessage {
    pub role: PromptRole,
    pub content: PromptContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl PromptMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: PromptRole::System,
            content: PromptContent::Text(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: PromptRole::User,
            content: PromptContent::Text(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: PromptRole::Assistant,
            content: PromptContent::Text(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn user_parts(parts: Vec<ContentPart>) -> Self {
        Self {
            role: PromptRole::User,
            content: PromptContent::Parts(parts),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn assistant_tool_calls(content: Option<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: PromptRole::Assistant,
            content: PromptContent::Text(content.unwrap_or_default()),
            tool_calls: Some(tool_calls),
            tool_call_id: None,
        }
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: PromptRole::Tool,
            content: PromptContent::Text(content.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum PromptContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
    File { file: FileInput },
}

#[derive(Debug, Clone, Serialize)]
pub struct ImageUrl {
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileInput {
    pub filename: String,
    pub file_data: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub messages: Vec<PromptMessage>,
    pub max_tokens: u32,
    pub response_schema: Option<JsonResponseSchema>,
    pub web_search: bool,
    pub tools: Vec<Value>,
    pub reasoning: Option<Value>,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiSessionPurpose {
    Conversation,
    ReplyDecision,
    CodeExecution,
    MemoryReview,
    ResponseReview,
}

impl AiSessionPurpose {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::ReplyDecision => "reply-decision",
            Self::CodeExecution => "code-execution",
            Self::MemoryReview => "memory-review",
            Self::ResponseReview => "response-review",
        }
    }
}

impl ModelRequest {
    pub fn text(messages: Vec<PromptMessage>, max_tokens: u32) -> Self {
        Self {
            messages,
            max_tokens,
            response_schema: None,
            web_search: false,
            tools: vec![],
            reasoning: None,
            session_id: None,
        }
    }

    pub fn text_with_web_search(messages: Vec<PromptMessage>, max_tokens: u32) -> Self {
        Self {
            messages,
            max_tokens,
            response_schema: None,
            web_search: true,
            tools: vec![],
            reasoning: None,
            session_id: None,
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
            web_search: false,
            tools: vec![],
            reasoning: None,
            session_id: None,
        }
    }

    pub fn with_tool(mut self, tool: Value) -> Self {
        self.tools.push(tool);
        self
    }

    pub fn with_reasoning_effort(mut self, effort: &'static str) -> Self {
        self.reasoning = Some(json!({
            "effort": effort,
            "exclude": true
        }));
        self
    }

    pub fn with_guild_session(mut self, guild_id: u64, purpose: AiSessionPurpose) -> Self {
        self.session_id = Some(format!(
            "discord-learn-programming-bot:guild:{guild_id}:{}",
            purpose.as_str()
        ));
        self
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

    async fn complete_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.complete(request).await.map(|content| ModelResponse {
            content: Some(content),
            tool_calls: vec![],
        })
    }
}

pub type SharedLanguageModel = Arc<dyn LanguageModel>;

#[derive(Clone)]
pub struct OpenRouterClient {
    client: Client,
    endpoint: String,
    api_key: String,
    model: String,
    fallback_models: Vec<String>,
}

impl OpenRouterClient {
    pub fn new(base_url: &str, api_key: String, model: String) -> Result<Self> {
        Self::with_models(base_url, api_key, vec![model])
    }

    pub fn with_models(base_url: &str, api_key: String, models: Vec<String>) -> Result<Self> {
        let mut models = models.into_iter();
        let model = models
            .next()
            .filter(|model| !model.trim().is_empty())
            .context("OpenRouter model list is empty")?;
        let fallback_models: Vec<_> = models.filter(|model| !model.trim().is_empty()).collect();
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
            fallback_models,
        })
    }
}

#[async_trait]
impl LanguageModel for OpenRouterClient {
    async fn complete(&self, request: ModelRequest) -> Result<String> {
        let response = self.complete_response(request).await?;
        response
            .content
            .filter(|content| !content.trim().is_empty())
            .context("OpenRouter returned no message content")
    }

    async fn complete_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        let heals_structured_response = request.response_schema.is_some();
        let requires_parameter_support =
            heals_structured_response || !request.tools.is_empty() || request.reasoning.is_some();
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
        let mut tools = request.tools;
        if request.web_search {
            tools.insert(
                0,
                json!({
                    "type": "openrouter:web_search",
                    "parameters": {
                        "max_results": 5,
                        "max_total_results": 8,
                        "search_context_size": "low"
                    }
                }),
            );
            tools.insert(
                1,
                json!({
                    "type": "openrouter:web_fetch"
                }),
            );
        }
        let payload = ChatCompletionRequest {
            model: &self.model,
            models: (!self.fallback_models.is_empty()).then_some(self.fallback_models.as_slice()),
            session_id: request.session_id.as_deref(),
            messages: &request.messages,
            max_tokens: request.max_tokens,
            response_format,
            provider: requires_parameter_support.then(|| {
                json!({
                    "require_parameters": true
                })
            }),
            plugins: heals_structured_response.then(|| {
                vec![json!({
                    "id": "response-healing"
                })]
            }),
            reasoning: request.reasoning,
            tools: (!tools.is_empty()).then_some(tools),
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
        let retry_after = response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs);
        if !status.is_success() {
            return Err(openrouter_status_error(status, retry_after));
        }

        let completion: ChatCompletionResponse = response
            .json()
            .await
            .context("invalid OpenRouter response")?;
        tracing::debug!(
            requested_model = %self.model,
            used_model = completion.model.as_deref().unwrap_or("unknown"),
            "OpenRouter completion received"
        );
        let used_model = completion.model.as_deref().unwrap_or("unknown").to_owned();
        let choice = completion
            .choices
            .into_iter()
            .next()
            .context("OpenRouter returned no choices")?;
        let finish_reason = choice
            .finish_reason
            .as_deref()
            .unwrap_or("unknown")
            .to_owned();
        let message = choice.message;
        let mut content = message.content;
        if request.web_search
            && let Some(content) = content.as_mut()
        {
            append_reference_links(content, &message.annotations);
        }
        if content
            .as_ref()
            .is_none_or(|content| content.trim().is_empty())
            && message.tool_calls.is_empty()
        {
            anyhow::bail!(
                "OpenRouter returned no message content (model={used_model}, finish_reason={finish_reason})"
            );
        }
        Ok(ModelResponse {
            content,
            tool_calls: message.tool_calls,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ModelResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    pub name: String,
    pub arguments: String,
}

pub async fn complete_json<T: DeserializeOwned>(
    model: &dyn LanguageModel,
    request: ModelRequest,
) -> Result<T> {
    let content = model.complete(request).await?;
    serde_json::from_str(&content).context("model returned invalid structured JSON")
}

pub async fn complete_with_retry(
    model: &dyn LanguageModel,
    request: ModelRequest,
    operation: &'static str,
) -> Result<String> {
    const MAX_ATTEMPTS: usize = 3;
    const BASE_DELAY: Duration = Duration::from_millis(200);

    let mut last_error = None;
    for attempt in 1..=MAX_ATTEMPTS {
        match model.complete(request.clone()).await {
            Ok(content) => return Ok(content),
            Err(error) => {
                let fallback =
                    BASE_DELAY.saturating_mul(1_u32 << (attempt.saturating_sub(1) as u32));
                let delay = model_retry_delay(&error, fallback);
                tracing::warn!(
                    operation,
                    attempt,
                    max_attempts = MAX_ATTEMPTS,
                    retry_delay_ms = delay.map(|delay| delay.as_millis()),
                    error = %format!("{error:#}"),
                    "model operation attempt failed"
                );
                last_error = Some(error);
                if attempt == MAX_ATTEMPTS || delay.is_none() {
                    break;
                }
                tokio::time::sleep(delay.unwrap_or_default()).await;
            }
        }
    }
    Err(last_error.context("model operation produced no error detail")?)
}

pub async fn complete_response_with_retry(
    model: &dyn LanguageModel,
    request: ModelRequest,
    operation: &'static str,
) -> Result<ModelResponse> {
    const MAX_ATTEMPTS: usize = 3;
    const BASE_DELAY: Duration = Duration::from_millis(200);

    let mut last_error = None;
    for attempt in 1..=MAX_ATTEMPTS {
        match model.complete_response(request.clone()).await {
            Ok(response) => return Ok(response),
            Err(error) => {
                let fallback =
                    BASE_DELAY.saturating_mul(1_u32 << (attempt.saturating_sub(1) as u32));
                let delay = model_retry_delay(&error, fallback);
                tracing::warn!(
                    operation,
                    attempt,
                    max_attempts = MAX_ATTEMPTS,
                    retry_delay_ms = delay.map(|delay| delay.as_millis()),
                    error = %format!("{error:#}"),
                    "model tool operation attempt failed"
                );
                last_error = Some(error);
                if attempt == MAX_ATTEMPTS || delay.is_none() {
                    break;
                }
                tokio::time::sleep(delay.unwrap_or_default()).await;
            }
        }
    }
    Err(last_error.context("model tool operation produced no error detail")?)
}

pub async fn complete_json_with_retry<T: DeserializeOwned>(
    model: &dyn LanguageModel,
    request: ModelRequest,
    operation: &'static str,
) -> Result<T> {
    const MAX_ATTEMPTS: usize = 3;
    const BASE_DELAY: Duration = Duration::from_millis(200);

    let mut last_error = None;
    for attempt in 1..=MAX_ATTEMPTS {
        match complete_json(model, request.clone()).await {
            Ok(value) => return Ok(value),
            Err(error) => {
                let fallback =
                    BASE_DELAY.saturating_mul(1_u32 << (attempt.saturating_sub(1) as u32));
                let delay = model_retry_delay(&error, fallback);
                tracing::warn!(
                    operation,
                    attempt,
                    max_attempts = MAX_ATTEMPTS,
                    retry_delay_ms = delay.map(|delay| delay.as_millis()),
                    error = %format!("{error:#}"),
                    "structured model operation attempt failed"
                );
                last_error = Some(error);
                if attempt == MAX_ATTEMPTS || delay.is_none() {
                    break;
                }
                tokio::time::sleep(delay.unwrap_or_default()).await;
            }
        }
    }
    Err(last_error.context("structured model operation produced no error detail")?)
}

fn openrouter_status_error(status: StatusCode, retry_after: Option<Duration>) -> anyhow::Error {
    // Provider error bodies can echo request data. Do not include them in logs.
    OpenRouterHttpError {
        status,
        retry_after,
    }
    .into()
}

pub fn model_retry_delay(error: &anyhow::Error, fallback: Duration) -> Option<Duration> {
    const MAX_RETRY_DELAY: Duration = Duration::from_secs(2);

    let Some(http_error) = error.downcast_ref::<OpenRouterHttpError>() else {
        return Some(fallback);
    };
    let retryable = http_error.status == StatusCode::REQUEST_TIMEOUT
        || http_error.status == StatusCode::TOO_MANY_REQUESTS
        || http_error.status.is_server_error();
    if !retryable {
        return None;
    }
    let delay = http_error.retry_after.unwrap_or(fallback);
    (delay <= MAX_RETRY_DELAY).then_some(delay)
}

pub fn model_error_is_global_fatal(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<OpenRouterHttpError>()
        .is_some_and(|error| {
            matches!(
                error.status,
                StatusCode::UNAUTHORIZED | StatusCode::PAYMENT_REQUIRED
            )
        })
}

#[derive(Debug, thiserror::Error)]
#[error("OpenRouter returned HTTP {status}")]
struct OpenRouterHttpError {
    status: StatusCode,
    retry_after: Option<Duration>,
}

#[derive(Serialize)]
struct ChatCompletionRequest<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    models: Option<&'a [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    messages: &'a [PromptMessage],
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plugins: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Value>>,
}

#[derive(Deserialize)]
struct ChatCompletionResponse {
    #[serde(default)]
    model: Option<String>,
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
    #[serde(default)]
    annotations: Vec<Annotation>,
}

#[derive(Deserialize)]
struct Annotation {
    #[serde(default)]
    url_citation: Option<UrlCitation>,
}

#[derive(Deserialize)]
struct UrlCitation {
    url: String,
    title: Option<String>,
}

fn append_reference_links(content: &mut String, annotations: &[Annotation]) {
    let mut links: Vec<(String, String)> = Vec::new();
    for annotation in annotations {
        let Some(citation) = &annotation.url_citation else {
            continue;
        };
        if links.iter().any(|(_, url)| url == &citation.url) {
            continue;
        }
        links.push((
            citation
                .title
                .clone()
                .unwrap_or_else(|| citation.url.clone()),
            citation.url.clone(),
        ));
    }
    if links.is_empty() {
        return;
    }
    content.push_str("\n\n参考リンク\n");
    for (title, url) in links {
        content.push_str(&format!("- [{title}]({url})\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_model_order_is_preserved() {
        let client = OpenRouterClient::with_models(
            "https://example.invalid/api/v1",
            "dummy-key".into(),
            vec![
                "free/first".into(),
                "free/router".into(),
                "paid/last".into(),
            ],
        )
        .unwrap();

        assert_eq!(client.model, "free/first");
        assert_eq!(client.fallback_models, ["free/router", "paid/last"]);
    }

    #[test]
    fn empty_model_list_is_rejected() {
        assert!(
            OpenRouterClient::with_models(
                "https://example.invalid/api/v1",
                "dummy-key".into(),
                vec![]
            )
            .is_err()
        );
    }

    #[test]
    fn reasoning_effort_can_be_disabled_for_short_structured_reviews() {
        let request = ModelRequest::json(vec![], 1_200, "review", json!({ "type": "object" }))
            .with_reasoning_effort("none");

        assert_eq!(
            request.reasoning,
            Some(json!({ "effort": "none", "exclude": true }))
        );
    }

    #[test]
    fn guild_sessions_are_stable_and_never_use_discord_channel_ids() {
        let first =
            ModelRequest::text(vec![], 100).with_guild_session(123, AiSessionPurpose::Conversation);
        let second =
            ModelRequest::text(vec![], 100).with_guild_session(123, AiSessionPurpose::Conversation);
        let review = ModelRequest::text(vec![], 100)
            .with_guild_session(123, AiSessionPurpose::ResponseReview);

        assert_eq!(first.session_id, second.session_id);
        assert_eq!(
            first.session_id.as_deref(),
            Some("discord-learn-programming-bot:guild:123:conversation")
        );
        assert_ne!(first.session_id, review.session_id);
        assert!(
            first
                .session_id
                .as_deref()
                .is_some_and(|session_id| !session_id.contains("channel"))
        );
    }

    #[test]
    fn openrouter_payload_includes_session_id() {
        let messages = vec![PromptMessage::system("stable prompt")];
        let payload = serde_json::to_value(ChatCompletionRequest {
            model: "test/model",
            models: None,
            session_id: Some("discord-learn-programming-bot:guild:123:conversation"),
            messages: &messages,
            max_tokens: 100,
            response_format: None,
            provider: None,
            plugins: None,
            reasoning: None,
            tools: None,
        })
        .unwrap();

        assert_eq!(
            payload["session_id"],
            "discord-learn-programming-bot:guild:123:conversation"
        );
    }

    #[test]
    fn web_search_injects_both_search_and_fetch_tools() {
        let mut tools = vec![];
        if true {
            tools.insert(
                0,
                json!({
                    "type": "openrouter:web_search",
                    "parameters": {
                        "max_results": 5,
                        "max_total_results": 8,
                        "search_context_size": "low"
                    }
                }),
            );
            tools.insert(
                1,
                json!({
                    "type": "openrouter:web_fetch"
                }),
            );
        }
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["type"], "openrouter:web_search");
        assert_eq!(tools[1]["type"], "openrouter:web_fetch");
    }

    #[test]
    fn retry_advice_respects_status_and_short_retry_after() {
        let unauthorized = openrouter_status_error(StatusCode::UNAUTHORIZED, None);
        assert_eq!(
            model_retry_delay(&unauthorized, Duration::from_millis(200)),
            None
        );
        assert!(model_error_is_global_fatal(&unauthorized));

        let short_limit =
            openrouter_status_error(StatusCode::TOO_MANY_REQUESTS, Some(Duration::from_secs(1)));
        assert!(!model_error_is_global_fatal(&short_limit));
        assert_eq!(
            model_retry_delay(&short_limit, Duration::from_millis(200)),
            Some(Duration::from_secs(1))
        );

        let long_limit =
            openrouter_status_error(StatusCode::TOO_MANY_REQUESTS, Some(Duration::from_secs(60)));
        assert_eq!(
            model_retry_delay(&long_limit, Duration::from_millis(200)),
            None
        );
    }
}
