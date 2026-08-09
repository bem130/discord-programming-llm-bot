use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;

use crate::{
    ai::{LanguageModel, ModelRequest, PromptMessage, complete_json},
    context::{context_data_message, conversation_prompt},
    model::{ChannelMode, ContextMessage},
    review::review_response,
};

const REPLY_DECISION_PROMPT: &str = include_str!("../prompt/reply_decision.md");
const ANSWER_MAX_TOKENS: u32 = 8_000;

pub async fn should_reply(
    model: &dyn LanguageModel,
    mode: ChannelMode,
    mentioned: bool,
    context: &[ContextMessage],
) -> Result<bool> {
    match mode {
        ChannelMode::Off | ChannelMode::ReadOnly => Ok(false),
        ChannelMode::Mention => Ok(mentioned),
        ChannelMode::Auto if mentioned => Ok(true),
        ChannelMode::Auto => {
            let decision: ReplyDecision = complete_json(
                model,
                ModelRequest::json(
                    vec![
                        PromptMessage::system(REPLY_DECISION_PROMPT),
                        context_data_message(context),
                    ],
                    300,
                    "reply_decision",
                    json!({
                        "type": "object",
                        "properties": {
                            "should_reply": { "type": "boolean" },
                            "reason": { "type": "string" }
                        },
                        "required": ["should_reply", "reason"],
                        "additionalProperties": false
                    }),
                ),
            )
            .await
            .context("automatic reply decision failed")?;
            tracing::debug!(
                should_reply = decision.should_reply,
                reason = %decision.reason,
                "automatic reply decision completed"
            );
            Ok(decision.should_reply)
        }
    }
}

pub async fn generate_reviewed_answer(
    model: &dyn LanguageModel,
    context: &[ContextMessage],
) -> Result<String> {
    let messages = conversation_prompt(context);
    let generated = model
        .complete(ModelRequest::text(messages.clone(), ANSWER_MAX_TOKENS))
        .await
        .context("answer generation failed")?;
    review_response(model, &messages, generated)
        .await
        .context("generated answer did not pass review")
}

#[derive(Debug, Deserialize)]
struct ReplyDecision {
    should_reply: bool,
    reason: String,
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use anyhow::anyhow;
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
                .ok_or_else(|| anyhow!("unexpected model call"))
        }
    }

    fn empty_model() -> StubModel {
        StubModel {
            responses: Mutex::new(VecDeque::new()),
        }
    }

    #[tokio::test]
    async fn mention_always_replies_in_auto_mode_without_model_call() {
        assert!(
            should_reply(&empty_model(), ChannelMode::Auto, true, &[])
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn read_only_never_replies() {
        assert!(
            !should_reply(&empty_model(), ChannelMode::ReadOnly, true, &[])
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn auto_mode_uses_structured_decision() {
        let model = StubModel {
            responses: Mutex::new(VecDeque::from([
                r#"{"should_reply":true,"reason":"question"}"#.into(),
            ])),
        };
        assert!(
            should_reply(&model, ChannelMode::Auto, false, &[])
                .await
                .unwrap()
        );
    }
}
