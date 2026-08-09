use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use thiserror::Error;

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
            Self::Mention => "会話を保存し、Botへのメンション時だけ返信します",
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
    pub author_id: u64,
    pub author_name: String,
    pub content: String,
    pub is_bot: bool,
    pub created_at: String,
    pub attachments: Vec<IncomingAttachment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextMessage {
    pub channel_id: u64,
    pub author_id: u64,
    pub author_name: String,
    pub content: String,
    pub is_bot: bool,
    pub created_at: String,
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
}
