use std::{env, num::NonZeroU64};

use thiserror::Error;

pub const DEFAULT_OPENROUTER_MODEL: &str = "openai/gpt-5.6-luna";
pub const DEFAULT_OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";

#[derive(Clone)]
pub struct Config {
    pub discord_token: String,
    pub discord_guild_id: NonZeroU64,
    pub openrouter_api_key: String,
    pub openrouter_model: String,
    pub openrouter_base_url: String,
    pub database_url: String,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let discord_guild_id = required("DISCORD_GUILD_ID")?
            .parse::<u64>()
            .map_err(|_| ConfigError::InvalidGuildId)?;

        Ok(Self {
            discord_token: required("DISCORD_TOKEN")?,
            discord_guild_id: NonZeroU64::new(discord_guild_id)
                .ok_or(ConfigError::InvalidGuildId)?,
            openrouter_api_key: required("OPENROUTER_API_KEY")?,
            openrouter_model: optional("OPENROUTER_MODEL")
                .unwrap_or_else(|| DEFAULT_OPENROUTER_MODEL.to_owned()),
            openrouter_base_url: optional("OPENROUTER_BASE_URL")
                .unwrap_or_else(|| DEFAULT_OPENROUTER_BASE_URL.to_owned()),
            database_url: required("DATABASE_URL")?,
        })
    }
}

fn required(name: &'static str) -> Result<String, ConfigError> {
    optional(name).ok_or(ConfigError::Missing(name))
}

fn optional(name: &'static str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("required environment variable {0} is missing or empty")]
    Missing(&'static str),
    #[error("DISCORD_GUILD_ID must be a non-zero integer")]
    InvalidGuildId,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_documented_openrouter_settings() {
        assert_eq!(DEFAULT_OPENROUTER_MODEL, "openai/gpt-5.6-luna");
        assert_eq!(DEFAULT_OPENROUTER_BASE_URL, "https://openrouter.ai/api/v1");
    }
}

