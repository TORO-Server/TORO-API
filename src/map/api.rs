//! `/map/v1/*` の HTTP (configuration、world、tiles)。移管前の `server/utils/dynmap-api.ts` と同じ応答を返す。
//!
//! 共通: 認証なし。CORS は `Access-Control-Allow-Origin: *`。ETag は本文の MD5 (`"<hex>"`)。
//! `If-None-Match` が ETag と**まったく同じ文字列**のときだけ 304 (旧実装のとおり。`W/` や複数の値は解かない)

use axum::body::Body;
use axum::http::header::{self, HeaderMap, HeaderValue};
use axum::http::{Method, StatusCode};
use axum::response::Response;
use md5::{Digest, Md5};
use serde_json::json;

use super::MapService;
use super::js;
use super::store::{StoreError, TileKey};
use crate::http::Ctx;

/// 描いていないタイルの代わり (Dynmap の `web/images/blank.png`。167 バイト)
const BLANK_PNG: &[u8] = include_bytes!("../../assets/map/blank.png");
const JSON_UTF8: &str = "application/json; charset=utf-8";
const TILE_TYPES: [&str; 3] = ["image/png", "image/jpeg", "image/webp"];

/// 返す本文
struct Reply {
    data: Vec<u8>,
    content_type: &'static str,
    cache: &'static str,
    etag: Option<String>,
    /// `Last-Modified` (エポックからのミリ秒)
    last_modified: Option<i64>,
}

impl Reply {
    fn new(data: impl Into<Vec<u8>>, content_type: &'static str, cache: &'static str) -> Self {
        Self {
            data: data.into(),
            content_type,
            cache,
            etag: None,
            last_modified: None,
        }
    }
}

fn md5_etag(data: &[u8]) -> String {
    let digest = Md5::digest(data);
    let mut etag = String::with_capacity(34);
    etag.push('"');
    for byte in digest {
        etag.push_str(&format!("{byte:02x}"));
    }
    etag.push('"');
    etag
}

/// 本文を返す。`If-None-Match` が ETag と同じなら 304
fn respond(ctx: &Ctx, body: Reply) -> Response {
    let etag = body.etag.unwrap_or_else(|| md5_etag(&body.data));
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(body.content_type),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(body.cache));
    if let Ok(value) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, value);
    }
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    if let Some(ms) = body.last_modified
        && let Ok(value) = HeaderValue::from_str(&js::utc_string(ms))
    {
        headers.insert(header::LAST_MODIFIED, value);
    }
    let mut res = Response::new(Body::empty());
    *res.headers_mut() = headers;
    if ctx.header(header::IF_NONE_MATCH) == etag {
        *res.status_mut() = StatusCode::NOT_MODIFIED;
    } else {
        *res.body_mut() = Body::from(body.data);
    }
    res
}

/// 地図 API のエラー。旧実装 (Nitro) は CORS のヘッダーを付けず、`Cache-Control: no-cache` で返していた。
/// 本文だけは、HTML から JSON に変わる (許容した差異)
pub fn error(status: u16, message: &str) -> Response {
    let mut res = Response::new(Body::from(json!({ "error": message }).to_string()));
    *res.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let headers = res.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    // 旧実装 (Nitro) のエラー応答が付けていたもの
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("script-src 'none'; frame-ancestors 'none';"),
    );
    res
}

fn failed(e: &StoreError) -> Response {
    eprintln!("map: database error: {e}");
    error(500, "Internal Server Error")
}

fn blank(ctx: &Ctx) -> Response {
    respond(
        ctx,
        Reply::new(BLANK_PNG, "image/png", "public, max-age=60"),
    )
}

fn json_body(text: String, cache: &'static str) -> Reply {
    Reply::new(text.into_bytes(), JSON_UTF8, cache)
}

/// `/map/v1/` 以下の振り分け。`ctx.path` は、デコード前のパス
pub async fn handle(svc: &MapService, ctx: &Ctx) -> Response {
    if ctx.method != Method::GET && ctx.method != Method::HEAD {
        let mut res = error(405, "Method Not Allowed");
        res.headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        return res;
    }
    let rest = ctx.path.strip_prefix("/map/v1/").unwrap_or("");
    // ルーターは末尾の / を 1 つだけ無視する
    let trimmed = rest.strip_suffix('/').unwrap_or(rest);

    if trimmed == "configuration" {
        return configuration(svc, ctx).await;
    }
    if trimmed == "ws" {
        // WebSocket の Upgrade は別の経路 (ws.rs)。ここに来るのは、通常の GET
        let mut res = error(426, "Upgrade Required");
        res.headers_mut()
            .insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        return res;
    }
    if let Some(world_and_ts) = trimmed.strip_prefix("world/") {
        // 旧実装は、パス全体を `decodeURI` してからワールド名を取り出す (壊れていれば、そのまま)
        let decoded = js::decode_uri(world_and_ts);
        let source = decoded.as_deref().unwrap_or(world_and_ts);
        let mut parts = source.split('/');
        if let (Some(world), Some(_ts), None) = (parts.next(), parts.next(), parts.next()) {
            return world_update(svc, ctx, world).await;
        }
    }
    if let Some(path) = trimmed.strip_prefix("tiles/")
        && !path.is_empty()
    {
        return tiles(svc, ctx, path).await;
    }
    error(404, "Not Found")
}

/// `/map/v1/configuration`: Dynmap のブラウザ側が最初に読む設定
/// (ログインは使っていないので、書き出されたものをそのまま返す)
async fn configuration(svc: &MapService, ctx: &Ctx) -> Response {
    match svc.store.standalone_file("dynmap_config.json").await {
        Ok(Some(config)) => respond(ctx, json_body(config, "no-cache")),
        Ok(None) => error(503, "Dynmap has not written its configuration yet"),
        Err(e) => failed(&e),
    }
}

/// `/map/v1/world/<world>/<ts>`: Dynmap のブラウザ側が 2 秒ごとに読むワールドの更新
/// (プレイヤーの位置、描き直したタイルなど)。中身は時刻によらず同じ。`<ts>` は見ない
async fn world_update(svc: &MapService, ctx: &Ctx, world: &str) -> Response {
    if world.contains('/') || world.contains('\\') {
        return respond(
            ctx,
            json_body(r#"{ "error": "invalid-world" }"#.into(), "no-cache"),
        );
    }
    // 知らないワールドの名前で DB を読まない (一覧が読めないときは、これまでどおり読みに行く)
    if let Some(names) = svc.hub.names_for(world).await
        && !names.contains(world)
    {
        return error(404, "Not Found");
    }
    match svc
        .store
        .standalone_file(&format!("dynmap_{world}.json"))
        .await
    {
        Ok(Some(update)) => respond(ctx, json_body(update, "no-cache")),
        Ok(None) => error(503, "Database unavailable"),
        Err(e) => failed(&e),
    }
}

/// `/map/v1/tiles/...`: Dynmap のタイル置き場と同じ形のパス
///
/// - `<world>/<prefix>/<x_y>/<zoom_x_y>.<ext>`: 地図のタイル
/// - `_markers_/marker_<world>.json`: マーカー
/// - `_markers_/<icon>.png`: マーカーのアイコン
/// - `faces/<8x8|16x16|32x32|body>/<name>.png`: プレイヤーの顔
async fn tiles(svc: &MapService, ctx: &Ctx, raw_path: &str) -> Response {
    // 壊れたエンコードや `..` を含むパスは、500 にせず blank.png
    let Some(path) = js::decode_uri_component(raw_path) else {
        return blank(ctx);
    };
    if path.contains("..") {
        return blank(ctx);
    }
    let parts: Vec<&str> = path.split('/').collect();

    if parts[0] == "_markers_" && parts.len() == 2 {
        let file = parts[1];
        if let Some(world) = marker_world(file) {
            return match svc.store.marker_file(world).await {
                Ok(content) => respond(
                    ctx,
                    Reply::new(
                        content.unwrap_or_else(|| "{ }".into()).into_bytes(),
                        "application/json",
                        "public, max-age=30",
                    ),
                ),
                Err(e) => failed(&e),
            };
        }
        return match svc.store.marker_icon(strip_extension(file)).await {
            Ok(Some(image)) => respond(ctx, Reply::new(image, "image/png", "public, max-age=3600")),
            Ok(None) => blank(ctx),
            Err(e) => failed(&e),
        };
    }

    if parts[0] == "faces" && parts.len() == 3 {
        let type_id = match parts[1] {
            "8x8" => 0,
            "16x16" => 1,
            "32x32" => 2,
            "body" => 3,
            _ => 0,
        };
        // 名前は最初の `.` まで (`.` を含む名前は切れる)
        let name = parts[2].split('.').next().unwrap_or("");
        return match svc.store.face(name, type_id).await {
            Ok(Some(image)) => respond(ctx, Reply::new(image, "image/png", "public, max-age=3600")),
            Ok(None) => blank(ctx),
            Err(e) => failed(&e),
        };
    }

    if parts.len() != 4 {
        return blank(ctx);
    }
    let (world, raw_prefix, file) = (parts[0], parts[1], parts[3]);
    let day = raw_prefix.ends_with("_day") && raw_prefix.len() > 4;
    let prefix = if day {
        &raw_prefix[..raw_prefix.len() - 4]
    } else {
        raw_prefix
    };
    // zz_x_y (z の数が縮小の段) または x_y
    let stem = file.split('.').next().unwrap_or("");
    let f: Vec<&str> = stem.split('_').collect();
    if f.len() != 2 && f.len() != 3 {
        return blank(ctx);
    }
    let zoom = if f.len() == 3 {
        js::utf16_len(f[0]) as i64
    } else {
        0
    };
    let (Some(x), Some(y)) = (
        js::safe_integer(js::number(f[f.len() - 2])),
        js::safe_integer(js::number(f[f.len() - 1])),
    ) else {
        return blank(ctx);
    };

    let key = TileKey {
        world,
        map: prefix,
        variant: if day { "DAY" } else { "STANDARD" },
        x,
        y,
        zoom,
    };
    let tile = match svc.store.tile(key).await {
        Ok(tile) => tile,
        Err(e) => return failed(&e),
    };
    let Some((tile, image)) = tile.and_then(|t| t.image.clone().map(|image| (t, image))) else {
        return blank(ctx);
    };
    // ?timestamp= が付いた URL はタイルが描き直されると別の URL になるので、長く持たせてよい (値は見ない)
    let cache = if ctx.q("timestamp").is_some() {
        "public, max-age=604800, immutable"
    } else {
        "public, max-age=300"
    };
    let content_type = usize::try_from(tile.format)
        .ok()
        .and_then(|i| TILE_TYPES.get(i).copied())
        .unwrap_or("image/jpeg");
    respond(
        ctx,
        Reply {
            data: image,
            content_type,
            cache,
            etag: Some(format!("\"{}\"", tile.hash_code)),
            last_modified: Some(tile.last_update),
        },
    )
}

/// `marker_<world>.json` のワールド名 (`/^marker_(.+)\.json$/`。`.` は改行に一致しない)
fn marker_world(file: &str) -> Option<&str> {
    let world = file.strip_prefix("marker_")?.strip_suffix(".json")?;
    let no_line_break = |c: char| !matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}');
    (!world.is_empty() && world.chars().all(no_line_break)).then_some(world)
}

/// `file.replace(/\.[^.]+$/, '')`: 最後の `.` 以降 (1 文字以上) を取る
fn strip_extension(file: &str) -> &str {
    match file.rfind('.') {
        Some(i) if i + 1 < file.len() => &file[..i],
        _ => file,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_world_follows_the_original_regex() {
        assert_eq!(marker_world("marker_main.json"), Some("main"));
        assert_eq!(marker_world("marker_a.b.json"), Some("a.b"));
        assert_eq!(marker_world("marker_.json"), None);
        assert_eq!(marker_world("marker_main.json/"), None);
        assert_eq!(marker_world("marker_ma\nin.json"), None);
        assert_eq!(marker_world("IC.png"), None);
    }

    #[test]
    fn strip_extension_follows_the_original_regex() {
        assert_eq!(strip_extension("IC.png"), "IC");
        assert_eq!(strip_extension("a.b.png"), "a.b");
        assert_eq!(strip_extension("IC"), "IC");
        assert_eq!(strip_extension("IC."), "IC.");
        assert_eq!(strip_extension(".png"), "");
    }

    #[test]
    fn blank_png_is_the_dynmap_image() {
        assert_eq!(BLANK_PNG.len(), 167);
        assert_eq!(md5_etag(BLANK_PNG), "\"1990bd0b9c4dbb6c8425aeb42207fecc\"");
    }
}
