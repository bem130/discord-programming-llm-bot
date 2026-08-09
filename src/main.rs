use anyhow::Context;
use discord_learn_programming_bot::config::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    init_tracing();

    let _config = Config::from_env().context("invalid application configuration")?;
    tracing::info!(
        "configuration loaded; bot startup will be wired in the next implementation step"
    );
    Ok(())
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "discord_learn_programming_bot=info,serenity=warn".into());

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
