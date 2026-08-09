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
    pub code_runner_enabled: bool,
    pub lima_instance: String,
    pub lima_template: String,
    pub code_execution_timeout_secs: u64,
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
            code_runner_enabled: optional("CODE_RUNNER_ENABLED")
                .map(|value| parse_bool("CODE_RUNNER_ENABLED", &value))
                .transpose()?
                .unwrap_or(false),
            lima_instance: optional("LIMA_INSTANCE")
                .unwrap_or_else(|| "discord-code-runner".to_owned()),
            lima_template: optional("LIMA_TEMPLATE")
                .unwrap_or_else(|| "src/runner/lima.yaml".to_owned()),
            code_execution_timeout_secs: optional("CODE_EXECUTION_TIMEOUT_SECS")
                .map(|value| {
                    value
                        .parse::<u64>()
                        .map_err(|_| ConfigError::InvalidInteger("CODE_EXECUTION_TIMEOUT_SECS"))
                })
                .transpose()?
                .unwrap_or(20)
                .clamp(1, 120),
        })
    }
}

fn parse_bool(name: &'static str, value: &str) -> Result<bool, ConfigError> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(ConfigError::InvalidBoolean(name)),
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
    #[error("{0} must be a boolean")]
    InvalidBoolean(&'static str),
    #[error("{0} must be an integer")]
    InvalidInteger(&'static str),
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
