# 3D 地図 API (`/map3d/v1`)

TORO Server の 3D 地図 (BlueMap) の画面が使う API です。3D 地図のデータは BlueMap の保存先 (データベースの `bluemap`) と、
Minecraft サーバー内蔵の BlueMap のサーバーにあり、この API だけがそれを読みます。`MAP_BLUEMAP_DATABASE` を設定しなければ、
この機能は動かず、`/map3d/v1/*` は 404 を返します。

## 経路

| 経路 (`/map3d/v1` 以下) | 内容 |
| --- | --- |
| `maps/{地図}/settings.json` | 地図の設定 (ETag、`If-None-Match` で 304、`no-cache`) |
| `maps/{地図}/textures.json` | テクスチャ (保存済みの gzip をそのまま返す) |
| `maps/{地図}/tiles/...` | 高精細 (`.prbm`) と低精細 (PNG) のタイル。描かれていない場所は 204 |
| `maps/{地図}/live/players.json` | プレイヤーの位置 (BlueMap のサーバーを優先し、つながらなければデータベースの最後のバージョン) |
| `maps/{地図}/live/markers.json` | マーカー (BlueMap のものに Dynmap の地点を足す) |
| `maps/{地図}/live/sse` | タイル・プレイヤー・マーカーの更新 (Server-Sent Events) |
| `maps/{地図}/assets/playerheads/{uuid}.png` | プレイヤーの顔 |
| `ws` | WebSocket。`{"bluemap":"<地図の ID>"}` を送ると、その地図の更新を受け取る。30 秒ごとに `{"type":"ping"}` |
| `marumasign/{地図}.json` | 看板のデータ (毎回取り直す) |
| `marumasign/resolve`、`marumasign/proxy` | 看板の URL の転送先の確認と、別のサイトの画像・動画などの中継 |
| `mcassets/...` | 構造物の看板を描くための Minecraft のリソース (`index.json`、`{hash}/assets.json`、`textures.bin`、`unifont/{XXXX}.txt`、`skin`) |

ほかに、看板の画像を返す `/_assets/{id}.{拡張子}` と、接続中のプレイヤーの IP アドレスを受け取る `POST /live/clients` があります。
`/live/clients` は内部専用の待受 (`/live/plugin` と同じ) だけで受け付け、受け取った IP アドレスはメモリ上にだけ置き、ログにもデータベースにも残しません。

すべての応答 (204、304、4xx、5xx を含む) に `Access-Control-Allow-Origin` を付けます。`OPTIONS` は `Range` を許可し、
`Access-Control-Expose-Headers` に `X-MarumaSign-Url, X-MarumaSign-Type, X-MarumaSign-Kind, Accept-Ranges, Content-Range, Content-Length` を入れます。

## 看板の中継の安全規則

看板の URL は誰でも書けるので、中継がただの中継 (オープンプロキシ) にならないようにしています。

- 読むのは、看板のデータにいま書かれている URL、その転送先、それらの再生リスト (HLS、MPEG-DASH) と glTF に書かれた URL だけです。
- https だけです。名前を引いたアドレスが、サーバーの中・家の中・共有アドレス・リンクローカルなどなら読みません。転送は 5 回まで追い、行き先ごとに確かめ直します。
- 中身は先頭のバイト列で種類を決め、HTML は返しません。大きさは種類ごとに上限があります。
- 画面 (`3d-map.torosaba.net`) と API (`api.torosaba.net`) は別のオリジン (同じサイト) です。`Sec-Fetch-Site: same-site` は、
  `Origin` が許可一覧 (`torosaba.net` とそのサブドメイン、`localhost`、`127.0.0.1`) にあるときだけ許可します。
  `Cross-Origin-Resource-Policy` は `same-site` です。
- 1 つの IP アドレスからの数と量、全体の量、同時に読む数に上限があります。同時に読む数は、このサービス全体の同時処理の枠とは別に数えます。
- 元のサイトが圧縮して返したときは、展開せずに 502 を返します。

## 環境変数

名前だけを書きます。値は配置の規則に従って設定します。

| 名前 | 内容 |
| --- | --- |
| `MAP_BLUEMAP_DATABASE` | BlueMap のデータベースの名前。無ければ 3D 地図 API は動かさない |
| `MAP_BLUEMAP_LIVE` | BlueMap 内蔵のサーバーの URL |
| `MAP_BLUEMAP_DYNMAP_WORLDS` | マーカーに地点を足す Dynmap のワールド (`地図=ワールド,...`) |
| `MAP_ASSETS_DATABASE` | 看板の画像を持つデータベースの名前。無ければ `/_assets/` は動かさない |
| `MAP_ASSETS_ORIGINS` | `/_assets/` を読んでよい地図のページの Origin (`,` 区切り) |
| `MAP_ASSETS_GRACE_MINUTES` | ログアウトしてからも、その IP アドレスから `/_assets/` を読める時間 (分) |
| `MAP_CLIENTS_TOKEN` | `/live/clients` に送るときの合言葉。空なら受け付けない |
| `MAP_MCASSETS_JAR` | Minecraft クライアントの jar か、`minecraft-client-<バージョン>.jar` があるフォルダ |
| `MAP_MCASSETS_CACHE` | Unifont を保存する、書き込める場所 (無ければメモリだけ) |
| `MAP_MCASSETS_RESOURCES_URL`、`MAP_MCASSETS_MOJANG_API_URL`、`MAP_MCASSETS_MOJANG_SERVICES_URL`、`MAP_MCASSETS_MOJANG_SESSION_URL` | Mojang への問い合わせ先 (試験で差し替えるためのもの) |
| `MAP_MARUMASIGN_ENABLED` | `false` なら看板の中継を動かさない |
| `MAP_MARUMASIGN_MAPS` | 看板のデータを読む地図の ID (`,` 区切り。空なら BlueMap の地図の一覧) |
| `MAP_MARUMASIGN_REFRESH_SECONDS` | 看板のデータを読み直す間隔 (秒) |
| `MAP_MARUMASIGN_CORS_ORIGIN` | 転送先の確認で、CORS を確かめるときに名乗る Origin |

## メモリ

キャッシュの上限は `src/map3d/budget.rs` に集めています。看板中継の本文のキャッシュ (96 MiB) と、`/_assets/` のかたまりのキャッシュ (160 MiB) の合計は、
既定で 256 MiB 以下です。mcassets は、1 つのバージョンあたり 10 MB 前後を常駐させ、新旧の 2 つまで持ちます。配備のときに、コンテナのメモリの上限を見直してください。

## 看板の共有ロジックの試験

看板の URL の確かめ方や、中身の種類の判別は、画面側 (TORO-3D-WebMap) の TypeScript が正本です。Rust 側の移植が同じ結果を返すことは、
[試験ベクタ](sign-vectors.md) で確かめています。
