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
| 2D 地図の HTTP (`/map/v1/configuration` `/world/...` `/tiles/...`) | `src/map/api.rs` | `tests/map.rs` |
| 2D 地図の WebSocket (`/map/v1/ws`) と配信ハブ | `src/map/ws.rs` `src/map/hub.rs` | `tests/map_ws.rs` `tests/map_hub.rs` |
| dynmap-live からの位置の受信 (`/live/plugin`。内部専用) | `src/map/live.rs` | `tests/map_ws.rs` |
| Dynmap の DB の読み取り (SELECT だけ) | `src/map/store.rs` | `tests/common/fake_map.rs` (偽の DB) |
| 移管前の実装 (JavaScript) と同じ結果にする道具 | `src/map/js.rs` | 同じファイル内 |

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

## 地図 API (`src/map/`)

2D 地図の画面 (map.torosaba.net) が読む API です。移管前は toro-webmap (TORO-WebMap のサーバー側) が返していて、応答は同じです (ヘッダー、ETag、本文、異常系のクセまで)。違うのは、未知のワールドの 404 の本文が HTML でなく JSON であることと、このサービス共通の `X-Content-Type-Options: nosniff` が付くことだけです。

- **Dynmap の DB を読む**: TiDB の `dynmap` データベースを読みます (地図の表示のための読み取り)。**発行するのは SELECT だけ**で、文は `src/map/store.rs` の定数に固定してあります (値は必ず bind)。接続は `MAP_DB_*` で決め、`MAP_DB_HOST` が無ければ地図 API は動きません (`/map/v1/*` は 404)。本番では読み取り専用の専用利用者を使います
- **ドライバは sqlx** (`mysql` と `runtime-tokio` だけ。TLS なし)。純 Rust で、OpenSSL も zlib も依存に入らないので、musl の静的リンク (scratch イメージ) のままビルドできます。`mysql_async` は zlib (`libz-sys`) の C のビルドを引くので選びませんでした。接続は最初の問い合わせまで張らず、プールは 16 本、1 つの問い合わせは 30 秒で切ります
- **キャッシュ**: HTTP の各ハンドラーは毎回 DB を引きます。HTTP のキャッシュは前段の toro-edge が担います。WebSocket のハブだけが、ワールドごとに 1 秒に 1 度だけ DB を読み、`timestamp` が変わったときだけ全員へ送ります (ワールドの一覧は 30 秒ごとに読み直し、知らない名前では DB を読みません)
- **公開の待受と内部専用の待受を分ける**: 公開の待受 (`PORT`。Tunnel の先) は `/map/v1/*` だけを受け、`/live/*` は 404 です。dynmap-live からの位置 (`/live/plugin`) は、`MAP_LIVE_ADDR` で決めた内部専用の待受でだけ受けます (決めなければ待受を起動しません)。`X-Toro-Token` を SHA-256 にして定数時間で比べ、`MAP_LIVE_TOKEN` が空ならどんな値も通しません。この待受を Tunnel の規則に載せないでください
- **WebSocket は、ボディを読む前に分ける**: ふつうの要求は本文を全部読む作りなので、`/map/v1/ws` の Upgrade は `src/routes.rs` の入り口で先に分けます。同時処理の枠 (256) は使わず、接続数は別に数えます (ブラウザ 2048、`/live/plugin` 8)。1 つのメッセージは 256 KiB まで、ブラウザからの購読は 512 文字までで、2 秒に 5 通を超えたものは無視します。Origin は空、`localhost`、`127.0.0.1`、`*.torosaba.net` だけを許し、他は接続のあとで 1008 で閉じます
- **3D (BlueMap) は含みません**: `/map/v1/ws` の `{"bluemap":"<map>"}` は 3D 用で、この段階では未対応です (黙って無視します)。3D の動的な側は、あとの段階で `src/map/` の隣に足します
- **JavaScript と同じ結果にする道具** (`src/map/js.rs`): `Number()`、`decodeURI`、`decodeURIComponent`、`Date#toUTCString`、`JSON.stringify` の数の書き方を、移管前と同じに再現しています (パスの数字やワールド名の解き方で、同じ入力が同じ応答になるように)

### 安全のための決まり (地図 API)

- タイルや顔のパスは、`..` を含むものと、壊れた `%` を含むものを blank.png にします (500 にしない)
- 認証は無い API です (公開の地図の読み取り)。CORS は `Access-Control-Allow-Origin: *`
- DB の書き込みは、このサービスから行いません。DB の利用者の権限も SELECT だけにしてください
