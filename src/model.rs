use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Feedback,
    Project,
    Reference,
}

impl MemoryKind {
    pub const ALL: [Self; 3] = [Self::Feedback, Self::Project, Self::Reference];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Feedback => "feedback",
            Self::Project => "project",
            Self::Reference => "reference",
        }
    }
}

impl fmt::Display for MemoryKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for MemoryKind {
    type Err = ParseMemoryKindError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "feedback" => Ok(Self::Feedback),
            "project" => Ok(Self::Project),
            "reference" => Ok(Self::Reference),
            _ => Err(ParseMemoryKindError(value.to_owned())),
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("unknown memory kind: {0}")]
pub struct ParseMemoryKindError(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    Candidate,
    Active,
    Deleted,
}

impl MemoryStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Active => "active",
            Self::Deleted => "deleted",
        }
    }
}

impl FromStr for MemoryStatus {
    type Err = ParseMemoryStatusError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "candidate" => Ok(Self::Candidate),
            "active" => Ok(Self::Active),
            "deleted" => Ok(Self::Deleted),
            _ => Err(ParseMemoryStatusError(value.to_owned())),
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("unknown memory status: {0}")]
pub struct ParseMemoryStatusError(String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GuildMemory {
    pub id: i64,
    pub guild_id: u64,
    pub kind: MemoryKind,
    pub title: String,
    pub summary: String,
    pub content: String,
    pub status: MemoryStatus,
    pub created_by: u64,
    pub source_channel_id: Option<u64>,
    pub source_message_id: Option<u64>,
    pub created_at: String,
    pub updated_at: String,
    pub reviewed_at: Option<String>,
    pub review_after: Option<String>,
    pub stale: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConversationSearchResult {
    pub message_id: u64,
    pub channel_id: u64,
    pub channel_name: Option<String>,
    pub is_current_channel: bool,
    pub message_url: String,
    pub author_id: u64,
    pub author_name: String,
    pub content: String,
    pub attachment_text: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelMode {
    #[default]
    Off,
    ReadOnly,
    Mention,
    Auto,
}

impl ChannelMode {
    pub const ALL: [Self; 4] = [Self::Off, Self::ReadOnly, Self::Mention, Self::Auto];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::ReadOnly => "read_only",
            Self::Mention => "mention",
            Self::Auto => "auto",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Off => "AIはメッセージを保存せず、返信もしません",
            Self::ReadOnly => "会話をサーバー共通コンテキストへ保存しますが、返信しません",
            Self::Mention => {
                "会話を保存し、Botへのメンションには原則返信します（不要時は見送ります）"
            }
            Self::Auto => "会話を保存し、必要性をAIが判断して返信します",
        }
    }

    pub const fn stores_messages(self) -> bool {
        !matches!(self, Self::Off)
    }
}

impl fmt::Display for ChannelMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ChannelMode {
    type Err = ParseChannelModeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "read_only" => Ok(Self::ReadOnly),
            "mention" => Ok(Self::Mention),
            "auto" => Ok(Self::Auto),
            _ => Err(ParseChannelModeError(value.to_owned())),
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("unknown channel mode: {0}")]
pub struct ParseChannelModeError(String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelSetting {
    pub guild_id: u64,
    pub channel_id: u64,
    pub mode: ChannelMode,
    pub updated_by: u64,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferencedMessage {
    pub message_id: u64,
    pub channel_id: u64,
    pub author_id: u64,
    pub author_name: String,
    pub content: String,
    pub is_bot: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingAttachment {
    pub filename: String,
    pub content_type: Option<String>,
    pub size_bytes: u64,
    pub extracted_text: Option<String>,
    pub binary_data: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingMessage {
    pub discord_message_id: u64,
    pub guild_id: u64,
    pub channel_id: u64,
    pub channel_name: Option<String>,
    pub settings_channel_id: u64,
    pub author_id: u64,
    pub author_name: String,
    pub content: String,
    pub is_bot: bool,
    pub created_at: String,
    pub referenced_message_id: Option<u64>,
    pub referenced_message: Option<ReferencedMessage>,
    pub attachments: Vec<IncomingAttachment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecentChannelActivity {
    pub channel_id: u64,
    pub channel_name: Option<String>,
    pub is_current_channel: bool,
    pub last_message_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextMessage {
    pub message_id: u64,
    pub channel_id: u64,
    pub author_id: u64,
    pub author_name: String,
    pub content: String,
    pub is_bot: bool,
    pub created_at: String,
    pub reply_to: Option<ReferencedMessage>,
    pub linked_messages: Vec<ReferencedMessage>,
    pub attachments: Vec<ContextAttachment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextAttachment {
    pub filename: String,
    pub content_type: Option<String>,
    pub extracted_text: Option<String>,
    pub binary_data: Option<Vec<u8>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_modes_round_trip() {
        for mode in ChannelMode::ALL {
            assert_eq!(mode.as_str().parse(), Ok(mode));
        }
    }

    #[test]
    fn memory_kinds_round_trip() {
        for kind in MemoryKind::ALL {
            assert_eq!(kind.as_str().parse(), Ok(kind));
        }
    }
}
