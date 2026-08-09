# Discord Learn Programming Bot

指定した1つのDiscordサーバーに常駐し、サーバー全体の会話コンテキストを共有しながらプログラミング学習を支援するRust製Botです。

## 現在の設計

- 対象サーバーは必須の `DISCORD_GUILD_ID` で固定
- OpenRouterのChat Completions APIを使用し、既定モデルは `openai/gpt-5.6-luna`
- チャンネル単位で `off`、`read_only`、`mention`、`auto` を設定
- 生成後にDiscord向け静的検査とAIレビューを明示的に実施
- SQLiteにサーバー全体の会話を保存し、別チャンネルの会話も応答コンテキストに利用
- Discordの文字数制限を超える応答はRust側で分割
- スラッシュコマンドの応答は公開（エフェメラル無効）

実行方法とDiscord Developer Portalの設定は、実装完了後に更新します。
