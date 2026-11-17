//! `/live/plugin`: TORO-Core の dynmap-live (Minecraft サーバーの中) からの WebSocket。
//! 全員の位置と体力が 0.25 秒ごとに届く。**内部専用の待受 (`MAP_LIVE_ADDR`) だけ**で受ける。
//! 公開の待受 (Tunnel の先) には、この経路を出さない (`/live/*` は 404)。さらに `X-Toro-Token` で確かめる。
//!
//! 同じ内部専用の待受で、`POST /live/clients` (TORO-Proxy が、いまつながっているプレイヤーの IP を全員分送ってくる) も受ける。
//! 合言葉は `MAP_CLIENTS_TOKEN` (`/live/plugin` の合言葉とは別)。IP は `crate::map3d::clients` がメモリにだけ置く。

use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequest, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::MapService;
use super::ws::MAX_PAYLOAD;
use crate::map3d::Map3d;
use crate::map3d::clients::parse_ips;

/// 合言葉を比べる。どちらも SHA-256 にしてから比べるので、長さが違っても (マルチバイトの文字でも)
/// 例外にならず、時間で中身が漏れない。決めた合言葉が空なら、どんな値でも通さない
pub fn token_matches(given: &str, expected: &str) -> bool {
    if expected.is_empty() {
        return false;
    }
    let a = Sha256::digest(given.as_bytes());
    let b = Sha256::digest(expected.as_bytes());
    // 最初の違いで止めずに、全部のバイトを比べる
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// 内部専用の待受の状態
#[derive(Clone)]
struct Internal {
    map: Arc<MapService>,
    /// `/live/clients` を受けるとき (3D 地図 API が動いているときだけ)
    map3d: Option<Arc<Map3d>>,
}

/// `/live/clients` の本文の大きさの上限 (2,000 件 × 64 文字より十分大きい)
const MAX_CLIENTS_BODY: usize = 256 * 1024;
const CLIENTS_BODY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 内部専用の待受のルーター (`/live/plugin` と `/healthz` だけ)
pub fn router(map: Arc<MapService>) -> Router {
    router_with(map, None)
}

/// 内部専用の待受のルーター (`/live/clients` も受けるとき)
pub fn router_with(map: Arc<MapService>, map3d: Option<Arc<Map3d>>) -> Router {
    Router::new()
        .fallback(dispatch)
        .with_state(Internal { map, map3d })
}

/// `/live/clients` と `/healthz` だけを受ける追加の内部専用の待受のルーター (`MAP_CLIENTS_ADDR`)。
/// `/live/clients` の検証は、`router_with` の待受と同じ `clients` を使う。`/healthz` には人数だけを出す (IP は出さない)
pub fn clients_router(map3d: Arc<Map3d>) -> Router {
    Router::new().fallback(dispatch_clients).with_state(map3d)
}

async fn dispatch_clients(State(map3d): State<Arc<Map3d>>, request: Request) -> Response {
    match request.uri().path() {
        "/healthz" => {
            let body = json!({ "ok": true, "players": map3d.clients.online_count() });
            let mut res = Response::new(body.to_string().into());
            let headers = res.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            res
        }
        "/live/clients" => clients(map3d, request).await,
        _ => text_response(StatusCode::NOT_FOUND, "Not Found"),
    }
}

fn text_response(status: StatusCode, text: &'static str) -> Response {
    let mut res = Response::new(text.into());
    *res.status_mut() = status;
    let headers = res.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

async fn dispatch(State(state): State<Internal>, request: Request) -> Response {
    let Internal { map, map3d } = state;
    match request.uri().path() {
        "/healthz" => {
            let mut res = Response::new(json!({ "ok": true }).to_string().into());
            res.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            res
        }
        "/live/plugin" => plugin(map, request).await,
        "/live/clients" => match map3d {
            Some(map3d) => clients(map3d, request).await,
            None => text_response(StatusCode::NOT_FOUND, "Not Found"),
        },
        _ => text_response(StatusCode::NOT_FOUND, "Not Found"),
    }
}

/// `POST /live/clients`: `{"ips":["…"]}` (最大 2,000 件、各 64 文字以内)。合言葉は `X-Toro-Token`。成功は 204
async fn clients(map3d: Arc<Map3d>, request: Request) -> Response {
    let given = request
        .headers()
        .get("x-toro-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !token_matches(given, &map3d.cfg.clients_token) {
        return text_response(StatusCode::FORBIDDEN, "Forbidden");
    }
    if request.method() != axum::http::Method::POST {
        return text_response(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed");
    }
    let body = request.into_body();
    let body = match tokio::time::timeout(
        CLIENTS_BODY_TIMEOUT,
        axum::body::to_bytes(body, MAX_CLIENTS_BODY),
    )
    .await
    {
        Ok(Ok(body)) => body,
        _ => return text_response(StatusCode::BAD_REQUEST, "Bad Request"),
    };
    let Some(ips) = parse_ips(&body) else {
        return text_response(StatusCode::BAD_REQUEST, "Bad Request");
    };
    map3d.clients.update(&ips);
    let mut res = Response::new(axum::body::Body::empty());
    *res.status_mut() = StatusCode::NO_CONTENT;
    res
}

async fn plugin(map: Arc<MapService>, request: Request) -> Response {
    let given = request
        .headers()
        .get("x-toro-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !token_matches(given, &map.live_token) {
        return text_response(StatusCode::FORBIDDEN, "Forbidden");
    }
    let upgrade = match WebSocketUpgrade::from_request(request, &()).await {
        Ok(upgrade) => upgrade,
        Err(_) => return text_response(StatusCode::UPGRADE_REQUIRED, "Upgrade Required"),
    };
    let Ok(permit) = map.live_slots.clone().try_acquire_owned() else {
        return text_response(StatusCode::SERVICE_UNAVAILABLE, "Service Unavailable");
    };
    upgrade
        .max_message_size(MAX_PAYLOAD)
        .max_frame_size(MAX_PAYLOAD)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            receive(map, socket).await;
        })
}

/// `{"t":"players","players":[...]}` を受けて、ハブに渡す。ほかのもの、壊れたものは無視する
async fn receive(map: Arc<MapService>, mut socket: WebSocket) {
    while let Some(Ok(message)) = socket.recv().await {
        let data: Option<Value> = match &message {
            Message::Text(text) => serde_json::from_str(text.as_str()).ok(),
            Message::Binary(bytes) => serde_json::from_slice(bytes).ok(),
            Message::Close(_) => break,
            _ => None,
        };
        if let Some(Value::Object(data)) = data
            && data.get("t").and_then(Value::as_str) == Some("players")
            && let Some(Value::Array(players)) = data.get("players")
        {
            map.hub.set_positions(players);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_compared_by_digest() {
        assert!(token_matches("secret", "secret"));
        assert!(!token_matches("secret", "Secret"));
        assert!(!token_matches("", "secret"));
        assert!(!token_matches("日本語", "secret"));
        // 決めた合言葉が空なら、空の値も通さない
        assert!(!token_matches("", ""));
        assert!(!token_matches("anything", ""));
    }
}
