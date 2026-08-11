use std::{env, num::NonZeroU64};

use thiserror::Error;

pub const DEFAULT_OPENROUTER_MODEL: &str = "openai/gpt-5.6-luna";
pub const DEFAULT_OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const DEFAULT_REPLY_DECISION_MODELS: &str = "google/gemma-4-26b-a4b-it:free,nvidia/nemotron-3-super-120b-a12b:free,openrouter/free,openai/gpt-5.6-luna";
pub const DEFAULT_REPLY_DECISION_THRESHOLD: f64 = 0.85;
pub const DEFAULT_OPENROUTER_TIMEOUT_SECS: u64 = 120;

#[derive(Clone)]
pub struct Config {
    pub discord_token: String,
    pub discord_guild_id: NonZeroU64,
    pub openrouter_api_key: String,
    pub openrouter_model: String,
    pub reply_decision_models: Vec<String>,
    pub reply_decision_threshold: f64,
    pub openrouter_base_url: String,
    pub openrouter_timeout_secs: u64,
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
            reply_decision_models: parse_model_list(
                &optional("OPENROUTER_REPLY_DECISION_MODELS")
                    .unwrap_or_else(|| DEFAULT_REPLY_DECISION_MODELS.to_owned()),
            )?,
            reply_decision_threshold: optional("REPLY_DECISION_THRESHOLD")
                .map(|value| parse_threshold("REPLY_DECISION_THRESHOLD", &value))
                .transpose()?
                .unwrap_or(DEFAULT_REPLY_DECISION_THRESHOLD),
            openrouter_base_url: optional("OPENROUTER_BASE_URL")
                .unwrap_or_else(|| DEFAULT_OPENROUTER_BASE_URL.to_owned()),
            openrouter_timeout_secs: optional("OPENROUTER_TIMEOUT_SECS")
                .map(|value| {
                    value
                        .parse::<u64>()
                        .map_err(|_| ConfigError::InvalidInteger("OPENROUTER_TIMEOUT_SECS"))
                })
                .transpose()?
                .unwrap_or(DEFAULT_OPENROUTER_TIMEOUT_SECS)
                .clamp(5, 300),
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

fn parse_model_list(value: &str) -> Result<Vec<String>, ConfigError> {
    let mut models = Vec::new();
    for model in value
        .split(',')
        .map(str::trim)
        .filter(|model| !model.is_empty())
    {
        if !models.iter().any(|existing| existing == model) {
            models.push(model.to_owned());
        }
    }
    if models.is_empty() {
        return Err(ConfigError::EmptyModelList);
    }
    Ok(models)
}

fn parse_threshold(name: &'static str, value: &str) -> Result<f64, ConfigError> {
    let threshold = value
        .parse::<f64>()
        .map_err(|_| ConfigError::InvalidNumber(name))?;
    if !threshold.is_finite() || !(0.0..=1.0).contains(&threshold) {
        return Err(ConfigError::NumberOutOfRange(name));
    }
    Ok(threshold)
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
    #[error("{0} must be a number")]
    InvalidNumber(&'static str),
    #[error("{0} must be between 0.0 and 1.0")]
    NumberOutOfRange(&'static str),
    #[error("OPENROUTER_REPLY_DECISION_MODELS must contain at least one model")]
    EmptyModelList,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_documented_openrouter_settings() {
        assert_eq!(DEFAULT_OPENROUTER_MODEL, "openai/gpt-5.6-luna");
        assert_eq!(DEFAULT_OPENROUTER_BASE_URL, "https://openrouter.ai/api/v1");
        assert_eq!(DEFAULT_REPLY_DECISION_THRESHOLD, 0.85);
        assert_eq!(DEFAULT_OPENROUTER_TIMEOUT_SECS, 120);
        assert!(DEFAULT_REPLY_DECISION_MODELS.starts_with("google/gemma-4-26b-a4b-it:free"));
    }

    #[test]
    fn model_list_is_trimmed_and_deduplicated() {
        assert_eq!(
            parse_model_list(" free/a, free/b,free/a ").unwrap(),
            vec!["free/a", "free/b"]
        );
        assert_eq!(parse_model_list(" , "), Err(ConfigError::EmptyModelList));
    }

    #[test]
    fn threshold_must_be_a_finite_probability() {
        assert_eq!(parse_threshold("TEST", "0.91").unwrap(), 0.91);
        assert_eq!(
            parse_threshold("TEST", "1.1"),
            Err(ConfigError::NumberOutOfRange("TEST"))
        );
        assert_eq!(
            parse_threshold("TEST", "NaN"),
            Err(ConfigError::NumberOutOfRange("TEST"))
        );
    }
}
