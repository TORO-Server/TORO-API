# toro-api

TORO Server の REST API のサーバーです。Rust で書かれています。

役割は「データを読み書きする」ことだけです。ログインとトークンの発行は [TORO-Auth](https://github.com/TORO-Server/TORO-Auth)、AI クライアントの接続先は [TORO-MCP](https://github.com/TORO-Server/TORO-MCP) が受け持ちます。

## 使えるもの

- Wiki の原稿の読み取り・一覧・全文検索・保存
- Minecraft サーバーの稼働状態
- 各サービスの URL の一覧

パス・認証の要否・パラメーターの正本は [assets/openapi/openapi.json](assets/openapi/openapi.json) です (動いているサーバーの `GET /openapi.json` でも同じものが返ります)。

## 動かしてみる

```bash
cargo test   # 試験。偽の認証サービスと偽の GitHub を自動で立てるので、秘密もネットワークも要りません
cargo run    # 動かす。環境変数 GITHUB_TOKEN と AUTH_INTERNAL_URL が要ります
```

設定の名前と既定値の正本は [.env.example](.env.example) です。秘密の値 (`GITHUB_TOKEN`) だけを、置き場の外の環境変数に書きます。

## 関連文書

- 認証のしくみ、機能ごとのソースの場所、安全のための決まり: [docs/design.md](docs/design.md)
- サービス全体の構成: [ServerImage-TORO の docs/architecture.md](https://github.com/TORO-Server/ServerImage-TORO/blob/main/docs/architecture.md)
- 文章の書き方: [WRITING-STYLE.md](https://github.com/TORO-Server/TORO-Website/blob/main/docs/WRITING-STYLE.md)
