# toro-api

TORO Server の REST API のサーバーです。Rust 製で、TORO のサーバーの上で動きます。

役割は「データを読み書きする」ことだけです。ログインとトークンの発行は auth (`TORO-Auth`)、AI クライアントの接続先は mcp (`TORO-MCP`) が受け持ちます。

## 窓口

| パス | 認証 | できること |
| --- | --- | --- |
| `GET /` | なし | 各サービスの URL の一覧 (プログラムはこれを読む) |
| `GET /me` | Bearer (任意) | トークンの持ち主 |
| `GET /docs/raw?path=` | なし | Wiki の原稿の取得 (`format=raw` で Markdown だけ) |
| `GET /docs/list?category=` | なし | `docs/` の下の原稿のパスの一覧 |
| `GET /docs/search?q=` | なし | 原稿の全文検索 (語をすべて含むものを、よく一致する順に)。512 バイト、異なる語 16 個まで |
| `POST /docs/save` | Bearer (編集権限) | 原稿の保存 (GitHub へのコミット) |
| `GET /server/status` | なし | Minecraft サーバーの稼働状態 (10 秒だけ覚える) |
| `GET /openapi.json` | なし | OpenAPI 仕様 (ChatGPT Actions / Gemini 向け) |
| `GET /healthz` | なし | 稼働確認 |

どのホスト名で受けても同じ答えを返します。公開の URL は `https://api.torosaba.net/` です。

**URL の正本は 1 か所です。** 別の場所に書き写さないでください。

- コード: `Config` (`src/config.rs`) だけが URL を持ち、API の応答もここから作ります (`src/site.rs`)
- プログラム向け: `https://api.torosaba.net/` の JSON
- 人向け: Wiki の「サービスのURLと役割」のページ

## つなぎ方

```
ブラウザ / AI クライアント → Cloudflare → cloudflared ─→ toro-api:8080 ─┬─ toro-auth:8081 (トークンの確認)
                                                                         ├─ GitHub API (Wiki の原稿)
                                                                         └─ toro-velocity:25565 (稼働状態)
```

- `AUTH_INTERNAL_URL` (既定は `http://toro-auth:8081`) に届くことが必要です。auth の内部の口で、トンネルには載せません
- データベースもディスクも使いません。持つのは、メモリの中の Wiki の全文検索の索引だけです

## 認証

このサービスは JWT の署名を確かめません (鍵を持ちません)。`Authorization: Bearer` のトークンを auth の `POST /introspect` に渡し、有効かどうかと中身 (claims) を教えてもらいます。90 日トークンの再作成による失効も auth が判断するので、ここでも通りません。

| トークン | 結果 |
| --- | --- |
| なし | 読み取りは通る。`/docs/save` は 401。`/me` は `authenticated: false` |
| auth が無効と答えた | `/docs/save` は 401。`/me` は `authenticated: false` |
| auth に尋ねられなかった (停止、タイムアウト 3 秒) | `/docs/save` と `/me` は 503。無効だとは決めつけない |

読み取り (`/docs/raw` `/docs/list` `/docs/search` `/server/status`) は auth を使いません。auth が止まっていても動きます。

## 機能ごとの置き場所

| 機能 | ソース | 試験 |
| --- | --- | --- |
| Wiki の原稿の読み書きと一覧 | `src/wiki/api.rs` `src/wiki/github.rs` `src/wiki/path.rs` `src/wiki/content.rs` | `tests/wiki.rs` |
| Wiki の全文検索 | `src/wiki/search.rs` | `tests/search.rs` |
| Minecraft の稼働状態 | `src/minecraft.rs` | 同じファイル内、`tests/service.rs` |
| トークンの確認 | `src/identity.rs` | `tests/service.rs` |
| OpenAPI 仕様 | `src/openapi.rs` `assets/openapi/` | `tests/service.rs` |
| 入り口、URL の一覧 | `src/api.rs` `src/site.rs` | `tests/service.rs` |

共通の部品: `src/routes.rs` (URL の振り分け)、`src/http.rs` (応答と CORS)、`src/config.rs`。

- URL を足す: 機能のモジュールにハンドラーを書き、`src/routes.rs` に 1 行足し、`assets/openapi/openapi.json` に仕様を足す

## 開発

```bash
cargo test
```

`tests/common/mod.rs` が偽の auth と GitHub を立てるので、試験に秘密やネットワークは要りません。

手元で動かすときは `GITHUB_TOKEN` を環境変数に入れ、`AUTH_INTERNAL_URL` に届く auth を用意して `cargo run` します。

## 設定

`.env.example` を見てください。秘密でない値の既定は本番の値なので、本番の env ファイルに書くのは秘密 (`GITHUB_TOKEN`) だけです。

## イメージ

GitHub Actions が、試験に通ったものだけを GHCR (`ghcr.io/toro-server/toro-api`) に置きます (linux/amd64)。タグ `v*` を打つとバージョンのタグが付きます。

## セキュリティの考え方

- **API は Bearer だけ**。Cookie は読まないので、他のサイトからの CSRF は成り立たない
- **CORS で Cookie 付きを許すのは、信頼する Origin だけ** (`CORS_ORIGINS` と auth)。他は `*` で、資格情報は付かない
- **トークンの検証は auth に任せる**。このサービスが侵害されても、トークンは偽造できない (署名の鍵を持たない)
- **auth の内部の口は公開しない**。`AUTH_INTERNAL_URL` の先をトンネルやポートの公開に載せない
- **Wiki のパスは `docs/` の下の `.md` だけ**。どの入口でも `..` や `\` を断る (`src/wiki/path.rs`)
- **保存できるのは Wiki のページだけ**。`.md` 以外の拡張子、ビルドが別に扱うもの (`docs/README.md`、`attachments`、`.` で始まるもの)、手元の Windows で取り出せない名前、見えない文字を含む名前は断る
- **保存する原稿の front matter は YAML だけ** (`src/wiki/content.rs`)。開きの `---` の後ろに他の言語を書いたものは断る
- **コミットメッセージは 1 行目の 200 文字まで**。だれの編集かを示す行を足させない
- **Wiki の変更には `can_edit` と、scope の `write` か `mcp` が要る**。`can_edit` は auth がいまのロールで決めた値
- **検索は 512 バイト、異なる語 16 個まで**。点数はロックの外で、同時に 2 つまで付ける
- **だれでも読める窓口には、GitHub の答えをそのまま見せない**。ログにだけ残す
- **遅い送信で同時処理の枠を埋めさせない**。ボディは 30 秒で読み切れなければ 408 で切る。稼働確認 (`/healthz`) は、この枠を使わない

### 知ったうえで残しているもの

- 匿名の Wiki の読み取りは、サーバーの GitHub トークンの枠を使う。Cloudflare のレート制限で守る
- 検索の索引はメモリの中だけ。再起動すると、最初の検索で原稿を読み直す
