//! `/map/v1/ws`: ブラウザ (map.torosaba.net の画面) との WebSocket。公開の待受で受ける。
//!
//! - 接続: Origin が空、または `localhost` / `127.0.0.1` / `*.torosaba.net` のとき許可。それ以外は `1008 Forbidden` で閉じる
//! - ブラウザ→サーバー: `{"subscribe":"<world>"}` (100 文字未満)。2 秒に 5 通までで、超えたものは無視。512 文字より長いものも無視
//! - サーバー→ブラウザ: `update` と `players` (`hub` を見てください)

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequest, Request};
use axum::http::header;
use axum::response::Response;
use serde_json::Value;
use tokio::task::JoinSet;

use super::hub::Peer;
use super::{MapService, js};
use crate::http;

/// 1 つのメッセージの大きさの上限 (全部の WebSocket に共通。dynmap-live の全員の位置が入る長さ)
pub const MAX_PAYLOAD: usize = 256 * 1024;
/// ブラウザから届くのは、名前が 1 つだけの短いもの。これより長いものは解かない
const MAX_MESSAGE_CHARS: usize = 512;
const RATE_WINDOW: Duration = Duration::from_secs(2);
const RATE_LIMIT: usize = 5;

/// Upgrade の要求か (`Connection: Upgrade` と `Upgrade: websocket` の GET)
pub fn is_upgrade(request: &Request) -> bool {
    request.method() == axum::http::Method::GET
        && request
            .headers()
            .get(header::UPGRADE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("websocket"))
}

/// `/map/v1/ws` (末尾の / は 1 つだけ無視する。ルーターと同じ)
pub fn is_ws_path(path: &str) -> bool {
    path.strip_suffix('/').unwrap_or(path) == "/map/v1/ws"
}

/// Origin を許すか。空 (付いていない) なら許す
pub fn origin_allowed(origin: Option<&str>) -> bool {
    let Some(origin) = origin else { return true };
    if origin.is_empty() {
        return true;
    }
    // /^https?:\/\/(localhost|127\.0\.0\.1|([a-z0-9-]+\.)?torosaba\.net)(:\d+)?$/i
    let lower = origin.to_ascii_lowercase();
    let Some(rest) = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
    else {
        return false;
    };
    let host = match rest.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => rest,
    };
    if host == "localhost" || host == "127.0.0.1" || host == "torosaba.net" {
        return true;
    }
    match host.strip_suffix(".torosaba.net") {
        Some(label) => {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        }
        None => false,
    }
}

/// ブラウザからのメッセージから、見るワールドの名前を取り出す。解けないもの、長すぎるものは None
pub fn subscribe_target(text: &str) -> Option<String> {
    if js::utf16_len(text) > MAX_MESSAGE_CHARS {
        return None;
    }
    let Value::Object(data) = serde_json::from_str::<Value>(text).ok()? else {
        return None;
    };
    let world = data.get("subscribe")?.as_str()?;
    let len = js::utf16_len(world);
    (len > 0 && len < 100).then(|| world.to_string())
}

/// 2 秒に 5 通まで
#[derive(Default)]
pub struct RateLimiter {
    times: VecDeque<Instant>,
}

impl RateLimiter {
    /// 受けてよければ true。超えたものは数えない
    pub fn allow(&mut self, now: Instant) -> bool {
        while self
            .times
            .front()
            .is_some_and(|t| now.duration_since(*t) >= RATE_WINDOW)
        {
            self.times.pop_front();
        }
        if self.times.len() >= RATE_LIMIT {
            return false;
        }
        self.times.push_back(now);
        true
    }
}

/// `/map/v1/ws` の Upgrade を受ける。本文は読まない (ボディを先に読む通常の経路より前に分かれる)
pub async fn accept(map: Arc<MapService>, request: Request) -> Response {
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .map(|v| v.to_str().map(str::to_string));
    // Origin が文字列として読めないものは、許さない
    let allowed = match &origin {
        None => true,
        Some(Ok(origin)) => origin_allowed(Some(origin)),
        Some(Err(_)) => false,
    };
    let upgrade = match WebSocketUpgrade::from_request(request, &()).await {
        Ok(upgrade) => upgrade,
        Err(_) => return upgrade_required(),
    };
    // 接続の数は、ふつうの要求の枠とは別に数える
    let Ok(permit) = map.ws_slots.clone().try_acquire_owned() else {
        return http::busy();
    };
    upgrade
        .max_message_size(MAX_PAYLOAD)
        .max_frame_size(MAX_PAYLOAD)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            if allowed {
                serve(map, socket).await;
            } else {
                forbid(socket).await;
            }
        })
}

pub fn upgrade_required() -> Response {
    let mut res = super::api::error(426, "Upgrade Required");
    res.headers_mut().insert(
        header::UPGRADE,
        axum::http::HeaderValue::from_static("websocket"),
    );
    res
}

/// 許さない Origin: つないだあとで、1008 で閉じる
async fn forbid(mut socket: WebSocket) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: 1008,
            reason: "Forbidden".into(),
        })))
        .await;
}

async fn serve(map: Arc<MapService>, mut socket: WebSocket) {
    let (peer, mut outbox) = Peer::new(map.next_peer_id());
    let mut limiter = RateLimiter::default();
    // 購読は DB を読むので、受信を止めないよう別の仕事にする。接続が終わるときにまとめて止める
    let mut subscriptions = JoinSet::new();
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                let text = match incoming {
                    Some(Ok(Message::Text(text))) => text.as_str().to_string(),
                    Some(Ok(Message::Binary(bytes))) => String::from_utf8_lossy(&bytes).into_owned(),
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                };
                if !limiter.allow(Instant::now()) {
                    continue;
                }
                // `{"bluemap":"<map>"}` は 3D 用。この段階では未対応で、黙って無視する
                if let Some(world) = subscribe_target(&text) {
                    let hub = map.hub.clone();
                    let peer = peer.clone();
                    subscriptions.spawn(async move { hub.subscribe(&peer, &world).await });
                }
            }
            Some(text) = outbox.recv() => {
                if socket.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            () = peer.kicked.notified() => break,
        }
    }
    // 動いている購読を止めて、終わるのを待ってから外す (閉じたあとに、見ている人として足されないように)
    subscriptions.shutdown().await;
    map.hub.leave(peer.id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_rules() {
        for ok in [
            "",
            "https://map.torosaba.net",
            "http://map.torosaba.net",
            "https://torosaba.net",
            "https://TOROSABA.net",
            "http://localhost",
            "http://localhost:3000",
            "http://127.0.0.1:8080",
            "https://a-1.torosaba.net:8443",
        ] {
            assert!(origin_allowed(Some(ok)), "{ok}");
        }
        assert!(origin_allowed(None));
        for bad in [
            "https://example.invalid",
            "https://evil-torosaba.net",
            "https://torosaba.net.evil.com",
            "https://a.b.torosaba.net",
            "https://.torosaba.net",
            "ftp://torosaba.net",
            "https://torosaba.net/",
            "https://torosaba.net:",
            "https://torosaba.net:80a",
            "null",
            "https://map.torosaba.net\n",
        ] {
            assert!(!origin_allowed(Some(bad)), "{bad}");
        }
    }

    #[test]
    fn subscribe_messages() {
        assert_eq!(
            subscribe_target(r#"{"subscribe":"main"}"#).as_deref(),
            Some("main")
        );
        assert_eq!(subscribe_target(r#"{"subscribe":""}"#), None);
        assert_eq!(subscribe_target(r#"{"subscribe":1}"#), None);
        assert_eq!(subscribe_target(r#"{"bluemap":"overworld"}"#), None);
        assert_eq!(subscribe_target("not json"), None);
        assert_eq!(subscribe_target("null"), None);
        assert_eq!(subscribe_target("[]"), None);
        let name99 = "a".repeat(99);
        assert!(subscribe_target(&format!(r#"{{"subscribe":"{name99}"}}"#)).is_some());
        let name100 = "a".repeat(100);
        assert_eq!(
            subscribe_target(&format!(r#"{{"subscribe":"{name100}"}}"#)),
            None
        );
        // 512 文字を超えるメッセージは解かない
        let long = format!(r#"{{"subscribe":"main","pad":"{}"}}"#, "x".repeat(500));
        assert_eq!(subscribe_target(&long), None);
    }

    #[test]
    fn rate_limit_is_five_per_two_seconds() {
        let mut limiter = RateLimiter::default();
        let start = Instant::now();
        for _ in 0..5 {
            assert!(limiter.allow(start));
        }
        assert!(!limiter.allow(start + Duration::from_millis(1999)));
        // 超えたものは数えない: 最初の 5 通が窓から出れば、また受けられる
        assert!(limiter.allow(start + Duration::from_millis(2000)));
    }
}
