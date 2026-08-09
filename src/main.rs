use std::{sync::Arc, time::Duration};

use anyhow::Context;
use discord_learn_programming_bot::{
    ai::{OpenRouterClient, SharedLanguageModel},
    config::Config,
    db::Database,
    discord::{DiscordBot, required_intents},
    runner::{LimaRunner, SharedCodeRunner},
};
use serenity::{Client, all::GuildId};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    init_tracing();

    let config = Config::from_env().context("invalid application configuration")?;
    let database = Database::connect(&config.database_url).await?;
    let model: SharedLanguageModel = Arc::new(OpenRouterClient::new(
        &config.openrouter_base_url,
        config.openrouter_api_key.clone(),
        config.openrouter_model.clone(),
    )?);
    let decision_models: Vec<SharedLanguageModel> = config
        .reply_decision_models
        .iter()
        .map(|model| {
            OpenRouterClient::new(
                &config.openrouter_base_url,
                config.openrouter_api_key.clone(),
                model.clone(),
            )
            .map(|client| Arc::new(client) as SharedLanguageModel)
        })
        .collect::<anyhow::Result<_>>()?;
    let runner: Option<SharedCodeRunner> = if config.code_runner_enabled {
        let runner = Arc::new(LimaRunner::new(
            config.lima_instance,
            config.lima_template,
            Duration::from_secs(config.code_execution_timeout_secs),
        )?);
        runner.ensure_ready().await?;
        Some(runner)
    } else {
        None
    };
    let handler = DiscordBot::new(
        GuildId::new(config.discord_guild_id.get()),
        database,
        model,
        decision_models,
        config.reply_decision_threshold,
        runner,
    );
    let mut client = Client::builder(config.discord_token, required_intents())
        .event_handler(handler)
        .await
        .context("failed to create Discord client")?;

    client.start().await.context("Discord client stopped")
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "discord_learn_programming_bot=info,serenity=warn".into());

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
