# Discord Learn Programming Bot

指定した1つのDiscordサーバーに常駐し、サーバー内で共有した会話コンテキストを使ってプログラミング学習を支援するRust製Botです。OpenRouter経由の既定モデルは `openai/gpt-5.6-luna` です。

## 実装済み機能

- 必須の `DISCORD_GUILD_ID` と一致する1サーバーだけで動作
- `/ai channel` でチャンネルごとに `off`、`read_only`、`mention`、`auto` を設定
- `/ai role` でBotを呼び出すサーバー共通Roleを設定
- threadとforum postでは親チャンネルのAIアクセス設定を継承
- `/ai status` で単一チャンネルまたはサーバー全体の設定を確認
- スラッシュコマンドの応答は公開。ephemeral応答は使用しない
- SQLiteに許可チャンネルの会話を保存し、直近の会話をチャンネル横断で参照
- テキスト・コード、PNG、JPEG、WebP、GIF、PDFの添付を読解
- OpenRouterのWeb検索server toolを通常回答で利用可能にし、取得したannotationを「参考リンク」として追記
- AI生成後にDiscord非互換表現の静的検査、会話に基づくAIレビュー、必要に応じた修正を実行
- 2,000文字制限を考慮して、Unicodeとコードフェンスを壊しにくい形でRust側から分割送信
- 通常のUnicode絵文字とサーバー固有絵文字によるリアクションを最大3件追加
- 任意でLima VMを起動・管理し、Rust、Cargo、GCCなどによるコード検証とapt packageの導入を実行
- 運営者 `@bem130` の設計思想を参考knowledgeとして利用。ただし他ユーザーへは強制しない

## ディレクトリ構成

```text
src/                         Rust実装
  discord/                   Discordイベントとslash command
  runner/                    Lima template
prompt/                      日本語prompt
  knowledge/                 出典・帰属を持つ参考knowledge
migration/                   SQLite migration
data/                        既定のSQLite保存先
```

主要なソース、prompt、DB schemaはそれぞれ `src/`、`prompt/`、`migration/` に分離しています。

## 必要なもの

- Rust stable toolchain
- Discord ApplicationとBot token
- OpenRouter API key
- コード実行を有効にする場合はLima 2.xと、そのhostで利用できるvirtualization backend

Limaの主要な対応hostはmacOSとLinuxです。Windows hostも記載されていますが未検証扱いで、WSL2 backendも実験的です。まずBot本体を `CODE_RUNNER_ENABLED=false` で起動し、Limaは対応host上で個別に検証することを推奨します。

## Discord Developer Portal

Botのinstall scopeには `bot` と `applications.commands` を指定します。Bot設定で **Message Content Intent** を有効にしてください。これが無いと通常メッセージ本文や添付情報を取得できません。

推奨Permissions Integerは、thread内の応答と将来の添付送信を含めて次の値です。

```text
274878024768
```

含まれる権限:

- View Channels
- Send Messages
- Send Messages in Threads
- Read Message History
- Add Reactions
- Embed Links
- Attach Files

threadを使わない場合は `117824` まで削減できます。

リアクションは通常リアクションのみです。スーパーリアクションは使用しません。標準絵文字のshortcodeはUnicodeへ変換し、`:custom_name:` は対象サーバーで利用可能な同名カスタム絵文字へ解決します。

## 設定

`.env.example` を参考に、Git管理しない `.env` を作成します。

```dotenv
DISCORD_TOKEN=replace-with-discord-bot-token
DISCORD_GUILD_ID=123456789012345678
OPENROUTER_API_KEY=replace-with-openrouter-api-key
DATABASE_URL=sqlite://data/bot.db?mode=rwc

OPENROUTER_MODEL=openai/gpt-5.6-luna
OPENROUTER_BASE_URL=https://openrouter.ai/api/v1
RUST_LOG=discord_learn_programming_bot=info,serenity=warn

CODE_RUNNER_ENABLED=false
LIMA_INSTANCE=discord-code-runner
LIMA_TEMPLATE=src/runner/lima.yaml
CODE_EXECUTION_TIMEOUT_SECS=20
```

`DISCORD_TOKEN`、`DISCORD_GUILD_ID`、`OPENROUTER_API_KEY`、`DATABASE_URL` は必須です。SQLite migrationは起動時に自動適用されます。PostgreSQLは使用しません。

## 起動

まず秘密情報を使わない検査を実行します。

```powershell
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

検査後、Botを起動します。

```powershell
cargo run --release
```

起動時にguild commandが `DISCORD_GUILD_ID` のサーバーへ登録されます。最初は全チャンネルが `off` なので、Discord上で次のように設定します。

```text
/ai channel mode:読み取りのみ target:#learning-log
/ai channel mode:メンション時に返信 target:#questions
/ai channel mode:自動返信 target:#pair-programming
/ai role action:設定 target:@aibot
/ai status
```

モードの意味:

- `off`: 保存も応答もしない
- `read_only`: 会話と対応添付をサーバー共有コンテキストへ保存するが、応答・リアクションはしない
- `mention`: 保存し、Botが明示的にメンションされた場合だけ必ず応答する
- `auto`: 保存し、メンションまたはBotへの返信には必ず応答する。それ以外はAIの介入scoreが `0.85` 以上なら応答し、短い反応だけで十分ならリアクションを付ける

設定したRoleへのメンションも、Botユーザーへの直接メンションと同じ明示呼び出しとして扱います。Role設定の解除は `/ai role action:解除` です。一般ユーザーがRoleをメンションできるようにするには、Discord側でそのRoleをmentionableにするか、利用者にRoleをメンションできる権限が必要です。

thread内のメッセージはthread IDではなく親チャンネルのモードで判定します。会話の保存先情報と返信先は実際のthreadのままなので、親チャンネルが `mention` または `auto` ならthread内でも同じ動作をします。thread内で `/ai channel` や `/ai status target:` を実行した場合も親チャンネルを対象にします。

`/ai` はDiscord側のdefault member permissionと実行時検査の両方で、Manage Server権限を持つ利用者だけに制限しています。

## 添付とコンテキスト

- テキスト・コードは1ファイル512 KiBまでUTF-8として抽出
- 画像とPDFは1ファイル20 MiBまでSQLiteへ保存
- 1回のAI requestへ送るmultimodal添付は直近8件、合計20 MiBまで
- 回答に使う会話はサーバー全体から直近40件
- OpenRouterには画像をbase64 `image_url`、PDFをbase64 `file` inputとして送信

`read_only`、`mention`、`auto` のチャンネル内容は、別の許可チャンネルの回答でも参照され得ます。秘密チャンネルや共有してはいけない情報を扱うチャンネルは `off` にしてください。SQLiteには本文と対応添付が暗号化せず保存されるため、DBファイルのOS権限とbackupの扱いも管理してください。

## 回答pipeline

```text
会話保存
  -> 自動介入判定・リアクション選択
  -> 必要ならLima実行計画と隔離実行
  -> Web検索を利用可能にして回答生成
  -> Markdown表・LaTeX・未閉鎖code fence・broadcast mentionの静的検査
  -> 元のpromptと会話に基づくAIレビュー
  -> 不承認なら最大2回修正
  -> Rust側で分割してDiscordへ送信
```

各promptには具体的な出力例を含めています。運営者方針は [owner_bem130_programming_stance.md](prompt/knowledge/owner_bem130_programming_stance.md) から独立したsystem knowledgeとして読み込みます。これはBotの安全規則やサーバー共通規約ではありません。

## Limaコード実行の安全境界

`CODE_RUNNER_ENABLED=true` の場合、Botは固定名のLima instanceを起動し、必要ならtemplateから作成します。host directoryのmountとport forwardingは無効にし、実行ファイルは一時directoryへbase64転送します。ファイル数、合計size、command数、package名、実行時間、出力量をRust側で制限します。

ただし、これは完全な対敵sandboxではありません。

- guestはpackage導入とWeb上の依存取得のためnetworkへ接続できる
- Lima instanceと導入済みsystem packageは実行間で持続する
- passwordless sudoを持つ専用guestであり、guest自体は破壊され得る
- AIが作る実行計画や利用者のコードが安全だと保証するものではない

公開サーバーで不特定ユーザーへ提供する場合は、Bot host自体も専用VMに置き、egress制限、実行ごとのVM再作成またはsnapshot復元、resource quota、監視を追加してください。hostやLima guestへDiscord tokenやOpenRouter keyを配置・転送しないでください。

## 公式資料

- [GPT-5.6 Luna model](https://developers.openai.com/api/docs/models/gpt-5.6-luna)
- [OpenRouter Web Search server tool](https://openrouter.ai/docs/guides/features/server-tools/web-search)
- [OpenRouter multimodal inputs](https://openrouter.ai/docs/guides/overview/multimodal/overview)
- [Discord permissions](https://docs.discord.com/developers/topics/permissions)
- [Discord message resource](https://docs.discord.com/developers/resources/message)
- [Discord Gateway intents](https://docs.discord.com/developers/events/gateway)
- [Lima installation](https://lima-vm.io/docs/installation/)
- [Lima WSL2 backend](https://lima-vm.io/docs/config/vmtype/wsl2/)
