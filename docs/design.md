# 設計メモ

利用者向けの概要は [README.md](../README.md) にあります。この文書は、認証のしくみ、機能ごとのソースの場所、安全のための決まりをまとめた、ただ 1 つの詳しい文書です。
各項目の数値は、コードと突き合わせて確かめたものです (確かめた場所は各項目の `src/` の名前)。

## URL の正本

URL の正本は 1 か所です。別の場所に書き写さないでください。

- コード: `Config` (`src/config.rs`) だけが URL を持ち、API の応答もここから作ります (`src/site.rs`)
- プログラム向け: 動いているサーバーの `GET /` の JSON
- 人向け: Wiki の「サービスのURLと役割」のページ

## 認証

このサービスは JWT の署名を確かめません (鍵を持ちません)。`Authorization: Bearer` のトークンを TORO-Auth の `POST /introspect` に渡し、有効かどうかと中身 (claims) を教えてもらいます。トークンを作り直したときの失効も TORO-Auth が判断するので、ここでも通りません。TORO-Auth に尋ねるときの待ち時間の上限は 3 秒です (`src/identity.rs`)。

| トークン | 結果 |
| --- | --- |
| なし | 読み取りは通る。`/docs/save` は 401。`/me` は `authenticated: false` |
| TORO-Auth が無効と答えた | `/docs/save` は 401。`/me` は `authenticated: false` |
| TORO-Auth に尋ねられなかった (停止、待ち時間の超過) | `/docs/save` と `/me` は 503。無効だとは決めつけない |

読み取り (`/docs/raw` `/docs/list` `/docs/search` `/server/status`) は TORO-Auth を使いません。TORO-Auth が止まっていても動きます。

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

URL を足すには、機能のモジュールにハンドラーを書き、`src/routes.rs` に 1 行足し、`assets/openapi/openapi.json` に仕様を足します。

## 安全のための決まり

- **API は Bearer だけ**。Cookie は読まないので、他のサイトからの CSRF は成り立たない
- **CORS で Cookie 付きを許すのは、信頼する Origin だけ** (`CORS_ORIGINS` と TORO-Auth)。他は `*` で、資格情報は付かない
- **トークンの検証は TORO-Auth に任せる**。このサービスが侵害されても、トークンは偽造できない (署名の鍵を持たない)
- **TORO-Auth の内部向けの口は公開しない**。`AUTH_INTERNAL_URL` の先を、外から届く経路に載せない
- **Wiki のパスは `docs/` の下の `.md` だけ**。どの入口でも `..` や `\` を断る (`src/wiki/path.rs`)
- **保存できるのは Wiki のページだけ**。`.md` 以外の拡張子、ビルドが別に扱うもの (`docs/README.md`、`attachments`、`.` で始まるもの)、手元の Windows で取り出せない名前、見えない文字を含む名前は断る
- **保存する原稿の front matter は YAML だけ** (`src/wiki/content.rs`)。開きの `---` の後ろに他の言語を書いたものは断る
- **コミットメッセージは 1 行目の 200 文字まで**。だれの編集かを示す行を足させない
- **Wiki の変更には `can_edit` と、scope の `write` か `mcp` が要る** (`src/identity.rs`)。`can_edit` は TORO-Auth がいまのロールで決めた値
- **検索は 512 バイト、異なる語 16 個まで** (`src/wiki/search.rs`)。点数はロックの外で、同時に 2 つまで付ける
- **だれでも読める API には、GitHub の答えをそのまま見せない**。ログにだけ残す
- **遅い送信で同時処理の枠を埋めさせない**。ボディは 30 秒で読み切れなければ 408 で切る (`src/routes.rs`、秒数は `src/config.rs`)。稼働確認 (`/healthz`) は、この枠を使わない

### 知ったうえで残しているもの

- 匿名の Wiki の読み取りは、サーバーの GitHub トークンの枠を使う。レート制限で守る
- 検索の索引はメモリの中だけ。再起動すると、最初の検索で原稿を読み直す
