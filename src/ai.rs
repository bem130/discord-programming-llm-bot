use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::{Client, StatusCode, header};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

const OPENROUTER_TITLE: &str = "Discord Learn Programming Bot";
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REASONING_REQUIRED_ERROR: &str =
    "Provider requires reasoning and does not allow disabling it";
const SERVER_TOOL_FAILURE_ERROR: &str = "Server tool request failed";

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
            content: content
                .map(PromptContent::Text)
                .unwrap_or(PromptContent::Null),
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
    Null,
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
        Self::with_models_and_timeout(base_url, api_key, vec![model], DEFAULT_REQUEST_TIMEOUT)
    }

    pub fn with_models(base_url: &str, api_key: String, models: Vec<String>) -> Result<Self> {
        Self::with_models_and_timeout(base_url, api_key, models, DEFAULT_REQUEST_TIMEOUT)
    }

    pub fn new_with_timeout(
        base_url: &str,
        api_key: String,
        model: String,
        request_timeout: Duration,
    ) -> Result<Self> {
        Self::with_models_and_timeout(base_url, api_key, vec![model], request_timeout)
    }

    fn with_models_and_timeout(
        base_url: &str,
        api_key: String,
        models: Vec<String>,
        request_timeout: Duration,
    ) -> Result<Self> {
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
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(request_timeout)
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
        match self.complete_response_once(request.clone()).await {
            Err(error) if request.reasoning.is_some() && is_reasoning_disabled_error(&error) => {
                tracing::warn!(
                    error = %format!("{error:#}"),
                    "OpenRouter rejected the requested reasoning override; retrying with provider defaults"
                );
                let mut fallback = request.clone();
                fallback.reasoning = None;
                self.complete_response_once(fallback).await
            }
            Err(error)
                if request.web_search
                    && error
                        .downcast_ref::<OpenRouterHttpError>()
                        .is_some_and(is_server_tool_failure) =>
            {
                tracing::warn!(
                    error = %format!("{error:#}"),
                    "OpenRouter web search server tool failed; retrying without web search"
                );
                let mut fallback = request;
                fallback.web_search = false;
                self.complete_response_once(fallback).await
            }
            result => result,
        }
    }
}

impl OpenRouterClient {
    async fn complete_response_once(&self, request: ModelRequest) -> Result<ModelResponse> {
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
            let (body, truncated, read_failed) = match read_error_body(response).await {
                Ok((body, truncated)) => (body, truncated, false),
                Err(_) => (String::new(), false, true),
            };
            let diagnostic = rejection_diagnostics(&body);
            let mut message = openrouter_error_message(&body);
            if status == StatusCode::FORBIDDEN {
                message = format!(
                    "{message} [error_type={}, scope={}, provider={}, response={}]",
                    diagnostic.error_type,
                    diagnostic.scope,
                    diagnostic.provider,
                    diagnostic.body_kind
                );
            }
            let (image_count, image_chars, text_chars) = request_content_stats(&request.messages);
            tracing::warn!(
                status = status.as_u16(),
                diagnostic = ?diagnostic,
                response_bytes = body.len(),
                response_truncated = truncated,
                response_read_failed = read_failed,
                shape = message_sequence_shape(&request.messages),
                image_count, image_chars, text_chars,
                tool_count = payload.tools.as_ref().map_or(0, Vec::len),
                web_search = request.web_search,
                "OpenRouter rejected request (content omitted)"
            );
            return Err(openrouter_status_error(status, retry_after, message));
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

fn openrouter_status_error(
    status: StatusCode,
    retry_after: Option<Duration>,
    message: String,
) -> anyhow::Error {
    OpenRouterHttpError {
        status,
        retry_after,
        message,
    }
    .into()
}

fn is_server_tool_failure(error: &OpenRouterHttpError) -> bool {
    error.message == SERVER_TOOL_FAILURE_ERROR
        && (error.status == StatusCode::BAD_REQUEST || error.status.is_server_error())
}

fn is_reasoning_disabled_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<OpenRouterHttpError>()
        .is_some_and(|error| {
            error.status == StatusCode::BAD_REQUEST && error.message == REASONING_REQUIRED_ERROR
        })
}

async fn read_error_body(mut response: reqwest::Response) -> Result<(String, bool)> {
    const MAX_BYTES: usize = 64 * 1024;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("failed to read OpenRouter error response")?
    {
        let remaining = MAX_BYTES - bytes.len();
        if chunk.len() > remaining {
            bytes.extend_from_slice(&chunk[..remaining]);
            return Ok((String::from_utf8_lossy(&bytes).into_owned(), true));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((String::from_utf8_lossy(&bytes).into_owned(), false))
}

#[derive(Debug)]
struct RejectionDiagnostics {
    body_kind: &'static str,
    error_type: &'static str,
    scope: &'static str,
    provider: &'static str,
}

fn rejection_diagnostics(body: &str) -> RejectionDiagnostics {
    // Only fixed labels leave this function: never raw provider errors, flagged input,
    // moderation patterns, arbitrary codes, or a reflected prompt/API credential.
    let parsed = serde_json::from_str::<Value>(body).ok();
    let metadata = parsed
        .as_ref()
        .and_then(|value| value.pointer("/error/metadata"));
    let reported_type = metadata
        .and_then(|value| value.get("error_type"))
        .and_then(Value::as_str);
    const TYPES: &[&str] = &[
        "authentication",
        "permission_denied",
        "payment_required",
        "content_policy_violation",
        "refusal",
        "context_length_exceeded",
        "max_tokens_exceeded",
        "token_limit_exceeded",
        "string_too_long",
        "rate_limit_exceeded",
        "provider_overloaded",
        "provider_unavailable",
        "invalid_request",
        "invalid_prompt",
        "not_found",
        "precondition_failed",
        "payload_too_large",
        "unprocessable",
        "invalid_image",
        "image_too_large",
        "image_too_small",
        "unsupported_image_format",
        "image_not_found",
        "image_download_failed",
        "server",
        "timeout",
        "unmapped",
    ];
    let error_type = TYPES
        .iter()
        .copied()
        .find(|kind| Some(*kind) == reported_type)
        .unwrap_or("unknown");
    let provider = match metadata
        .and_then(|value| value.get("provider_name"))
        .and_then(Value::as_str)
    {
        Some("OpenAI") => "OpenAI",
        Some("Azure") => "Azure",
        Some("Anthropic") => "Anthropic",
        Some("Google") | Some("Google AI Studio") | Some("Google Vertex") => "Google",
        Some("DeepSeek") => "DeepSeek",
        Some("OpenRouter") => "OpenRouter",
        Some(_) => "other",
        None => "unknown",
    };
    let scope = if metadata
        .is_some_and(|value| value.get("patterns").is_some() || value.get("guardrail_id").is_some())
    {
        "guardrail"
    } else if metadata
        .is_some_and(|value| value.get("flagged_input").is_some() || value.get("reasons").is_some())
    {
        "moderation"
    } else if provider != "unknown" {
        "provider"
    } else {
        "unknown"
    };
    let body_kind = if body.trim().is_empty() {
        "empty"
    } else if parsed.is_some() {
        "json"
    } else if body.trim_start().starts_with('<') {
        "html_or_xml"
    } else {
        "text"
    };
    RejectionDiagnostics {
        body_kind,
        error_type,
        scope,
        provider,
    }
}

fn request_content_stats(messages: &[PromptMessage]) -> (usize, usize, usize) {
    let (mut images, mut image_chars, mut text_chars) = (0, 0, 0);
    for message in messages {
        match &message.content {
            PromptContent::Text(text) => text_chars += text.len(),
            PromptContent::Parts(parts) => {
                for part in parts {
                    match part {
                        ContentPart::ImageUrl { image_url } => {
                            images += 1;
                            image_chars += image_url.url.len();
                        }
                        ContentPart::Text { text } => text_chars += text.len(),
                        ContentPart::File { .. } => {}
                    }
                }
            }
            PromptContent::Null => {}
        }
    }
    (images, image_chars, text_chars)
}

fn openrouter_error_message(body: &str) -> String {
    const MAX_INSPECTED_BODY_CHARS: usize = 4_000;

    let body = body.trim();
    if body.chars().count() > MAX_INSPECTED_BODY_CHARS {
        return "response body too large to inspect".to_string();
    }
    let parsed = serde_json::from_str::<Value>(body).ok();
    let extracted = parsed
        .as_ref()
        .and_then(|value| value.pointer("/error/message"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .unwrap_or(body);
    let normalized = extracted.to_ascii_lowercase();
    if normalized.contains("reasoning is mandatory") && normalized.contains("cannot be disabled") {
        REASONING_REQUIRED_ERROR.to_owned()
    } else if normalized.contains("server tool request failed") {
        SERVER_TOOL_FAILURE_ERROR.to_owned()
    } else if normalized.contains("invalid message") {
        "Invalid message format".to_owned()
    } else if normalized.contains("rate limit") {
        "Rate limit exceeded".to_owned()
    } else {
        "OpenRouter rejected the request".to_owned()
    }
}

fn message_sequence_shape(messages: &[PromptMessage]) -> String {
    messages
        .iter()
        .take(128)
        .map(|message| {
            let role = match message.role {
                PromptRole::System => "system",
                PromptRole::User => "user",
                PromptRole::Assistant => "assistant",
                PromptRole::Tool => "tool",
            };
            let content = match &message.content {
                PromptContent::Text(text) if text.is_empty() => "text(empty)",
                PromptContent::Text(_) => "text",
                PromptContent::Parts(_) => "parts",
                PromptContent::Null => "null",
            };
            format!(
                "{role}:{content}:tc={}:tid={}",
                message.tool_calls.as_ref().map_or(0, Vec::len),
                message.tool_call_id.is_some()
            )
        })
        .collect::<Vec<_>>()
        .join(" | ")
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
#[error("OpenRouter returned HTTP {status}: {message}")]
struct OpenRouterHttpError {
    status: StatusCode,
    retry_after: Option<Duration>,
    message: String,
}

#[derive(Serialize)]
struct ChatCompletionRequest<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    models: Option<&'a [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    messages: &'a [PromptMessage],
    #[serde(rename = "max_completion_tokens")]
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
        let Ok(parsed) = reqwest::Url::parse(&citation.url) else {
            continue;
        };
        let Some(host) = parsed.host_str() else {
            continue;
        };
        if !matches!(parsed.scheme(), "http" | "https")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || reference_host_is_local(host)
        {
            continue;
        }
        if links.iter().any(|(_, url)| url == parsed.as_str()) || content.contains(parsed.as_str())
        {
            continue;
        }
        let title = citation
            .title
            .as_deref()
            .filter(|title| {
                !title.trim().is_empty()
                    && !title.to_ascii_lowercase().contains("localhost")
                    && !title.contains("127.0.0.1")
                    && !title.contains("[::1]")
            })
            .unwrap_or(host);
        let title: String = title
            .chars()
            .take(200)
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        links.push((
            title.replace('[', "\\[").replace(']', "\\]"),
            parsed.to_string(),
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

fn reference_host_is_local(host: &str) -> bool {
    let host = host
        .trim_end_matches('.')
        .trim_matches(['[', ']'])
        .to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => {
            ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified()
        }
        Ok(std::net::IpAddr::V6(ip)) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip.to_ipv4_mapped().is_some_and(|ip| {
                    ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified()
                })
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejection_diagnostics_preserve_fixed_categories_without_echoing_input() {
        let body = json!({"error": {"message": "DUMMY_PRIVATE_ECHO", "metadata": {
            "error_type": "content_policy_violation", "provider_name": "Azure",
            "flagged_input": "DUMMY_PRIVATE_ECHO", "reasons": ["DUMMY_PRIVATE_ECHO"], "raw": "DUMMY_PRIVATE_ECHO"
        }}}).to_string();
        let diagnostic = rejection_diagnostics(&body);
        assert_eq!(diagnostic.error_type, "content_policy_violation");
        assert_eq!(diagnostic.scope, "moderation");
        assert_eq!(diagnostic.provider, "Azure");
        assert!(!format!("{diagnostic:?}").contains("DUMMY_PRIVATE_ECHO"));
        let guardrail = rejection_diagnostics(
            r#"{"error":{"metadata":{"error_type":"permission_denied","patterns":["DUMMY_PRIVATE_ECHO"]}}}"#,
        );
        assert_eq!(guardrail.scope, "guardrail");
        let unknown = rejection_diagnostics(
            r#"{"error":{"metadata":{"error_type":"DUMMY_PRIVATE_ECHO","provider_name":"DUMMY_PRIVATE_ECHO"}}}"#,
        );
        assert_eq!(unknown.error_type, "unknown");
        assert_eq!(unknown.provider, "other");
        assert!(!format!("{unknown:?}").contains("DUMMY_PRIVATE_ECHO"));
        assert_eq!(
            rejection_diagnostics("<html>access denied</html>").body_kind,
            "html_or_xml"
        );
        assert_eq!(rejection_diagnostics("").body_kind, "empty");
    }

    #[test]
    fn forbidden_responses_never_trigger_parameter_or_tool_fallbacks() {
        for message in [
            SERVER_TOOL_FAILURE_ERROR,
            REASONING_REQUIRED_ERROR,
            "content_policy_violation",
        ] {
            let error = OpenRouterHttpError {
                status: StatusCode::FORBIDDEN,
                retry_after: Some(Duration::from_secs(1)),
                message: message.into(),
            };
            assert!(!is_server_tool_failure(&error));
            let error: anyhow::Error = error.into();
            assert!(!is_reasoning_disabled_error(&error));
            assert!(model_retry_delay(&error, Duration::from_millis(100)).is_none());
        }
    }

    #[test]
    fn request_diagnostics_measure_images_without_echoing_content() {
        let messages = vec![
            PromptMessage::user("private text"),
            PromptMessage::user_parts(vec![
                ContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url: "data:image/png;base64,private".into(),
                    },
                },
                ContentPart::Text {
                    text: "more text".into(),
                },
            ]),
        ];
        assert_eq!(request_content_stats(&messages), (1, 29, 21));
        let shape = message_sequence_shape(&messages);
        assert!(!shape.contains("private"));
    }

    #[test]
    fn references_skip_internal_urls_duplicate_links_and_converter_titles() {
        let annotations: Vec<Annotation> = serde_json::from_value(serde_json::json!([
            {"url_citation": {"url": "http://localhost:8123/file.pdf", "title": "converted"}},
            {"url_citation": {"url": "http://127.0.0.1:8123/file.pdf"}},
            {"url_citation": {"url": "http://[::1]/file.pdf"}},
            {"url_citation": {"url": "http://10.0.0.1/file.pdf"}},
            {"url_citation": {"url": "file:///tmp/file.pdf"}},
            {"url_citation": {"url": "https://docs.arduino.cc/pinout.pdf", "title": "localhost:8123"}},
            {"url_citation": {"url": "https://docs.arduino.cc/pinout.pdf", "title": "duplicate"}}
        ])).unwrap();
        let mut content = "説明".to_string();
        append_reference_links(&mut content, &annotations);
        assert!(!content.contains("localhost"));
        assert!(!content.contains("127.0.0.1"));
        assert!(!content.contains("10.0.0.1"));
        assert!(content.contains("[docs.arduino.cc](https://docs.arduino.cc/pinout.pdf)"));
        assert_eq!(
            content
                .matches("https://docs.arduino.cc/pinout.pdf")
                .count(),
            1
        );
        let original = content.clone();
        append_reference_links(&mut content, &annotations);
        assert_eq!(content, original);
    }

    #[test]
    fn tool_only_assistant_message_serializes_content_as_null() {
        let tool_call = ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: ToolFunction {
                name: "execute_code_in_sandbox".into(),
                arguments: r#"{"code":"print(1)"}"#.into(),
            },
        };

        let tool_only = PromptMessage::assistant_tool_calls(None, vec![tool_call.clone()]);
        let payload = serde_json::to_value(&tool_only).unwrap();
        assert_eq!(payload["role"], "assistant");
        assert_eq!(payload["content"], serde_json::Value::Null);
        assert_eq!(payload["tool_calls"][0]["id"], "call_1");

        let with_text =
            PromptMessage::assistant_tool_calls(Some("thinking".into()), vec![tool_call]);
        let payload = serde_json::to_value(&with_text).unwrap();
        assert_eq!(payload["content"], "thinking");
    }

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
    fn reasoning_effort_is_encoded_for_structured_reviews() {
        let request = ModelRequest::json(vec![], 1_200, "review", json!({ "type": "object" }))
            .with_reasoning_effort("low");

        assert_eq!(
            request.reasoning,
            Some(json!({ "effort": "low", "exclude": true }))
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
        assert_eq!(payload["max_completion_tokens"], 100);
        assert!(payload.get("max_tokens").is_none());
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
        let unauthorized =
            openrouter_status_error(StatusCode::UNAUTHORIZED, None, "unauthorized".into());
        assert_eq!(
            model_retry_delay(&unauthorized, Duration::from_millis(200)),
            None
        );
        assert!(model_error_is_global_fatal(&unauthorized));

        let short_limit = openrouter_status_error(
            StatusCode::TOO_MANY_REQUESTS,
            Some(Duration::from_secs(1)),
            "rate limited".into(),
        );
        assert!(!model_error_is_global_fatal(&short_limit));
        assert_eq!(
            model_retry_delay(&short_limit, Duration::from_millis(200)),
            Some(Duration::from_secs(1))
        );

        let long_limit = openrouter_status_error(
            StatusCode::TOO_MANY_REQUESTS,
            Some(Duration::from_secs(60)),
            "rate limited".into(),
        );
        assert_eq!(
            model_retry_delay(&long_limit, Duration::from_millis(200)),
            None
        );
    }

    #[test]
    fn extracts_openrouter_error_message_and_truncates_oversized_bodies() {
        assert_eq!(
            openrouter_error_message(
                r#"{"error":{"message":"Reasoning is mandatory for this endpoint and cannot be disabled.","code":400}}"#
            ),
            REASONING_REQUIRED_ERROR
        );
        assert_eq!(
            openrouter_error_message(
                r#"{"error":{"message":"Invalid message format","code":400}}"#
            ),
            "Invalid message format"
        );
        assert_eq!(
            openrouter_error_message("plain text body"),
            "OpenRouter rejected the request"
        );
        assert_eq!(
            openrouter_error_message(&"x".repeat(5_000)),
            "response body too large to inspect"
        );
        assert_eq!(
            openrouter_error_message(&format!(
                r#"{{"error":{{"message":"{}"}}}}"#,
                "y".repeat(1_000)
            ))
            .as_str(),
            "OpenRouter rejected the request"
        );
    }

    #[test]
    fn extracts_provider_name_from_error_metadata() {
        let body = r#"{"error":{"message":"Provider returned error","metadata":{"provider_name":"DeepSeek"}}}"#;
        assert_eq!(rejection_diagnostics(body).provider, "DeepSeek");
        assert_eq!(rejection_diagnostics("not json").provider, "unknown");
        assert_eq!(
            rejection_diagnostics(r#"{"error":{"message":"boom"}}"#).provider,
            "unknown"
        );
    }

    #[test]
    fn detects_server_tool_failures() {
        assert!(is_server_tool_failure(&OpenRouterHttpError {
            status: StatusCode::BAD_REQUEST,
            retry_after: None,
            message: "Server tool request failed".into(),
        }));
        assert!(!is_server_tool_failure(&OpenRouterHttpError {
            status: StatusCode::BAD_REQUEST,
            retry_after: None,
            message: "Invalid message format".into(),
        }));
    }

    #[test]
    fn detects_provider_rejection_of_disabled_reasoning() {
        let error: anyhow::Error = OpenRouterHttpError {
            status: StatusCode::BAD_REQUEST,
            retry_after: None,
            message: REASONING_REQUIRED_ERROR.into(),
        }
        .into();
        assert!(is_reasoning_disabled_error(&error));
    }

    #[test]
    fn message_sequence_shape_marks_roles_and_structure() {
        let messages = vec![
            PromptMessage::system("system prompt"),
            PromptMessage::assistant_tool_calls(
                None,
                vec![ToolCall {
                    id: "call_1".into(),
                    kind: "function".into(),
                    function: ToolFunction {
                        name: "execute_code_in_sandbox".into(),
                        arguments: "{}".into(),
                    },
                }],
            ),
            PromptMessage::tool_result("call_1", "output"),
        ];

        assert_eq!(
            message_sequence_shape(&messages),
            "system:text:tc=0:tid=false | assistant:null:tc=1:tid=false | tool:text:tc=0:tid=true"
        );
    }
}
