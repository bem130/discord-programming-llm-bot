pub const DISCORD_MESSAGE_LIMIT: usize = 2_000;
pub const SAFE_MESSAGE_CHUNK_UTF16_UNITS: usize = 1_800;
pub const SAFE_ATTACHMENT_PART_SIZE: usize = 24 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentPart {
    pub filename: String,
    pub bytes: Vec<u8>,
}

pub fn split_message(content: &str) -> Vec<String> {
    if content.is_empty() {
        return vec![];
    }

    let raw_chunks = split_raw(content, SAFE_MESSAGE_CHUNK_UTF16_UNITS);
    let mut code_fence: Option<String> = None;
    raw_chunks
        .into_iter()
        .map(|chunk| {
            let opening_fence = code_fence.clone();
            update_code_fence(&chunk, &mut code_fence);

            let mut decorated = String::new();
            if let Some(opening_fence) = opening_fence {
                decorated.push_str(&opening_fence);
                decorated.push('\n');
            }
            decorated.push_str(&chunk);
            if code_fence.is_some() {
                if !decorated.ends_with('\n') {
                    decorated.push('\n');
                }
                decorated.push_str("```");
            }
            decorated
        })
        .collect()
}

pub fn split_attachment(filename: &str, bytes: &[u8]) -> Vec<AttachmentPart> {
    if bytes.len() <= SAFE_ATTACHMENT_PART_SIZE {
        return vec![AttachmentPart {
            filename: filename.to_owned(),
            bytes: bytes.to_vec(),
        }];
    }

    let total = bytes.len().div_ceil(SAFE_ATTACHMENT_PART_SIZE);
    bytes
        .chunks(SAFE_ATTACHMENT_PART_SIZE)
        .enumerate()
        .map(|(index, part)| AttachmentPart {
            filename: numbered_filename(filename, index + 1, total),
            bytes: part.to_vec(),
        })
        .collect()
}

fn split_raw(content: &str, max_utf16_units: usize) -> Vec<String> {
    let characters: Vec<char> = content.chars().collect();
    let mut chunks = Vec::new();
    let mut start = 0;

    while start < characters.len() {
        let mut hard_end = start;
        let mut utf16_units = 0;
        while hard_end < characters.len() {
            let character_units = characters[hard_end].len_utf16();
            if utf16_units + character_units > max_utf16_units {
                break;
            }
            utf16_units += character_units;
            hard_end += 1;
        }
        let end = if hard_end == characters.len() {
            hard_end
        } else {
            (start..hard_end)
                .rev()
                .find(|index| matches!(characters[*index], '\n' | ' '))
                .filter(|index| *index > start)
                .map_or(hard_end, |index| index + 1)
        };
        chunks.push(characters[start..end].iter().collect());
        start = end;
    }

    chunks
}

fn update_code_fence(chunk: &str, code_fence: &mut Option<String>) {
    for line in chunk.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            if code_fence.is_some() {
                *code_fence = None;
            } else {
                *code_fence = Some(
                    trimmed
                        .split_whitespace()
                        .next()
                        .unwrap_or("```")
                        .chars()
                        .take(64)
                        .collect::<String>(),
                );
            }
        }
    }
}

fn numbered_filename(filename: &str, part: usize, total: usize) -> String {
    match filename.rsplit_once('.') {
        Some((stem, extension)) => format!("{stem}.part-{part}-of-{total}.{extension}"),
        None => format!("{filename}.part-{part}-of-{total}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_message_is_unchanged() {
        assert_eq!(split_message("hello"), vec!["hello"]);
    }

    #[test]
    fn long_unicode_message_stays_within_discord_limit() {
        let content = "🦀".repeat(4_001);
        let chunks = split_message(&content);
        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.encode_utf16().count() <= DISCORD_MESSAGE_LIMIT)
        );
        assert_eq!(chunks.concat(), content);
    }

    #[test]
    fn supplementary_unicode_is_counted_conservatively() {
        let content = "🦀".repeat(2_000);
        let chunks = split_message(&content);
        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.encode_utf16().count() <= DISCORD_MESSAGE_LIMIT)
        );
        assert_eq!(chunks.concat(), content);
    }

    #[test]
    fn code_fences_are_closed_and_reopened() {
        let content = format!("```rust\n{}\n```", "let x = 1;\n".repeat(300));
        let chunks = split_message(&content);
        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.matches("```").count().is_multiple_of(2))
        );
    }

    #[test]
    fn large_attachment_gets_numbered_parts() {
        let bytes = vec![7; SAFE_ATTACHMENT_PART_SIZE + 1];
        let parts = split_attachment("answer.txt", &bytes);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].filename, "answer.part-1-of-2.txt");
        assert_eq!(parts[1].filename, "answer.part-2-of-2.txt");
        assert_eq!(
            parts
                .into_iter()
                .flat_map(|part| part.bytes)
                .collect::<Vec<_>>(),
            bytes
        );
    }
}
