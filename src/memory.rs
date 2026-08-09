use std::collections::HashSet;

use crate::model::{ContextMessage, GuildMemory, MemoryKind};

const DEFAULT_DETAIL_LIMIT: usize = 8;

pub fn select_relevant_memories<'a>(
    memories: &'a [GuildMemory],
    context: &[ContextMessage],
) -> Vec<&'a GuildMemory> {
    let query = context
        .iter()
        .rev()
        .take(10)
        .flat_map(|message| {
            std::iter::once(message.content.as_str()).chain(
                message
                    .attachments
                    .iter()
                    .filter_map(|attachment| attachment.extracted_text.as_deref()),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let query_terms = ngrams(&query);

    let mut selected: Vec<(&GuildMemory, usize)> = memories
        .iter()
        .filter_map(|memory| {
            let always_include = memory.kind == MemoryKind::Feedback;
            let searchable = format!("{}\n{}", memory.title, memory.summary);
            let memory_terms = ngrams(&searchable);
            let score = query_terms.intersection(&memory_terms).count();
            (always_include || score > 0).then_some((memory, score))
        })
        .collect();
    selected.sort_by(|(left, left_score), (right, right_score)| {
        let left_feedback = left.kind == MemoryKind::Feedback;
        let right_feedback = right.kind == MemoryKind::Feedback;
        right_feedback
            .cmp(&left_feedback)
            .then_with(|| right_score.cmp(left_score))
            .then_with(|| right.updated_at.cmp(&left.updated_at))
    });
    selected
        .into_iter()
        .take(DEFAULT_DETAIL_LIMIT)
        .map(|(memory, _)| memory)
        .collect()
}

fn ngrams(text: &str) -> HashSet<String> {
    let normalized: Vec<char> = text
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|character| character.is_alphanumeric())
        .collect();
    if normalized.len() < 2 {
        return normalized
            .into_iter()
            .map(|character| character.to_string())
            .collect();
    }
    normalized
        .windows(2)
        .map(|window| window.iter().collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ContextAttachment, MemoryStatus};

    fn memory(id: i64, kind: MemoryKind, title: &str, summary: &str) -> GuildMemory {
        GuildMemory {
            id,
            guild_id: 1,
            kind,
            title: title.into(),
            summary: summary.into(),
            content: format!("{title} details"),
            status: MemoryStatus::Active,
            created_by: 2,
            source_channel_id: Some(3),
            source_message_id: Some(4),
            created_at: "2026-08-09".into(),
            updated_at: format!("2026-08-0{id}"),
            reviewed_at: Some("2026-08-09".into()),
            review_after: Some("2026-09-08".into()),
            stale: false,
        }
    }

    #[test]
    fn selects_feedback_and_topic_matches() {
        let memories = vec![
            memory(1, MemoryKind::Project, "Rustの設計", "Resultで失敗を表す"),
            memory(2, MemoryKind::Reference, "Python資料", "型ヒントの資料"),
            memory(3, MemoryKind::Feedback, "回答方針", "結論を先にする"),
        ];
        let context = vec![ContextMessage {
            channel_id: 1,
            author_id: 2,
            author_name: "user".into(),
            content: "RustのResultについて".into(),
            is_bot: false,
            created_at: "2026-08-09".into(),
            attachments: vec![ContextAttachment {
                filename: "none".into(),
                content_type: None,
                extracted_text: None,
                binary_data: None,
            }],
        }];

        let selected = select_relevant_memories(&memories, &context);
        assert_eq!(selected[0].id, 3);
        assert!(selected.iter().any(|memory| memory.id == 1));
        assert!(!selected.iter().any(|memory| memory.id == 2));
    }
}
