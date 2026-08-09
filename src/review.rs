use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::json;

use crate::ai::{
    AiSessionPurpose, LanguageModel, ModelRequest, PromptMessage, complete_json_with_retry,
    complete_with_retry,
};

const REVIEW_PROMPT: &str = include_str!("../prompt/review.md");
const MAX_REPAIR_ATTEMPTS: usize = 2;

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
    let mut candidate = generated;

    for attempt in 0..=MAX_REPAIR_ATTEMPTS {
        let issues = static_issues(&candidate);
        if !issues.is_empty() {
            if attempt == MAX_REPAIR_ATTEMPTS {
                bail!("response failed static review after repair attempts");
            }
            let feedback = issues
                .iter()
                .map(StaticIssue::instruction)
                .collect::<Vec<_>>()
                .join("; ");
            candidate = repair(model, guild_id, original_messages, &candidate, &feedback).await?;
            continue;
        }

        let verdict = semantic_review(model, guild_id, original_messages, &candidate).await?;
        if verdict.approved {
            return Ok(candidate);
        }
        if attempt == MAX_REPAIR_ATTEMPTS {
            bail!("response failed semantic review after repair attempts");
        }
        candidate = repair(
            model,
            guild_id,
            original_messages,
            &candidate,
            &verdict.feedback,
        )
        .await?;
    }

    unreachable!("review loop always returns or fails")
}

async fn semantic_review(
    model: &dyn LanguageModel,
    guild_id: u64,
    original_messages: &[PromptMessage],
    candidate: &str,
) -> Result<ReviewVerdict> {
    let mut messages = vec![PromptMessage::system(REVIEW_PROMPT)];
    messages.extend_from_slice(original_messages);
    messages.push(PromptMessage::user(format!(
        "上記が元の会話です。候補回答を審査してください。\n<candidate>\n{candidate}\n</candidate>"
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
                    "feedback": { "type": "string" }
                },
                "required": ["approved", "feedback"],
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
    original_messages: &[PromptMessage],
    candidate: &str,
    feedback: &str,
) -> Result<String> {
    let mut messages = vec![PromptMessage::system(
        "候補回答を修正する編集者です。修正済み回答だけを返してください。",
    )];
    messages.extend_from_slice(original_messages);
    messages.push(PromptMessage::user(format!(
        "上記が元の会話です。\n\n候補回答:\n<candidate>\n{candidate}\n</candidate>\n\n修正指示:\n{feedback}"
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
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use async_trait::async_trait;

    use super::*;

    struct StubModel {
        responses: Mutex<VecDeque<String>>,
    }

    #[async_trait]
    impl LanguageModel for StubModel {
        async fn complete(&self, _request: ModelRequest) -> Result<String> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .context("missing stub response")
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

    #[tokio::test]
    async fn repairs_static_issue_before_semantic_approval() {
        let model = StubModel {
            responses: Mutex::new(VecDeque::from([
                "- A: 1\n- B: 2".to_owned(),
                r#"{"approved":true,"feedback":"ok"}"#.to_owned(),
            ])),
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
}
