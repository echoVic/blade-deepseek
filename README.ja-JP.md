# Orca

ターミナル向けの DeepSeek ネイティブなコーディングエージェントです。

Orca にタスクを渡すと、コードの読み取り、ファイルの編集、コマンドの実行、
結果の検証を行い、完了するか判断が必要になるまで作業を続けます。
対話的な作業には TUI、スクリプトや CI には `orca exec` を使用できます。
Orca は Rust 製で、ローカルで動作し、MIT ライセンスで提供されます。

[English](README.md) · [简体中文](README.zh-CN.md) · [日本語](README.ja-JP.md) · [Tiếng Việt](README.vi.md) · [한국어](README.ko-KR.md) · [Español](README.es-419.md) · [Português](README.pt-BR.md)

[Web サイト](https://orcaagent.dev/) · [変更履歴](https://orcaagent.dev/changelog/) · [リリース](https://github.com/echoVic/blade-deepseek/releases/latest) · [npm](https://www.npmjs.com/package/@blade-ai/orca)

## インストール

```bash
npm install -g @blade-ai/orca
```

ネイティブバイナリを直接インストールすることもできます。

```bash
curl -fsSL https://orcaagent.dev/install.sh | sh
```

npm パッケージは macOS と Linux の ARM64 / x64 に対応しています。
ビルド済みアーカイブは [GitHub Releases](https://github.com/echoVic/blade-deepseek/releases/latest) からも入手できます。

## 使い方

```bash
export DEEPSEEK_API_KEY=sk-...

orca                                      # TUI を開く
orca exec "失敗しているテストを修正"        # ヘッドレスで実行
orca exec --verifier "cargo test" "修正する" # 完了前に検証
orca --mode=acp                           # ACP クライアントを接続
```

TUI では `@` でファイル、Skills、Plugins、MCP Resources を検索できます。
`/plan` は読み取り専用の計画、`/goal` は永続的な目標、`/workflows` は
バックグラウンド作業、`/trust` は現在のフォルダーのサンドボックス権限を管理します。

## 主な機能

- DeepSeek の推論とツール利用のセマンティクスに直接対応し、SSE ストリーミング、
  プレフィックスキャッシュに適したプロンプト、自動コンテキスト管理、再試行を提供します。
- コードの読み取り、検索、編集、書き込み、シェルコマンドの実行、指定コマンドでの検証を行います。
- `suggest`、サンドボックス内の `auto-edit`、フルアクセスの `full-auto`、
  読み取り専用の `plan` とフォルダー単位の信頼設定でリスクを制御します。
- ローカルの会話履歴を保存し、再開、フォーク、検索、名前変更、アーカイブ、圧縮に対応します。
- 固定ターン上限のない永続的な目標、サブエージェント、JavaScript ワークフローで長時間のタスクを処理します。
- 信頼済みワークスペースから指示、Skills、Plugins、カスタムツール、MCP ツールとリソースを読み込みます。
- エディター、ハーネス、CI 向けに安定した JSONL、app-server、Agent Client
  Protocol（ACP）の契約を提供します。

設定の優先順位は、環境変数、CLI 引数、設定ファイル、既定値です。
完全なコマンド一覧は `orca --help` または `orca exec --help` で確認できます。
ユーザー設定は `~/.orca/config.toml` にあり、信頼済みプロジェクトでは
`.orca/config.toml`、`AGENTS.md`、ルール、Skills、ワークフローも利用できます。

詳細:

- [Persistent Goal Mode](docs/goal-mode.md)
- [Harness と app-server の契約](docs/harness-contract.md)
- [動的ワークフロー設計](docs/claude-code-workflow-parity.md)
- [プロダクションロードマップ](docs/production-roadmap.md)

## コミュニティ

- QQ グループ: `472309526`
- [Telegram](https://t.me/+11No1w5ZbTMyZTQ1)

## コントリビューション

コントリビューションの前に [CONTRIBUTING.md](CONTRIBUTING.md) をお読みください。
大規模または互換性に影響する変更は、先に Issue を作成してください。

- [バグを報告](https://github.com/echoVic/blade-deepseek/issues/new?template=bug_report.yml)
- [機能を提案](https://github.com/echoVic/blade-deepseek/issues/new?template=feature_request.yml)
- [サポートを受ける](SUPPORT.md)
- [脆弱性を報告](SECURITY.md)

## ライセンス

[MIT](LICENSE)
