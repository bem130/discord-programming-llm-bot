use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use crate::ai::{
    AiSessionPurpose, LanguageModel, ModelRequest, PromptContent, PromptMessage, PromptRole,
    complete_json_with_retry, complete_with_retry,
};

const REVIEW_PROMPT: &str = include_str!("../prompt/review.md");
const MAX_REPAIR_ATTEMPTS: usize = 2;
const MAX_TOOL_RESULT_CHARS: usize = 2_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaticIssue {
    MarkdownTable,
    Latex,
    UnbalancedCodeFence,
    BroadcastMention,
}

impl StaticIssue {
    fn instruction(&self) -> &'static str {
        match self {
            Self::MarkdownTable => "Markdown表を箇条書きへ変更する",
            Self::Latex => "LaTeX数式をプレーンテキストまたはコードブロックへ変更する",
            Self::UnbalancedCodeFence => "コードフェンスを正しく閉じる",
            Self::BroadcastMention => "@everyone または @here を通常の文言へ変更する",
        }
    }
}

pub fn static_issues(candidate: &str) -> Vec<StaticIssue> {
    let mut issues = Vec::new();
    if candidate.lines().any(looks_like_table_separator) {
        issues.push(StaticIssue::MarkdownTable);
    }
    if contains_latex(candidate) {
        issues.push(StaticIssue::Latex);
    }
    if !candidate.matches("```").count().is_multiple_of(2) {
        issues.push(StaticIssue::UnbalancedCodeFence);
    }
    if candidate.contains("@everyone") || candidate.contains("@here") {
        issues.push(StaticIssue::BroadcastMention);
    }
    issues
}

pub async fn review_response(
    model: &dyn LanguageModel,
    guild_id: u64,
    original_messages: &[PromptMessage],
    generated: String,
) -> Result<String> {
    let context = review_context(original_messages);
    let mut candidate = generated;
    let mut feedback_history: Vec<String> = Vec::new();
    let mut best_candidate: Option<(u8, String)> = None;

    for attempt in 0..=MAX_REPAIR_ATTEMPTS {
        let issues = static_issues(&candidate);
        if !issues.is_empty() {
            if attempt == MAX_REPAIR_ATTEMPTS {
                warn!(
                    issues = ?issues,
                    "static review still failing; delivering best effort candidate"
                );
                return Ok(sanitize_fallback(&candidate));
            }
            let feedback = issues
                .iter()
                .map(StaticIssue::instruction)
                .collect::<Vec<_>>()
                .join("; ");
            feedback_history.push(feedback.clone());
            candidate = repair(model, guild_id, &context, &candidate, &feedback_history).await?;
            continue;
        }

        let verdict = semantic_review(model, guild_id, &context, &candidate).await?;
        if best_candidate
            .as_ref()
            .is_none_or(|(best_score, _)| verdict.score >= *best_score)
        {
            best_candidate = Some((verdict.score, candidate.clone()));
        }
        if verdict.approved {
            return Ok(candidate);
        }
        warn!(
            attempt,
            score = verdict.score,
            feedback = %verdict.feedback,
            "semantic review rejected candidate"
        );
        if attempt == MAX_REPAIR_ATTEMPTS {
            let (score, best) = best_candidate.unwrap_or((0, candidate.clone()));
            warn!(
                score,
                "semantic review exhausted; delivering best scored candidate"
            );
            return Ok(best);
        }
        feedback_history.push(verdict.feedback);
        candidate = repair(model, guild_id, &context, &candidate, &feedback_history).await?;
    }

    unreachable!("review loop always returns or fails")
}

fn sanitize_fallback(candidate: &str) -> String {
    let mut result = candidate
        .replace("@everyone", "皆さん")
        .replace("@here", "皆さん");
    if !result.matches("```").count().is_multiple_of(2) {
        result.push_str("\n```");
    }
    result
}

fn review_context(messages: &[PromptMessage]) -> Vec<PromptMessage> {
    messages
        .iter()
        .map(|message| match message.role {
            PromptRole::Tool => truncate_tool_result(message),
            _ => message.clone(),
        })
        .collect()
}

fn truncate_tool_result(message: &PromptMessage) -> PromptMessage {
    let mut message = message.clone();
    if let PromptContent::Text(text) = &mut message.content
        && text.chars().count() > MAX_TOOL_RESULT_CHARS
    {
        let truncated: String = text.chars().take(MAX_TOOL_RESULT_CHARS).collect();
        *text = format!("{truncated}\n\n[ツール結果は長いため省略されました]");
    }
    message
}

async fn semantic_review(
    model: &dyn LanguageModel,
    guild_id: u64,
    context: &[PromptMessage],
    candidate: &str,
) -> Result<ReviewVerdict> {
    let mut messages = vec![PromptMessage::system(REVIEW_PROMPT)];
    messages.extend_from_slice(context);
    messages.push(PromptMessage::user(format!(
        "上記が元の会話とツールの取得結果です(長いツール結果は省略されています)。候補回答はツールで取得した履歴に基づいて生成されています。候補回答を審査してください。\n<candidate>\n{candidate}\n</candidate>"
    )));

    complete_json_with_retry(
        model,
        ModelRequest::json(
            messages,
            1_200,
            "response_review",
            json!({
                "type": "object",
                "properties": {
                    "approved": { "type": "boolean" },
                    "feedback": { "type": "string" },
                    "score": { "type": "integer", "minimum": 1, "maximum": 10 }
                },
                "required": ["approved", "feedback", "score"],
                "additionalProperties": false
            }),
        )
        .with_reasoning_effort("none")
        .with_guild_session(guild_id, AiSessionPurpose::ResponseReview),
        "semantic_review",
    )
    .await
    .context("AI response review failed")
}

async fn repair(
    model: &dyn LanguageModel,
    guild_id: u64,
    context: &[PromptMessage],
    candidate: &str,
    feedback_history: &[String],
) -> Result<String> {
    let mut messages = vec![PromptMessage::system(
        "あなたは候補回答を修正する編集者です。\n\
         修正指示のすべてが解消されるよう、候補回答を最小限の変更で修正してください。\n\
         修正指示に含まれない部分は変更せず、既に正しい内容は維持してください。\n\
         修正済みの回答本文だけを返してください。",
    )];
    messages.extend_from_slice(context);
    let instructions: String = feedback_history
        .iter()
        .enumerate()
        .map(|(index, feedback)| format!("{}. {feedback}", index + 1))
        .collect::<Vec<_>>()
        .join("\n");
    messages.push(PromptMessage::user(format!(
        "上記が元の会話とツールの取得結果です。\n\n候補回答:\n<candidate>\n{candidate}\n</candidate>\n\n修正指示(すべて対応すること):\n{instructions}"
    )));
    complete_with_retry(
        model,
        ModelRequest::text(messages, 4_000)
            .with_guild_session(guild_id, AiSessionPurpose::ResponseReview),
        "response_repair",
    )
    .await
    .context("AI response repair failed")
}

fn looks_like_table_separator(line: &str) -> bool {
    let trimmed = line.trim().trim_matches('|');
    let cells: Vec<_> = trimmed.split('|').map(str::trim).collect();
    cells.len() >= 2
        && cells.iter().all(|cell| {
            cell.chars().filter(|character| *character == '-').count() >= 3
                && cell
                    .chars()
                    .all(|character| matches!(character, '-' | ':' | ' '))
        })
}

fn contains_latex(candidate: &str) -> bool {
    candidate.contains("$$")
        || candidate.contains("\\(")
        || candidate.contains("\\)")
        || candidate.contains("\\[")
        || candidate.contains("\\]")
        || candidate.contains("\\begin{")
        || candidate.contains("\\frac{")
}

#[derive(Debug, Deserialize)]
struct ReviewVerdict {
    approved: bool,
    feedback: String,
    #[serde(deserialize_with = "deserialize_review_score")]
    score: u8,
}

fn deserialize_review_score<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let score = u8::deserialize(deserializer)?;
    if (1..=10).contains(&score) {
        Ok(score)
    } else {
        Err(serde::de::Error::custom(
            "review score must be between 1 and 10",
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use async_trait::async_trait;

    use super::*;
    use crate::ai::{ToolCall, ToolFunction};

    struct StubModel {
        responses: Mutex<VecDeque<String>>,
        requests: Mutex<Vec<ModelRequest>>,
    }

    #[async_trait]
    impl LanguageModel for StubModel {
        async fn complete(&self, request: ModelRequest) -> Result<String> {
            self.requests.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .context("missing stub response")
        }
    }

    fn user_text(request: &ModelRequest) -> String {
        request
            .messages
            .iter()
            .rev()
            .find(|message| matches!(message.role, PromptRole::User))
            .and_then(content_text)
            .unwrap_or_default()
    }

    fn content_text(message: &PromptMessage) -> Option<String> {
        match &message.content {
            PromptContent::Text(text) => Some(text.clone()),
            PromptContent::Parts(_) => None,
        }
    }

    #[test]
    fn finds_discord_incompatible_markdown() {
        let response = "| A | B |\n| --- | :---: |\n| 1 | 2 |\n\n$$x^2$$\n@everyone";
        assert_eq!(
            static_issues(response),
            vec![
                StaticIssue::MarkdownTable,
                StaticIssue::Latex,
                StaticIssue::BroadcastMention
            ]
        );
    }

    #[test]
    fn accepts_pipes_in_code_and_plain_math() {
        let response = "```rust\nlet value = left | right;\n```\n\nx^2 + y^2";
        assert!(static_issues(response).is_empty());
    }

    #[test]
    fn review_context_keeps_conversation_and_truncates_tool_results() {
        let long_result = "x".repeat(5_000);
        let messages = vec![
            PromptMessage::system("system"),
            PromptMessage::user("user 1"),
            PromptMessage::assistant("assistant text"),
            PromptMessage::assistant_tool_calls(
                None,
                vec![ToolCall {
                    id: "call_1".into(),
                    kind: "function".into(),
                    function: ToolFunction {
                        name: "get_messages_newer_than_datetime".into(),
                        arguments: "{}".into(),
                    },
                }],
            ),
            PromptMessage::tool_result("call_1", long_result),
            PromptMessage::user("user 2"),
        ];

        let context = review_context(&messages);

        assert_eq!(context.len(), 6);
        assert!(matches!(context[0].role, PromptRole::System));
        assert!(matches!(context[1].role, PromptRole::User));
        assert_eq!(content_text(&context[2]), Some("assistant text".to_owned()),);
        assert!(matches!(context[3].role, PromptRole::Assistant));
        assert!(context[3].tool_calls.is_some());
        assert!(matches!(context[4].role, PromptRole::Tool));
        let truncated = content_text(&context[4]).unwrap();
        assert!(truncated.chars().count() <= MAX_TOOL_RESULT_CHARS + 64);
        assert!(truncated.contains("[ツール結果は長いため省略されました]"));
        assert!(matches!(context[5].role, PromptRole::User));
    }

    #[tokio::test]
    async fn repairs_static_issue_before_semantic_approval() {
        let model = StubModel {
            responses: Mutex::new(VecDeque::from([
                "- A: 1\n- B: 2".to_owned(),
                r#"{"approved":true,"feedback":"ok","score":8}"#.to_owned(),
            ])),
            requests: Mutex::new(Vec::new()),
        };

        let result = review_response(
            &model,
            123,
            &[PromptMessage::user("compare")],
            "| A | B |\n| --- | --- |".to_owned(),
        )
        .await
        .unwrap();

        assert_eq!(result, "- A: 1\n- B: 2");
    }

    #[tokio::test]
    async fn repair_accumulates_feedback_across_rejections() {
        let model = StubModel {
            responses: Mutex::new(VecDeque::from([
                r#"{"approved":false,"feedback":"feedback A","score":4}"#.to_owned(),
                "candidate B".to_owned(),
                r#"{"approved":false,"feedback":"feedback B","score":5}"#.to_owned(),
                "candidate C".to_owned(),
                r#"{"approved":true,"feedback":"ok","score":8}"#.to_owned(),
            ])),
            requests: Mutex::new(Vec::new()),
        };

        let result = review_response(
            &model,
            123,
            &[PromptMessage::user("summarize")],
            "candidate A".to_owned(),
        )
        .await
        .unwrap();

        assert_eq!(result, "candidate C");
        let repair_prompts: Vec<String> = model
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(user_text)
            .filter(|text| text.contains("修正指示"))
            .collect();
        assert_eq!(repair_prompts.len(), 2);
        assert!(repair_prompts[0].contains("1. feedback A"));
        assert!(repair_prompts[1].contains("1. feedback A"));
        assert!(repair_prompts[1].contains("2. feedback B"));
    }

    #[tokio::test]
    async fn delivers_best_scored_candidate_when_semantic_review_exhausted() {
        let model = StubModel {
            responses: Mutex::new(VecDeque::from([
                r#"{"approved":false,"feedback":"fb1","score":6}"#.to_owned(),
                "candidate B".to_owned(),
                r#"{"approved":false,"feedback":"fb2","score":4}"#.to_owned(),
                "candidate C".to_owned(),
                r#"{"approved":false,"feedback":"fb3","score":5}"#.to_owned(),
            ])),
            requests: Mutex::new(Vec::new()),
        };

        let result = review_response(
            &model,
            123,
            &[PromptMessage::user("summarize")],
            "candidate A".to_owned(),
        )
        .await
        .unwrap();

        assert_eq!(result, "candidate A");
    }

    #[tokio::test]
    async fn retries_review_when_score_is_out_of_range() {
        let model = StubModel {
            responses: Mutex::new(VecDeque::from([
                r#"{"approved":false,"feedback":"invalid","score":11}"#.to_owned(),
                r#"{"approved":true,"feedback":"ok","score":8}"#.to_owned(),
            ])),
            requests: Mutex::new(Vec::new()),
        };

        let result = review_response(
            &model,
            123,
            &[PromptMessage::user("help")],
            "candidate".to_owned(),
        )
        .await
        .unwrap();

        assert_eq!(result, "candidate");
        assert_eq!(model.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn delivers_sanitized_candidate_when_static_issues_persist() {
        let model = StubModel {
            responses: Mutex::new(VecDeque::from([
                "```rust\nlet x = 1;\n@everyone".to_owned(),
                "```rust\nlet x = 1;\n@everyone".to_owned(),
            ])),
            requests: Mutex::new(Vec::new()),
        };

        let result = review_response(
            &model,
            123,
            &[PromptMessage::user("help")],
            "```rust\nlet x = 1;\n@everyone".to_owned(),
        )
        .await
        .unwrap();

        assert_eq!(result, "```rust\nlet x = 1;\n皆さん\n```");
        assert!(!result.contains("@everyone"));
        assert!(result.matches("```").count().is_multiple_of(2));
    }

    #[test]
    fn sanitize_fallback_removes_mentions_and_closes_fences() {
        let result = sanitize_fallback("hi @everyone and ```rust\nlet x = 1;");
        assert!(!result.contains("@everyone"));
        assert!(result.contains("皆さん"));
        assert!(result.matches("```").count().is_multiple_of(2));
    }

    #[test]
    fn sanitize_fallback_leaves_valid_candidate_unchanged() {
        let candidate = "plain text\n```\ncode\n```";
        assert_eq!(sanitize_fallback(candidate), candidate);
    }
}
