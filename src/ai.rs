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
    pub content: PromptContent,
}

impl PromptMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: PromptRole::System,
            content: PromptContent::Text(content.into()),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: PromptRole::User,
            content: PromptContent::Text(content.into()),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: PromptRole::Assistant,
            content: PromptContent::Text(content.into()),
        }
    }

    pub fn user_parts(parts: Vec<ContentPart>) -> Self {
        Self {
            role: PromptRole::User,
            content: PromptContent::Parts(parts),
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
}

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub messages: Vec<PromptMessage>,
    pub max_tokens: u32,
    pub response_schema: Option<JsonResponseSchema>,
    pub web_search: bool,
}

impl ModelRequest {
    pub fn text(messages: Vec<PromptMessage>, max_tokens: u32) -> Self {
        Self {
            messages,
            max_tokens,
            response_schema: None,
            web_search: false,
        }
    }

    pub fn text_with_web_search(messages: Vec<PromptMessage>, max_tokens: u32) -> Self {
        Self {
            messages,
            max_tokens,
            response_schema: None,
            web_search: true,
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
            tools: request.web_search.then(|| {
                vec![json!({
                    "type": "openrouter:web_search",
                    "parameters": {
                        "max_results": 5,
                        "max_total_results": 8,
                        "search_context_size": "low"
                    }
                })]
            }),
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
        let message = completion
            .choices
            .into_iter()
            .next()
            .map(|choice| choice.message)
            .context("OpenRouter returned no choices")?;
        let mut content = message
            .content
            .filter(|content| !content.trim().is_empty())
            .context("OpenRouter returned no message content")?;
        if request.web_search {
            append_reference_links(&mut content, &message.annotations);
        }
        Ok(content)
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
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Value>>,
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
