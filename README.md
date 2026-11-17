# toro-api

TORO Server の REST API (他のプログラムとデータをやり取りするための決まり) のサーバーです。Rust で書かれています。

役割は「データを読み書きする」ことだけです。ログインとトークン (本人確認済みの印になる文字列) の発行は [TORO-Auth](https://github.com/TORO-Server/TORO-Auth)、AI クライアント (AI を使うアプリ) の接続先は [TORO-MCP](https://github.com/TORO-Server/TORO-MCP) が受け持ちます。

## 使えるもの

- Wiki の原稿の読み取り・一覧・全文検索・保存
- Minecraft サーバーの稼働状態
- 2D 地図の API (`/map/v1/*`。Dynmap のデータベースを読んで、地図の画面 map.torosaba.net に返す。WebSocket (つなぎっぱなしで通信する方式) の `/map/v1/ws` を含む)
- 各サービスの URL の一覧

パス・認証の要否・パラメーターの正本は [assets/openapi/openapi.json](assets/openapi/openapi.json) です (動いているサーバーの `GET /openapi.json` でも同じものが返ります)。

## 動かしてみる

```bash
cargo test   # 試験。偽の認証サービスと偽の GitHub を自動で立てるので、秘密もネットワークも要りません
cargo run    # 動かす。環境変数 GITHUB_TOKEN、AUTH_INTERNAL_URL、MC_STATUS_ADDR が要ります
```

設定の名前と既定値の正本は [.env.example](.env.example) です。秘密の値 (`GITHUB_TOKEN`) だけを、置き場の外の環境変数に書きます。

## 関連文書

- 認証のしくみ、機能ごとのソースの場所、安全のための決まり: [docs/design.md](docs/design.md)
