# Discord Learn Programming Bot

指定した1つのDiscordサーバーに常駐し、サーバー内で共有した会話コンテキストを使ってプログラミング学習を支援するRust製Botです。OpenRouter経由の既定モデルは `openai/gpt-5.6-luna` です。

## 実装済み機能

- 必須の `DISCORD_GUILD_ID` と一致する1サーバーだけで動作
- `/ai channel` でチャンネルごとに `off`、`read_only`、`mention`、`auto` を設定
- `/ai role` でBotを呼び出すサーバー共通Roleを設定
- threadとforum postでは親チャンネルのAIアクセス設定を継承
- `/ai status` で単一チャンネルまたはサーバー全体の設定を確認
- スラッシュコマンドの応答は公開。ephemeral応答は使用しない
- SQLiteに許可チャンネルの会話を保存し、現在のチャンネルの直近5件と最近アクティブなチャンネル一覧を毎回参照
- AIが保存済みの全会話と抽出済み添付テキストを検索し、日時・現在チャンネル判定・DiscordメッセージURL付きで取得
- Discord上のメッセージ編集・削除をDBへ同期し、検索対象は常に最新版に更新
- 索引・詳細・出典・30日の再確認期限を持ち、AIの独立審査後に自動追加される重要メモリ
- テキスト・コード、PNG、JPEG、WebP、GIF、PDFの添付を読解
- OpenRouterのWeb検索server toolを通常回答で利用可能にし、取得したannotationを「参考リンク」として追記
- 自動介入判定は無料モデルを優先し、無料枠・provider障害時は設定順にmodel fallback
- AI生成後にDiscord非互換表現の静的検査、会話に基づくAIレビュー、必要に応じた修正を実行
- 回答生成・検索・実行・レビュー中はtyping表示を8秒ごとに更新し、完了または失敗時に停止
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
- コード実行を有効にする場合はLima 2.xとQEMU 11.0以降

Lima公式ではWindows hostが未検証扱いで、WSL2 backendも実験的です。このプロジェクトのtemplateは通常のUbuntu disk imageと `apt` を使うため、host間で挙動が変わらないよう `vmType: qemu` を明示しています。

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
OPENROUTER_REPLY_DECISION_MODELS=google/gemma-4-26b-a4b-it:free,nvidia/nemotron-3-super-120b-a12b:free,openrouter/free,openai/gpt-5.6-luna
REPLY_DECISION_THRESHOLD=0.85
OPENROUTER_BASE_URL=https://openrouter.ai/api/v1
RUST_LOG=discord_learn_programming_bot=info,serenity=warn

CODE_RUNNER_ENABLED=false
LIMA_INSTANCE=discord-code-runner
LIMA_TEMPLATE=src/runner/lima.yaml
CODE_EXECUTION_TIMEOUT_SECS=20
```

`DISCORD_TOKEN`、`DISCORD_GUILD_ID`、`OPENROUTER_API_KEY`、`DATABASE_URL` は必須です。SQLite migrationは起動時に自動適用されます。PostgreSQLは使用しません。

## Lima VMのセットアップ

Windowsでは、リポジトリ直下から次のscriptを実行します。

```powershell
powershell -ExecutionPolicy Bypass -File scripts/setup-lima-windows.ps1
```

このscriptは次を行います。

- QEMU 11.0以降がなければ公式winget packageを導入またはupgrade
- Lima 2.2.0の公式Windows archiveをdownloadし、releaseのSHA-256 digestを検証
- Limaをユーザー領域へ展開し、Limaと必要なGit Unix toolsをユーザーの `PATH` へ追加
- `src/runner/lima.yaml` を検証して `discord-code-runner` instanceを作成・起動
- guest内のRust、Cargo、GCC、Pythonをsmoke test

再実行しても、同じversionのLimaと既存instanceは再利用します。完了後は新しいPowerShellを開き、状態を確認できます。

```powershell
limactl --version
limactl list
limactl shell discord-code-runner bash -lc "rustc --version && cargo --version"
```

VMの作成・起動を行わずhost toolだけ導入する場合は `-SkipVmStart` を指定します。instance名やtemplateを変える場合は、それぞれ `-InstanceName` と `-Template` を指定し、Bot側の `LIMA_INSTANCE` と `LIMA_TEMPLATE` に同じ値を設定してください。

中断された初回作成などでinstanceが `Broken` になった場合、scriptは既存diskを勝手に削除せず停止します。そのinstanceを破棄してよいことを確認した場合だけ、次のように再作成します。

```powershell
powershell -ExecutionPolicy Bypass -File scripts/setup-lima-windows.ps1 `
  -RecreateBrokenInstance
```

macOSまたはLinuxでは[Lima公式のinstallation手順](https://lima-vm.io/docs/installation/)に従ってLimaとQEMUを導入した後、次を実行します。

```bash
limactl validate src/runner/lima.yaml
limactl --tty=false start --name discord-code-runner src/runner/lima.yaml
limactl --tty=false shell discord-code-runner bash -lc \
  'rustc --version && cargo --version && gcc --version | head -n 1 && python3 --version'
```

VMのsmoke testが完了するまでは `CODE_RUNNER_ENABLED=false` のままにします。Botを有効化するときだけ `true` に変更してください。Botの起動時には既存instanceを再利用し、停止中なら起動します。

`OPENROUTER_REPLY_DECISION_MODELS` はアプリケーションが左から順に試すmodel IDです。既定値は、無料かつstructured outputs対応の `google/gemma-4-26b-a4b-it:free`、`nvidia/nemotron-3-super-120b-a12b:free`、利用可能な無料modelを選ぶ `openrouter/free`、最後に通常回答と同じLunaです。HTTPエラーだけでなく、不正JSONや空応答でも次のmodelへ進みます。JSON要求にはOpenRouterのResponse Healingも使用します。`inclusionai/ling-3.0-tiny:free` は2026-08-09時点の公式model一覧ではstructured outputs非対応のため、既定の判定modelからは外しています。

最後のLunaまで到達すると有料推論になるため、完全無料に限定する場合は末尾のLunaを削除できます。その場合、全無料modelが失敗した自動介入は安全側に倒して発言しません。明示メンションは判定失敗時にも返信対象となります。回答生成自体またはレビュー呼び出し自体が全再試行に失敗した場合は固定の障害案内を返信します。レビューが不承認のまま修正上限に達した場合は、意味レビューで最も高く評価された候補を送信します。静的検査の修正上限に達した場合は、broadcast mentionの除去とコードフェンス閉鎖を行ったbest-effort候補を送信します。

`REPLY_DECISION_THRESHOLD` は `0.0` から `1.0` の範囲で設定します。値を上げるほど自発的な介入が減ります。

`reply_decision` へ渡す会話は現在のチャンネルの直近5メッセージです。添付本文や画像・PDF本体は渡さず、メッセージごとの添付数、コード/テキスト添付の有無と行数、その他添付数だけを渡します。通常回答も同じチャンネルの直近5件から始め、それより古い情報や別チャンネルの本文が必要ならAIが全履歴検索を自発的に利用します。

## AI sessionとprompt送信

OpenRouterのChat Completions呼び出し自体は会話履歴を保持しません。会話ログの正本はSQLiteであり、Botは必要なコンテキストを各requestへ明示的に再送します。`session_id` はサーバー側の会話保存先ではなく、同じ処理を同じproviderへ寄せてprompt cacheを再利用しやすくするrouting keyとして使います。

sessionはDiscordチャンネルでは分割しません。`Guild ID × AI処理種別` の安定したIDを使い、通常会話、返信要否判定、コード実行計画、メモリ審査、回答レビューを分離します。同じ通常会話のtool roundとretryは同じsessionを使うため、別チャンネルへ移動してもGuild内の通常会話sessionは変わりません。OpenRouterの完全一致response cacheは、現在のDiscord会話へ古い同一回答を再利用しないよう有効化していません。

`prompt/` は `include_str!` でバイナリへ埋め込み、用途ごとに次のタイミングで送ります。実行時データより前に用途別の固定promptを置くため、対応providerの自動prompt cacheで安定したprefixを再利用できます。promptを変更した場合は再ビルドが必要です。

- 通常回答: `system.md`、運営者knowledge、runtime context、共有メモリ、現在のチャンネルの直近5件。tool roundごとにtool結果（会話検索・共有メモリ・VMコード実行）を追加して再送
- 自動返信判定: `reply_decision.md`、runtime context、現在のチャンネルの直近5件の軽量表現、利用可能な絵文字
- メモリ候補審査: `memory_review.md`、runtime context、現在のチャンネルの直近5件のテキスト、既存メモリ索引、候補
- 回答レビュー・修正: `review.md` または固定の編集promptと、通常回答に使ったruntime context・直近5件を含む元request、候補回答

runtime contextには、現在地に加えて最近アクティブな許可チャンネルを最大12件、そして現在の日時（UTC、`now` フィールド）を含めます。一覧は保存メッセージの到着順から機械的に計算し、チャンネルID・最新の保存名・現在のチャンネルか・最終発言日時だけを持ちます。別チャンネルの本文は一覧へ含めず、必要な場合だけ会話検索を使います。日時（`now`）はメッセージの `created_at` と同じUTC表現で、AIがタイムゾーンや「今日」の解釈に使います。

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
/ai memory list
```

モードの意味:

- `off`: 保存も応答もしない
- `read_only`: 会話と対応添付をサーバー共有コンテキストへ保存するが、応答・リアクションはしない
- `mention`: 保存し、Botが明示的にメンションされた場合だけ必ず応答する
- `auto`: 保存し、メンションまたはBotへの返信には必ず応答する。それ以外はAIの介入scoreが `REPLY_DECISION_THRESHOLD` 以上なら応答し、短い反応だけで十分ならリアクションを付ける

設定したRoleへのメンションも、Botユーザーへの直接メンションと同じ明示呼び出しとして扱います。Role設定の解除は `/ai role action:解除` です。一般ユーザーがRoleをメンションできるようにするには、Discord側でそのRoleをmentionableにするか、利用者にRoleをメンションできる権限が必要です。

thread内のメッセージはthread IDではなく親チャンネルのモードで判定します。会話の保存先情報と返信先は実際のthreadのままなので、親チャンネルが `mention` または `auto` ならthread内でも同じ動作をします。thread内で `/ai channel` や `/ai status target:` を実行した場合も親チャンネルを対象にします。

Discordのtyping表示は10秒で失効するため、Botは回答処理中に8秒間隔で更新します。最初の回答chunkを送信し終えるまで表示を維持し、成功・失敗のどちらでも更新taskを停止します。

`/ai` はDiscord側のdefault member permissionと実行時検査の両方で、Manage Server権限を持つ利用者だけに制限しています。

## 会話検索と長期メモリ

通常回答モデルには、次のローカルtoolを渡します。

- `search_conversation_history`: 保存済みの全会話本文と抽出済み添付テキストを検索。リテラル検索とRust正規表現、channel/user絞り込みに対応し、各結果へチャンネル名・現在チャンネル判定・日時・メッセージURLを付与
- `get_guild_memory_details`: promptに毎回渡す短い索引から、必要なメモリ詳細だけを取得
- `propose_guild_memory`: 明示的な依頼がなくても、長期的なプロジェクト判断・教訓・一次資料を検出して保存を提案
- `execute_code_in_sandbox`: 隔離されたLima VM環境でコード（Rust, Python, C/C++, Go, JavaScript, TypeScript, Haskell, OCaml, Lean 4, Coq, Elm, Common Lisp, Lazy-K等）のコンパイル・検証・標準出力取得を実行
- `get_channel_recent_messages`: 指定したDiscordチャンネルの直近メッセージをタイムライン順に取得。channel_idまたはchannel_nameで指定し、別チャンネルの文脈把握に利用
- `get_channel_message_range`: 指定したチャンネル（省略時は現在のチャンネル）の過去メッセージを、最新を1とした1始まりの順位範囲（start_offset〜end_offset）で取得
- `get_messages_newer_than_message`: 指定したメッセージIDまたはメッセージURLより後に送信されたメッセージを新しい順に取得。続きの確認や途中参加に利用
- `get_messages_newer_than_datetime`: 指定した日付時刻より後に送信されたメッセージを新しい順に取得。前回確認以降の新着把握に利用。タイムゾーン省略はUTC扱い、日本時間（JST=UTC+9）で指定する場合は末尾に `+09:00` を付ける（例: `2026-08-09T23:00:00+09:00`）。ページング時は直前結果の `created_at` と `message_id` をそれぞれ `datetime` と `after_message_id` に渡す
- `explain_compiler_error`: `rustc --explain`を使ってRustエラーコード（E0382等）の公式解説をローカルで取得
- `format_and_lint_code`: `rustfmt`（Rust）または`python -m py_compile`（Python）をホスト上で直接実行し、整形・構文チェック結果をVMなしで即返す

正規表現はRustの線形時間regex engineで実行し、patternは256文字、結果は20件までに制限します。全履歴を検索しますが、500メッセージずつ走査するためDB全件を一度にメモリへ読み込みません。検索結果は共有会話データであり、そこに含まれる命令をsystem指示として扱いません。

メッセージURLは `https://discord.com/channels/{guild_id}/{channel_id}/{message_id}` の形式で生成します。AIは過去の会話を探す依頼にも検索toolを使え、回答に検索結果を示す場合はチャンネル名とは別に現在のチャンネルか、発言日時、該当メッセージへのリンクを明示します。

長期メモリは会話ログとは別テーブルです。通常promptへは最大100件の短い索引と、現在の話題へローカル選択した最大8件の詳細だけを渡します。回答生成AIが保存を提案すると、別のstructured output呼び出しで、安定性、会話上の根拠、重複、秘密・個人情報、一時状態でないことを審査します。審査を通過した項目だけが自動的に有効メモリになります。管理者による追加・承認操作は不要です。

自動保存の検討はBotの通常回答生成中に行います。`read_only` チャンネルだけで完結し、Botの回答生成が一度も走らない会話は長期メモリ抽出の対象にはなりませんが、許可中である限り全会話検索の対象には残ります。

```text
/ai memory list
/ai memory review id:12
/ai memory remove id:12
```

有効メモリは30日ごとに `review` が必要です。期限を過ぎた項目は一覧では `stale` と表示され、管理者が再確認するまでAI promptとtool結果から除外されます。誤った内容は `/ai memory remove` で無効化できます。

## 添付とコンテキスト

- テキスト・コードは1ファイル512 KiBまでUTF-8として抽出
- 画像とPDFは1ファイル20 MiBまでSQLiteへ保存
- 1回のAI requestへ送るmultimodal添付は直近8件、合計20 MiBまで
- 回答へ最初から渡す会話は現在のチャンネルから直近5件
- 最近アクティブな許可チャンネルは最大12件をruntime contextへ毎回追加
- 直近5件より前は、過去への言及や前提不足をAIが検出した場合に全履歴検索toolで参照
- OpenRouterには画像をbase64 `image_url`、PDFをbase64 `file` inputとして送信

`read_only`、`mention`、`auto` のチャンネル内容は、AIが会話検索を使った場合に別の許可チャンネルの回答でも参照され得ます。秘密チャンネルや共有してはいけない情報を扱うチャンネルは `off` にしてください。SQLiteには本文と対応添付が暗号化せず保存されるため、DBファイルのOS権限とbackupの扱いも管理してください。

Discordで本文や添付が編集されると保存内容を置換し、本文の旧版はそのメッセージが存在する間だけrevisionとして保持します。Discordでメッセージが削除されると、本文、添付、revisionをSQLiteから連鎖削除します。削除済み内容は会話検索にも現れません。

## 回答pipeline

```text
会話保存
  -> 自動介入判定・リアクション選択
  -> Web検索・Web Fetch・全会話検索・長期メモリtool・VMコード実行toolを利用可能にして回答生成
  -> メモリ提案があれば独立AI審査後に自動保存
  -> Markdown表・LaTeX・未閉鎖code fence・broadcast mentionの静的検査
  -> 元のpromptと会話に基づくAIレビュー
  -> 不承認なら最大2回修正し、なお不承認なら最高スコア候補を送信
  -> Rust側で分割してDiscordへ送信
```

意味レビューとメモリ審査は最大3回再試行します。これらは短いstructured JSONを返す判定処理なので、Lunaのreasoningを `none` に固定し、通常回答生成のreasoning設定とは分離しています。空応答時のログには使用modelと `finish_reason` を記録します。未レビュー回答や仮メッセージを先に送ることはなく、処理中は8秒更新のtyping表示を使います。

各promptには具体的な出力例を含めています。運営者方針は [owner_bem130_programming_stance.md](prompt/knowledge/owner_bem130_programming_stance.md) から独立したsystem knowledgeとして読み込みます。これはBotの安全規則やサーバー共通規約ではありません。

## Limaコード実行の安全境界

`CODE_RUNNER_ENABLED=true` の場合、Botは固定名のLima instanceを起動し、存在しない場合だけtemplateから作成します。既存instanceの起動失敗を新規作成へ誤分類しないため、壊れたinstanceは明示的に再作成するまでエラーにします。

host directoryのmountとport forwardingは無効にし、実行ファイルは一時directoryへbase64転送します。ファイル数、合計size、command数、package名、実行時間、出力量をRust側で制限します。実行は1件ずつ直列化するため、同じinstanceで `apt` やVM resourceを競合させません。

生成されたcommandはroot権限を持つLima userから直接実行しません。`nobody` userの一時systemd unitへ落とし、system filesystemとhomeをread-only/不可視にした上で、1.5 GiB memory、150% CPU、256 process/thread、実行時間、単一file sizeを制限します。unit全体をcontrol groupとして終了するため、background processも実行後に残しません。追加のUbuntu packageを導入する段階だけは、Rust側で検証したpackage名を使ってLima userが `apt-get` を実行します。

ただし、これは完全な対敵sandboxではありません。

- guestはpackage導入とWeb上の依存取得のためnetworkへ接続できる
- Lima instanceと導入済みsystem packageは実行間で持続する
- passwordless sudoを持つ専用guestであり、guest自体は破壊され得る
- AIが作る実行計画や利用者のコードが安全だと保証するものではない

公開サーバーで不特定ユーザーへ提供する場合は、Bot host自体も専用VMに置き、egress制限、実行ごとのVM再作成またはsnapshot復元、resource quota、監視を追加してください。hostやLima guestへDiscord tokenやOpenRouter keyを配置・転送しないでください。

## 公式資料

- [GPT-5.6 Luna model](https://developers.openai.com/api/docs/models/gpt-5.6-luna)
- [OpenRouter Web Search server tool](https://openrouter.ai/docs/guides/features/server-tools/web-search)
- [OpenRouter model fallbacks](https://openrouter.ai/docs/guides/routing/model-fallbacks)
- [OpenRouter free models router](https://openrouter.ai/docs/guides/routing/routers/free-router)
- [OpenRouter structured outputs](https://openrouter.ai/docs/guides/features/structured-outputs)
- [OpenRouter response healing](https://openrouter.ai/docs/guides/features/plugins/response-healing)
- [OpenRouter tool calling](https://openrouter.ai/docs/guides/features/tool-calling)
- [OpenRouter prompt cachingとsession_id](https://openrouter.ai/docs/guides/best-practices/prompt-caching)
- [OpenRouter response caching](https://openrouter.ai/docs/guides/features/response-caching)
- [OpenRouter multimodal inputs](https://openrouter.ai/docs/guides/overview/multimodal/overview)
- [Discord permissions](https://docs.discord.com/developers/topics/permissions)
- [Discord message resource](https://docs.discord.com/developers/resources/message)
- [Discord Gateway intents](https://docs.discord.com/developers/events/gateway)
- [Lima installation](https://lima-vm.io/docs/installation/)
- [Lima WSL2 backend](https://lima-vm.io/docs/config/vmtype/wsl2/)
- [設計参考: AIエージェントのメモリ設計](https://note.com/dev_owl/n/n64fd0c4dcdcb)
