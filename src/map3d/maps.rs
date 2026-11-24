//! `/map3d/v1/maps/{地図}/...`: BlueMap の地図のデータ。移管前の地図サーバーの `/maps/<地図>/...` と同じ応答を返す
//! (CORS の違いだけ、すべての応答に `Access-Control-Allow-Origin` が付く)。
//!
//! - `settings.json`、`textures.json`、`tiles/...`、`assets/...`: DB (`bluemap`) から。保存済みの gzip はそのまま返す
//! - `live/players.json`、`live/markers.json`、`live/sse`: BlueMap 内蔵のサーバーから (DB は失敗時の最後のバージョン)
//!
//! 鍵の名前は BlueMap 5.x の `KeyedMapStorage` と同じ。

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::http::Method;
use axum::http::header::{self, HeaderValue};
use axum::response::Response;
use futures_util::stream;
use serde_json::Value;

use super::hub::Subscription;
use super::store::{BlueData, content_encoding, decompress};
use super::{Body3, Map3d, error, markers, respond};
use crate::http::Ctx;
use crate::map::js;

const LIVE_TIMEOUT: Duration = Duration::from_secs(2);
const SSE_START_WAIT: Duration = Duration::from_secs(5);
const SSE_KEEP_ALIVE: Duration = Duration::from_secs(15);

/// `rest` は `/map3d/v1/maps/` より後ろ
pub async fn handle(svc: &Arc<Map3d>, ctx: &Ctx, rest: &str) -> Response {
    // 壊れたエンコードは 404 (500 にしない)
    let Some(path) = js::decode_uri_component(rest) else {
        return error(404, "Not Found");
    };
    let Some((map, map_path)) = path.split_once('/') else {
        return error(404, "Not Found");
    };

    if let Some(tile) = parse_tile_path(map_path) {
        return tile_response(svc, ctx, map, tile).await;
    }
    match map_path {
        "live/players.json" => return players(svc, ctx, map).await,
        "live/markers.json" => return markers_response(svc, ctx, map).await,
        "live/sse" => return sse(svc, ctx, map).await,
        _ => {}
    }

    let key = match map_path {
        "settings.json" => "bluemap:settings".to_string(),
        "textures.json" => "bluemap:textures".to_string(),
        other => match other.strip_prefix("assets/") {
            Some(asset) if !asset.is_empty() => format!("bluemap:asset/{asset}"),
            _ => return error(404, "Not Found"),
        },
    };
    let found = match svc.store.item(map, &key).await {
        Ok(Some(found)) => found,
        Ok(None) => return error(404, "Not Found"),
        Err(e) => return failed(&e.0),
    };
    let cache = if map_path == "settings.json" {
        "no-cache"
    } else {
        "public, max-age=3600"
    };
    item_response(ctx, found, content_type_of(map_path), cache)
}

fn failed(message: &str) -> Response {
    eprintln!("map3d: database error: {message}");
    error(500, "Internal Server Error")
}

fn content_type_of(path: &str) -> &'static str {
    match path
        .rsplit('.')
        .next()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        _ => "application/octet-stream",
    }
}

fn item_response(
    ctx: &Ctx,
    found: BlueData,
    content_type: &'static str,
    cache: &'static str,
) -> Response {
    respond(
        ctx,
        Body3 {
            encoding: content_encoding(&found.compression),
            data: found.data,
            content_type,
            cache,
            etag: None,
        },
    )
}

/// タイルの場所
#[derive(Debug, PartialEq, Eq)]
pub struct TilePath {
    pub lod: i64,
    pub x: i64,
    pub z: i64,
}

/// `tiles/<lod>/x<-?[\d/]+>z<-?[\d/]+>.(prbm|png)` (`/^tiles\/(\d+)\/x(-?[\d\/]+)z(-?[\d\/]+)\.(prbm|png)$/`)
pub fn parse_tile_path(path: &str) -> Option<TilePath> {
    let rest = path.strip_prefix("tiles/")?;
    let (lod, rest) = rest.split_once('/')?;
    if lod.is_empty() || !lod.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let rest = rest.strip_prefix('x')?;
    let (x, rest) = rest.split_once("/z")?;
    let (z, ext) = rest.rsplit_once('.')?;
    if ext != "prbm" && ext != "png" {
        return None;
    }
    // 符号は先頭だけ。残りは数字と `/` (1 桁ずつ `/` で区切る)
    let coordinate = |text: &str| -> Option<i64> {
        let digits = text.strip_prefix('-').unwrap_or(text);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit() || b == b'/') {
            return None;
        }
        // JavaScript の Number('-') や安全でない整数は 404。`/` だけなら Number('') で 0
        js::safe_integer(js::number(&text.replace('/', "")))
    };
    Some(TilePath {
        lod: js::safe_integer(js::number(lod))?,
        x: coordinate(x)?,
        z: coordinate(z)?,
    })
}

async fn tile_response(svc: &Arc<Map3d>, ctx: &Ctx, map: &str, tile: TilePath) -> Response {
    let storage = if tile.lod == 0 {
        "bluemap:hires".to_string()
    } else {
        format!("bluemap:lowres/{}", tile.lod)
    };
    match svc.store.tile(map, &storage, tile.x, tile.z).await {
        Ok(Some(found)) => item_response(
            ctx,
            found,
            if tile.lod == 0 {
                "application/octet-stream"
            } else {
                "image/png"
            },
            "public, max-age=86400, stale-if-error=604800",
        ),
        // 描いていないタイルは 204 (BlueMap と同じ)
        Ok(None) => {
            let mut res = Response::new(Body::empty());
            *res.status_mut() = axum::http::StatusCode::NO_CONTENT;
            res.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=60"),
            );
            res
        }
        Err(e) => failed(&e.0),
    }
}

/// 知らない地図の名前で、BlueMap 本体へ聞きに行かない
async fn unknown_map(svc: &Arc<Map3d>, map: &str) -> Option<Response> {
    match svc.store.map_exists(map).await {
        Ok(true) => None,
        Ok(false) => Some(error(404, "Not Found")),
        Err(e) => Some(failed(&e.0)),
    }
}

/// `live/players.json`: BlueMap 内蔵のサーバーに聞く。止まっているときは DB の最後のバージョン
async fn players(svc: &Arc<Map3d>, ctx: &Ctx, map: &str) -> Response {
    if let Some(res) = unknown_map(svc, map).await {
        return res;
    }

    if let Some(body) = svc.live.get(map, "players.json", LIVE_TIMEOUT).await {
        return respond(
            ctx,
            Body3 {
                data: body,
                content_type: "application/json",
                cache: "no-store",
                etag: None,
                encoding: None,
            },
        );
    }
    match svc.store.item(map, "bluemap:players").await {
        Ok(Some(found)) => item_response(ctx, found, "application/json", "no-cache"),
        Ok(None) => error(404, "Not Found"),
        Err(e) => failed(&e.0),
    }
}

/// `live/markers.json`: BlueMap のマーカーに、Dynmap の地点を足す (`{...Dynmap, ...BlueMap}`)
async fn markers_response(svc: &Arc<Map3d>, ctx: &Ctx, map: &str) -> Response {
    if let Some(res) = unknown_map(svc, map).await {
        return res;
    }

    let (bluemap, dynmap) = tokio::join!(bluemap_markers(svc, map), dynmap_markers(svc, map));
    let mut merged = dynmap;
    for (key, value) in bluemap {
        merged.insert(key, value);
    }
    respond(
        ctx,
        Body3 {
            data: Value::Object(merged).to_string().into_bytes(),
            content_type: "application/json",
            cache: "no-cache",
            etag: None,
            encoding: None,
        },
    )
}

async fn bluemap_markers(svc: &Arc<Map3d>, map: &str) -> serde_json::Map<String, Value> {
    if let Some(body) = svc.live.get(map, "markers.json", LIVE_TIMEOUT).await
        && let Ok(Value::Object(sets)) = serde_json::from_slice::<Value>(&body)
    {
        return sets;
    }
    // Minecraft サーバーの再起動中など。DB の最後のバージョンにする
    let Ok(Some(found)) = svc.store.item(map, "bluemap:markers").await else {
        return Default::default();
    };
    match decompress(&found).and_then(|raw| serde_json::from_slice::<Value>(&raw).ok()) {
        Some(Value::Object(sets)) => sets,
        _ => Default::default(),
    }
}

async fn dynmap_markers(svc: &Arc<Map3d>, map: &str) -> serde_json::Map<String, Value> {
    let Some((_, world)) = svc.cfg.dynmap_worlds.iter().find(|(m, _)| m == map) else {
        return Default::default();
    };
    {
        let cache = svc.dynmap_markers.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, sets)) = cache.get(world)
            && at.elapsed() < Duration::from_secs(markers::MAX_AGE_SECS)
        {
            return sets.clone();
        }
    }
    let sets = match svc.map.store.marker_file(world).await {
        Ok(Some(content)) => markers::dynmap_sets(&content),
        Ok(None) => Default::default(),
        Err(e) => {
            eprintln!("map3d: dynmap markers: {e}");
            return Default::default();
        }
    };
    let mut cache = svc.dynmap_markers.lock().unwrap_or_else(|e| e.into_inner());
    // 世界の数は設定の数だけ (増えない)
    cache.insert(world.clone(), (Instant::now(), sets.clone()));
    sets
}

/// `live/sse`: BlueMap 内蔵のサーバーの SSE を流す (WebSocket の裏側の購読と共有する)。
/// つながりが切れたら、こちらも閉じる (ブラウザの EventSource がつなぎ直す)
async fn sse(svc: &Arc<Map3d>, ctx: &Ctx, map: &str) -> Response {
    if let Some(res) = unknown_map(svc, map).await {
        return res;
    }

    let mut res = Response::new(Body::empty());
    let headers = res.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if ctx.method == Method::HEAD {
        return res;
    }
    let Some(permit) = svc.map.acquire_ws(&ctx.headers) else {
        return crate::http::busy();
    };
    let mut sub = svc.hub.subscribe(map);
    // BlueMap につながるのを少し待つ (つながらなければ 503)
    let connected = matches!(
        tokio::time::timeout(SSE_START_WAIT, sub.open.wait_for(|open| *open)).await,
        Ok(Ok(_))
    );
    if !connected {
        let mut res = error(503, "Service Unavailable");
        res.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
        return res;
    }
    *res.body_mut() = Body::from_stream(sse_stream(sub, permit));
    res
}

/// イベントを SSE の形にする
pub fn sse_frame(event: &str, data: &str) -> String {
    let mut out = format!("event: {event}\n");
    for line in data.split('\n') {
        out.push_str("data: ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out
}

fn sse_stream(
    sub: Subscription,
    permit: crate::map::WsPermit,
) -> impl futures_util::Stream<Item = Result<Bytes, Infallible>> {
    // 最初に、つながったことを知らせる (ブラウザの onopen のため)
    let first = Bytes::from_static(b": open\n\n");
    let head = stream::iter([Ok(first)]);
    let tail = stream::unfold(
        (sub, permit, tokio::time::interval(SSE_KEEP_ALIVE)),
        |(mut sub, permit, mut keep)| async move {
            keep.reset();
            loop {
                tokio::select! {
                    event = sub.events.recv() => match event {
                        Ok(event) => {
                            let frame = sse_frame(&event.event, &event.data);
                            return Some((Ok(Bytes::from(frame)), (sub, permit, keep)));
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => return None,
                    },
                    changed = sub.open.changed() => {
                        // BlueMap とのつながりが切れたら閉じる
                        if changed.is_err() || !*sub.open.borrow_and_update() {
                            return None;
                        }
                    }
                    _ = keep.tick() => {
                        return Some((Ok(Bytes::from_static(b": keep-alive\n\n")), (sub, permit, keep)));
                    }
                }
            }
        },
    );
    futures_util::StreamExt::chain(head, tail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_paths() {
        assert_eq!(
            parse_tile_path("tiles/0/x-3/7/0/z-3/9/3.prbm"),
            Some(TilePath {
                lod: 0,
                x: -370,
                z: -393
            })
        );
        assert_eq!(
            parse_tile_path("tiles/2/x1/z0.png"),
            Some(TilePath { lod: 2, x: 1, z: 0 })
        );
        // 拡張子は prbm と png だけ (どちらでも、lod で返す形が決まる)
        assert!(parse_tile_path("tiles/0/x1/z1.png").is_some());
        assert_eq!(parse_tile_path("tiles/0/x1/z1.jpg"), None);
        assert_eq!(parse_tile_path("tiles/0/x1/z1.prbm.gz"), None);
        assert_eq!(parse_tile_path("tiles/a/x1/z1.png"), None);
        assert_eq!(parse_tile_path("tiles//x1/z1.png"), None);
        assert_eq!(parse_tile_path("tiles/0/x/z1.png"), None);
        assert_eq!(parse_tile_path("tiles/0/xa/z1.png"), None);
        assert_eq!(parse_tile_path("tiles/0/x-/z1.png"), None);
        assert_eq!(parse_tile_path("tiles/0/x1/z1"), None);
        // 符号は先頭だけ
        assert_eq!(parse_tile_path("tiles/0/x1/-2/z1.png"), None);
        // `/` だけなら Number('') で 0
        assert_eq!(
            parse_tile_path("tiles/0/x//z1.png"),
            Some(TilePath { lod: 0, x: 0, z: 1 })
        );
        // 安全でない整数は 404
        assert_eq!(
            parse_tile_path("tiles/0/x99999999999999999999/z1.png"),
            None
        );
        assert_eq!(parse_tile_path("settings.json"), None);
    }

    #[test]
    fn content_types() {
        assert_eq!(content_type_of("settings.json"), "application/json");
        assert_eq!(content_type_of("assets/playerheads/x.PNG"), "image/png");
        assert_eq!(content_type_of("a.jpg"), "image/jpeg");
        assert_eq!(content_type_of("a.bin"), "application/octet-stream");
    }

    #[test]
    fn sse_frames() {
        assert_eq!(
            sse_frame("tile", "{\"a\":1}"),
            "event: tile\ndata: {\"a\":1}\n\n"
        );
        assert_eq!(
            sse_frame("player", "a\nb"),
            "event: player\ndata: a\ndata: b\n\n"
        );
    }
}
