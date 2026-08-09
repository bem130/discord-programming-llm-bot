use anyhow::{Context as _, Result, bail};
use serenity::all::{
    ChannelId, CommandDataOption, CommandDataOptionValue, CommandInteraction, CommandOptionType,
    Context, CreateAllowedMentions, CreateCommand, CreateCommandOption, CreateInteractionResponse,
    CreateInteractionResponseFollowup, CreateInteractionResponseMessage, GuildId, Permissions,
};

use crate::{db::Database, delivery::split_message, model::ChannelMode};

pub fn ai_command() -> CreateCommand {
    let channel = CreateCommandOption::new(
        CommandOptionType::SubCommand,
        "channel",
        "チャンネルのAIアクセスモードを設定します",
    )
    .add_sub_option(
        CreateCommandOption::new(CommandOptionType::String, "mode", "設定するモード")
            .required(true)
            .add_string_choice("無効", "off")
            .add_string_choice("読み取りのみ", "read_only")
            .add_string_choice("メンション時に返信", "mention")
            .add_string_choice("自動返信", "auto"),
    )
    .add_sub_option(
        CreateCommandOption::new(
            CommandOptionType::Channel,
            "target",
            "対象チャンネル（省略時は現在のチャンネル）",
        )
        .required(false),
    );
    let status = CreateCommandOption::new(
        CommandOptionType::SubCommand,
        "status",
        "現在のAIアクセス設定を表示します",
    )
    .add_sub_option(
        CreateCommandOption::new(
            CommandOptionType::Channel,
            "target",
            "対象チャンネル（省略時はサーバー内の設定一覧）",
        )
        .required(false),
    );

    CreateCommand::new("ai")
        .description("プログラミング講師AIの設定を管理します")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .dm_permission(false)
        .add_option(channel)
        .add_option(status)
}

pub async fn handle_ai_command(
    context: &Context,
    command: &CommandInteraction,
    configured_guild_id: GuildId,
    database: &Database,
) -> Result<()> {
    if command.guild_id != Some(configured_guild_id) {
        respond_public(
            context,
            command,
            "このBotは設定されたサーバーでのみ動作します。",
        )
        .await?;
        return Ok(());
    }
    if !has_manage_guild(command) {
        respond_public(
            context,
            command,
            "このコマンドの実行には「サーバーを管理」権限が必要です。",
        )
        .await?;
        return Ok(());
    }

    let subcommand = command.data.options.first().context("missing subcommand")?;
    match (subcommand.name.as_str(), &subcommand.value) {
        ("channel", CommandDataOptionValue::SubCommand(options)) => {
            set_channel(context, command, configured_guild_id, database, options).await
        }
        ("status", CommandDataOptionValue::SubCommand(options)) => {
            show_status(context, command, configured_guild_id, database, options).await
        }
        _ => bail!("unknown /ai subcommand"),
    }
}

async fn set_channel(
    context: &Context,
    command: &CommandInteraction,
    guild_id: GuildId,
    database: &Database,
    options: &[CommandDataOption],
) -> Result<()> {
    let mode = string_option(options, "mode")
        .context("missing mode")?
        .parse::<ChannelMode>()?;
    let channel_id = channel_option(options, "target").unwrap_or(command.channel_id);
    database
        .set_channel_mode(
            guild_id.get(),
            channel_id.get(),
            mode,
            command.user.id.get(),
        )
        .await?;

    respond_public(
        context,
        command,
        &format!(
            "<#{}> のAIアクセスを `{}` に設定しました。\n{}",
            channel_id.get(),
            mode,
            mode.description()
        ),
    )
    .await
}

async fn show_status(
    context: &Context,
    command: &CommandInteraction,
    guild_id: GuildId,
    database: &Database,
    options: &[CommandDataOption],
) -> Result<()> {
    let content = if let Some(channel_id) = channel_option(options, "target") {
        let mode = database
            .channel_mode(guild_id.get(), channel_id.get())
            .await?;
        format!(
            "<#{}> のAIアクセスは `{}` です。\n{}",
            channel_id.get(),
            mode,
            mode.description()
        )
    } else {
        let settings = database.channel_settings(guild_id.get()).await?;
        if settings.is_empty() {
            "設定済みチャンネルはありません。すべてのチャンネルは既定で `off` です。".to_owned()
        } else {
            let mut lines = vec![
                "現在のAIアクセス設定:".to_owned(),
                "（一覧にないチャンネルは `off` です）".to_owned(),
            ];
            lines.extend(
                settings
                    .into_iter()
                    .map(|setting| format!("- <#{}>: `{}`", setting.channel_id, setting.mode)),
            );
            lines.join("\n")
        }
    };

    respond_public(context, command, &content).await
}

pub async fn respond_with_generic_error(context: &Context, command: &CommandInteraction) {
    let _ = command
        .create_response(
            &context.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("処理に失敗しました。ログを確認してください。")
                    .allowed_mentions(CreateAllowedMentions::new()),
            ),
        )
        .await;
}

async fn respond_public(
    context: &Context,
    command: &CommandInteraction,
    content: &str,
) -> Result<()> {
    let mut chunks = split_message(content).into_iter();
    let first = chunks.next().context("response content was empty")?;
    command
        .create_response(
            &context.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(first)
                    .allowed_mentions(CreateAllowedMentions::new()),
            ),
        )
        .await?;
    for chunk in chunks {
        command
            .create_followup(
                &context.http,
                CreateInteractionResponseFollowup::new()
                    .content(chunk)
                    .allowed_mentions(CreateAllowedMentions::new()),
            )
            .await?;
    }
    Ok(())
}

fn string_option<'a>(options: &'a [CommandDataOption], name: &str) -> Option<&'a str> {
    options.iter().find_map(|option| {
        (option.name == name)
            .then_some(&option.value)
            .and_then(|value| match value {
                CommandDataOptionValue::String(value) => Some(value.as_str()),
                _ => None,
            })
    })
}

fn channel_option(options: &[CommandDataOption], name: &str) -> Option<ChannelId> {
    options.iter().find_map(|option| {
        (option.name == name)
            .then_some(&option.value)
            .and_then(|value| match value {
                CommandDataOptionValue::Channel(channel_id) => Some(*channel_id),
                _ => None,
            })
    })
}

fn has_manage_guild(command: &CommandInteraction) -> bool {
    command
        .member
        .as_ref()
        .and_then(|member| member.permissions)
        .is_some_and(|permissions| {
            permissions.contains(Permissions::MANAGE_GUILD)
                || permissions.contains(Permissions::ADMINISTRATOR)
        })
}
