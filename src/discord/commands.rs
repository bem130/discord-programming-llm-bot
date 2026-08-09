use anyhow::{Context as _, Result, bail};
use serenity::all::{
    ChannelId, CommandDataOption, CommandDataOptionValue, CommandInteraction, CommandOptionType,
    Context, CreateAllowedMentions, CreateCommand, CreateCommandOption, CreateInteractionResponse,
    CreateInteractionResponseFollowup, CreateInteractionResponseMessage, GuildId, Permissions,
    RoleId,
};

use crate::{
    db::Database,
    delivery::split_message,
    model::{ChannelMode, MemoryKind, MemoryStatus},
};

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
    let role = CreateCommandOption::new(
        CommandOptionType::SubCommand,
        "role",
        "Botを呼び出すサーバー共通Roleを設定または解除します",
    )
    .add_sub_option(
        CreateCommandOption::new(CommandOptionType::String, "action", "実行する操作")
            .required(true)
            .add_string_choice("設定", "set")
            .add_string_choice("解除", "clear"),
    )
    .add_sub_option(
        CreateCommandOption::new(
            CommandOptionType::Role,
            "target",
            "設定するRole（actionがsetの場合は必須）",
        )
        .required(false),
    );
    let memory_add = CreateCommandOption::new(
        CommandOptionType::SubCommand,
        "add",
        "確認済みの重要情報をサーバー共有メモリへ追加します",
    )
    .add_sub_option(
        CreateCommandOption::new(CommandOptionType::String, "kind", "メモリの種類")
            .required(true)
            .add_string_choice("運用フィードバック", "feedback")
            .add_string_choice("プロジェクト情報・判断", "project")
            .add_string_choice("参考資料", "reference"),
    )
    .add_sub_option(
        CreateCommandOption::new(CommandOptionType::String, "title", "短い識別名").required(true),
    )
    .add_sub_option(
        CreateCommandOption::new(CommandOptionType::String, "summary", "毎回参照する短い要約")
            .required(true),
    )
    .add_sub_option(
        CreateCommandOption::new(
            CommandOptionType::String,
            "content",
            "必要時に参照する詳細と判断理由",
        )
        .required(true),
    );
    let memory_list = CreateCommandOption::new(
        CommandOptionType::SubCommand,
        "list",
        "メモリ候補・有効メモリ・期限切れを一覧表示します",
    );
    let memory_approve = memory_id_subcommand("approve", "AIが提案したメモリ候補を承認します");
    let memory_review =
        memory_id_subcommand("review", "内容を確認したメモリの期限を30日延長します");
    let memory_remove = memory_id_subcommand("remove", "メモリまたは候補を無効化します");
    let memory = CreateCommandOption::new(
        CommandOptionType::SubCommandGroup,
        "memory",
        "サーバー共有の長期メモリを管理します",
    )
    .add_sub_option(memory_add)
    .add_sub_option(memory_list)
    .add_sub_option(memory_approve)
    .add_sub_option(memory_review)
    .add_sub_option(memory_remove);

    CreateCommand::new("ai")
        .description("プログラミング講師AIの設定を管理します")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .dm_permission(false)
        .add_option(channel)
        .add_option(role)
        .add_option(memory)
        .add_option(status)
}

fn memory_id_subcommand(name: &'static str, description: &'static str) -> CreateCommandOption {
    CreateCommandOption::new(CommandOptionType::SubCommand, name, description).add_sub_option(
        CreateCommandOption::new(CommandOptionType::Integer, "id", "対象メモリID").required(true),
    )
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
        ("role", CommandDataOptionValue::SubCommand(options)) => {
            configure_invocation_role(context, command, configured_guild_id, database, options)
                .await
        }
        ("memory", CommandDataOptionValue::SubCommandGroup(options)) => {
            handle_memory(context, command, configured_guild_id, database, options).await
        }
        ("status", CommandDataOptionValue::SubCommand(options)) => {
            show_status(context, command, configured_guild_id, database, options).await
        }
        _ => bail!("unknown /ai subcommand"),
    }
}

async fn handle_memory(
    context: &Context,
    command: &CommandInteraction,
    guild_id: GuildId,
    database: &Database,
    options: &[CommandDataOption],
) -> Result<()> {
    let action = options.first().context("missing memory action")?;
    let CommandDataOptionValue::SubCommand(arguments) = &action.value else {
        bail!("invalid memory action");
    };
    match action.name.as_str() {
        "add" => {
            let kind = string_option(arguments, "kind")
                .context("missing memory kind")?
                .parse::<MemoryKind>()?;
            let title = string_option(arguments, "title").context("missing memory title")?;
            let summary = string_option(arguments, "summary").context("missing memory summary")?;
            let content = string_option(arguments, "content").context("missing memory content")?;
            if let Some(problem) = validate_memory_text(title, summary, content) {
                return respond_public(context, command, problem).await;
            }
            let id = database
                .create_memory(
                    guild_id.get(),
                    kind,
                    title,
                    summary,
                    content,
                    MemoryStatus::Active,
                    command.user.id.get(),
                    Some(command.channel_id.get()),
                    None,
                )
                .await?;
            respond_public(
                context,
                command,
                &format!(
                    "メモリ `#{id}`（`{kind}`）を追加しました。30日以内に `/ai memory review` で再確認してください。"
                ),
            )
            .await
        }
        "list" => {
            let memories = database.list_memories(guild_id.get()).await?;
            let mut lines =
                vec!["サーバー共有メモリ（候補は承認されるまでAI回答に使われません）:".to_owned()];
            if memories.is_empty() {
                lines.push("- なし".to_owned());
            } else {
                lines.extend(memories.into_iter().map(|memory| {
                    let state = if memory.stale {
                        "stale"
                    } else {
                        memory.status.as_str()
                    };
                    format!(
                        "- `#{}` [`{}` / `{state}`] {} — {}",
                        memory.id, memory.kind, memory.title, memory.summary
                    )
                }));
            }
            respond_public(context, command, &lines.join("\n")).await
        }
        "approve" => {
            let id = memory_id(arguments)?;
            let changed = database.approve_memory(guild_id.get(), id).await?;
            respond_public(
                context,
                command,
                if changed {
                    "候補を承認し、有効な共有メモリにしました。"
                } else {
                    "承認可能な候補が見つかりません。IDと状態を確認してください。"
                },
            )
            .await
        }
        "review" => {
            let id = memory_id(arguments)?;
            let changed = database.review_memory(guild_id.get(), id).await?;
            respond_public(
                context,
                command,
                if changed {
                    "内容を再確認済みにし、参照期限を30日延長しました。"
                } else {
                    "有効なメモリが見つかりません。IDと状態を確認してください。"
                },
            )
            .await
        }
        "remove" => {
            let id = memory_id(arguments)?;
            let changed = database.remove_memory(guild_id.get(), id).await?;
            respond_public(
                context,
                command,
                if changed {
                    "メモリを無効化しました。以後AI回答には使われません。"
                } else {
                    "対象メモリが見つかりません。"
                },
            )
            .await
        }
        _ => bail!("unknown memory action"),
    }
}

fn validate_memory_text(title: &str, summary: &str, content: &str) -> Option<&'static str> {
    if title.trim().is_empty() || summary.trim().is_empty() || content.trim().is_empty() {
        return Some("title、summary、contentには空でない文字列を指定してください。");
    }
    if title.chars().count() > 100
        || summary.chars().count() > 300
        || content.chars().count() > 3_000
    {
        return Some("長さの上限はtitle 100文字、summary 300文字、content 3000文字です。");
    }
    None
}

fn memory_id(options: &[CommandDataOption]) -> Result<i64> {
    let id = integer_option(options, "id").context("missing memory id")?;
    anyhow::ensure!(id > 0, "memory id must be positive");
    Ok(id)
}

async fn configure_invocation_role(
    context: &Context,
    command: &CommandInteraction,
    guild_id: GuildId,
    database: &Database,
    options: &[CommandDataOption],
) -> Result<()> {
    match string_option(options, "action").context("missing role action")? {
        "set" => {
            let Some(role_id) = role_option(options, "target") else {
                respond_public(
                    context,
                    command,
                    "設定するRoleを `target` に指定してください。",
                )
                .await?;
                return Ok(());
            };
            if role_id.get() == guild_id.get() {
                respond_public(
                    context,
                    command,
                    "`@everyone` は呼び出しRoleに設定できません。専用Roleを指定してください。",
                )
                .await?;
                return Ok(());
            }
            database
                .set_invocation_role(guild_id.get(), role_id.get(), command.user.id.get())
                .await?;
            respond_public(
                context,
                command,
                &format!(
                    "<@&{}> をBot呼び出しRoleに設定しました。直接メンションと同じように扱います。",
                    role_id.get()
                ),
            )
            .await
        }
        "clear" => {
            database.clear_invocation_role(guild_id.get()).await?;
            respond_public(context, command, "Bot呼び出しRoleの設定を解除しました。").await
        }
        _ => bail!("unknown role action"),
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
    let requested_channel_id = channel_option(options, "target").unwrap_or(command.channel_id);
    let channel_id = super::configuration_channel_id(context, requested_channel_id).await?;
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
    let content = if let Some(requested_channel_id) = channel_option(options, "target") {
        let channel_id = super::configuration_channel_id(context, requested_channel_id).await?;
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
        let invocation_role = database.invocation_role_id(guild_id.get()).await?;
        let mut lines = vec!["現在のAI設定:".to_owned()];
        lines.push(match invocation_role {
            Some(role_id) => format!("- Bot呼び出しRole: <@&{role_id}>"),
            None => "- Bot呼び出しRole: 未設定".to_owned(),
        });
        if settings.is_empty() {
            lines.push("- 設定済みチャンネルなし（すべてのチャンネルは既定で `off`）".to_owned());
        } else {
            lines.push("- チャンネル設定（一覧にないチャンネルは `off`）:".to_owned());
            lines.extend(
                settings
                    .into_iter()
                    .map(|setting| format!("  - <#{}>: `{}`", setting.channel_id, setting.mode)),
            );
        }
        lines.join("\n")
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

fn role_option(options: &[CommandDataOption], name: &str) -> Option<RoleId> {
    options.iter().find_map(|option| {
        (option.name == name)
            .then_some(&option.value)
            .and_then(|value| match value {
                CommandDataOptionValue::Role(role_id) => Some(*role_id),
                _ => None,
            })
    })
}

fn integer_option(options: &[CommandDataOption], name: &str) -> Option<i64> {
    options.iter().find_map(|option| {
        (option.name == name)
            .then_some(&option.value)
            .and_then(|value| match value {
                CommandDataOptionValue::Integer(value) => Some(*value),
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
