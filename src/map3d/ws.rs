//! `/map3d/v1/ws`: ブラウザ (3d-map.torosaba.net の画面) との WebSocket。公開の待受で受ける。
//!
//! - 接続: Origin の規則は `/map/v1/ws` と同じ (空、`localhost`、`127.0.0.1`、`(*.)torosaba.net`。ほかは `1008 Forbidden` で閉じる)
//! - ブラウザ→サーバー: `{"bluemap":"<地図の id>"}` (100 文字未満)。全体で 512 文字以内、2 秒に 5 通まで (超えたものは無視)。
//!   壊れたもの・オブジェクトでないものは、応答せず、接続も保つ
//! - サーバー→ブラウザ:
//!   - `{"type":"bluemap","map":"<id>","state":"open|error"}`: BlueMap 本体につながっているか (つなぎ直すたびに変わる)
//!   - `{"type":"bluemap","map":"<id>","event":"player|tile|marker","data":"<BlueMap の SSE の JSON を文字列のまま>"}`
//!   - `{"type":"ping"}`: 見ている地図があるあいだ 30 秒ごと (前段のプロキシは 120 秒何も流れない WebSocket を切る)
//! - 知らない地図の id は `state:"error"` だけ返し、BlueMap へつなぎに行かない (接続は保つ)

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequest, Request};
use axum::http::header;
use axum::response::Response;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;

use super::hub::Subscription;
use super::{Map3d, error};
use crate::http;
use crate::map::js;
use crate::map::ws::{MAX_PAYLOAD, RateLimiter, origin_allowed};

const HEARTBEAT: Duration = Duration::from_secs(30);
/// ブラウザから届くのは、名前が 1 つだけの短いもの。これより長いものは解かない
const MAX_MESSAGE_CHARS: usize = 512;

/// 通常の GET (Upgrade ではない) への応答
pub fn upgrade_required() -> Response {
    let mut res = error(426, "Upgrade Required");
    res.headers_mut().insert(
        header::UPGRADE,
        axum::http::HeaderValue::from_static("websocket"),
    );
    res
}

/// ブラウザからのメッセージから、見る地図の id を取り出す。解けないもの、長すぎるものは None
pub fn subscribe_target(text: &str) -> Option<String> {
    if js::utf16_len(text) > MAX_MESSAGE_CHARS {
        return None;
    }
    let Value::Object(data) = serde_json::from_str::<Value>(text).ok()? else {
        return None;
    };
    let map = data.get("bluemap")?.as_str()?;
    let len = js::utf16_len(map);
    (len > 0 && len < 100).then(|| map.to_string())
}

fn state_text(map: &str, open: bool) -> String {
    json!({ "type": "bluemap", "map": map, "state": if open { "open" } else { "error" } })
        .to_string()
}

fn event_text(map: &str, event: &str, data: &str) -> String {
    json!({ "type": "bluemap", "map": map, "event": event, "data": data }).to_string()
}

/// `/map3d/v1/ws` の Upgrade を受ける。本文は読まない (ボディを先に読む通常の経路より前に分かれる)
pub async fn accept(svc: Arc<Map3d>, request: Request) -> Response {
    let allowed = match request.headers().get(header::ORIGIN) {
        None => true,
        // Origin が文字列として読めないものは、許さない
        Some(value) => value
            .to_str()
            .map(|origin| origin_allowed(Some(origin)))
            .unwrap_or(false),
    };
    let upgrade = match WebSocketUpgrade::from_request(request, &()).await {
        Ok(upgrade) => upgrade,
        Err(_) => return upgrade_required(),
    };
    // 接続の数は、ふつうの要求の枠とは別に数える (2D の WebSocket と同じ枠)
    let Ok(permit) = svc.map.ws_slots.clone().try_acquire_owned() else {
        return http::busy();
    };
    upgrade
        .max_message_size(MAX_PAYLOAD)
        .max_frame_size(MAX_PAYLOAD)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            if allowed {
                serve(svc, socket).await;
            } else {
                forbid(socket).await;
            }
        })
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

async fn send(socket: &mut WebSocket, text: String) -> bool {
    socket.send(Message::Text(text.into())).await.is_ok()
}

async fn serve(svc: Arc<Map3d>, mut socket: WebSocket) {
    let mut limiter = RateLimiter::default();
    let mut current: Option<Subscription> = None;
    let mut heartbeat =
        tokio::time::interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        // 見ている地図があれば、そのイベントとつながりの変化も待つ
        let upstream = async {
            match current.as_mut() {
                Some(sub) => {
                    tokio::select! {
                        event = sub.events.recv() => Upstream::Event(event),
                        changed = sub.open.changed() => Upstream::State(changed.is_ok()),
                    }
                }
                None => std::future::pending().await,
            }
        };
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
                if let Some(map) = subscribe_target(&text) {
                    if current.as_ref().is_some_and(|sub| sub.map == map) {
                        continue;
                    }
                    // 前の地図は、新しい地図を確かめる前に手放す
                    current = None;
                    let known = svc.store.map_exists(&map).await.unwrap_or(false);
                    if !known {
                        if !send(&mut socket, state_text(&map, false)).await {
                            break;
                        }
                        continue;
                    }
                    let mut sub = svc.hub.subscribe(&map);
                    let open = *sub.open.borrow_and_update();
                    let ok = send(&mut socket, state_text(&map, open)).await;
                    current = Some(sub);
                    if !ok {
                        break;
                    }
                }
            }
            up = upstream => {
                let Some(sub) = current.as_mut() else { continue };
                let text = match up {
                    Upstream::Event(Ok(event)) => Some(event_text(&sub.map, &event.event, &event.data)),
                    // 読み飛ばした分は、画面が次のイベントと 1 秒ごとの取りに行きで追いつく
                    Upstream::Event(Err(RecvError::Lagged(_))) => None,
                    Upstream::State(true) => {
                        let open = *sub.open.borrow_and_update();
                        Some(state_text(&sub.map, open))
                    }
                    // 送り手が消えた (見ている人がいるあいだは起こらない)。空回りしないよう手放す
                    Upstream::Event(Err(RecvError::Closed)) | Upstream::State(false) => {
                        current = None;
                        None
                    }
                };
                if let Some(text) = text && !send(&mut socket, text).await {
                    break;
                }
            }
            _ = heartbeat.tick() => {
                if current.is_some() && !send(&mut socket, r#"{"type":"ping"}"#.to_string()).await {
                    break;
                }
            }
        }
    }
    // `current` を手放して、見る人が 0 になったら BlueMap とのつなぎをやめる
    drop(current);
}

enum Upstream {
    Event(Result<Arc<super::hub::UpEvent>, RecvError>),
    /// つながりの変化 (`false` は送り手が消えた)
    State(bool),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscribe_messages() {
        assert_eq!(
            subscribe_target(r#"{"bluemap":"overworld"}"#).as_deref(),
            Some("overworld")
        );
        assert_eq!(subscribe_target(r#"{"bluemap":""}"#), None);
        assert_eq!(subscribe_target(r#"{"bluemap":1}"#), None);
        assert_eq!(subscribe_target(r#"{"subscribe":"main"}"#), None);
        assert_eq!(subscribe_target("not json"), None);
        assert_eq!(subscribe_target("null"), None);
        assert_eq!(subscribe_target("[]"), None);
        assert_eq!(subscribe_target("5"), None);
        let name99 = "a".repeat(99);
        assert!(subscribe_target(&format!(r#"{{"bluemap":"{name99}"}}"#)).is_some());
        let name100 = "a".repeat(100);
        assert_eq!(
            subscribe_target(&format!(r#"{{"bluemap":"{name100}"}}"#)),
            None
        );
        let long = format!(r#"{{"bluemap":"a","pad":"{}"}}"#, "x".repeat(500));
        assert_eq!(subscribe_target(&long), None);
    }

    #[test]
    fn message_shapes() {
        assert_eq!(
            state_text("overworld", false),
            r#"{"type":"bluemap","map":"overworld","state":"error"}"#
        );
        assert_eq!(
            state_text("overworld", true),
            r#"{"type":"bluemap","map":"overworld","state":"open"}"#
        );
        assert_eq!(
            event_text("overworld", "tile", r#"{"a":1}"#),
            r#"{"type":"bluemap","map":"overworld","event":"tile","data":"{\"a\":1}"}"#
        );
    }
}
