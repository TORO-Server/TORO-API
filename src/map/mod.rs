//! 2D 地図の API (Dynmap の DB を読んで返す)。移管前の TORO-WebMap のサーバー側 (地図サーバー) の置き換え。
//!
//! - `api`: `/map/v1/configuration`、`/map/v1/world/{world}/{ts}`、`/map/v1/tiles/...` (HTTP)
//! - `ws`: `/map/v1/ws` (ブラウザとの WebSocket。公開の待受)
//! - `live`: `/live/plugin` (dynmap-live からの位置の受信。**内部専用の待受だけ**)
//! - `hub`: WebSocket の配信ハブ (ワールドごとに 1 秒に 1 度だけ DB を読む)
//! - `store`: DB の読み取り (SELECT だけ)。試験では差し替える
//! - `js`: 移管前の実装 (JavaScript) と同じ結果にするための小さな道具
//!
//! 3D (BlueMap) の動的な側は、あとの段階でこのモジュールの隣に足す。
//! `/map/v1/ws` の `{"bluemap":"<map>"}` は 3D 用で、**この段階では未対応** (黙って無視する。応答は返さない)。

pub mod api;
pub mod hub;
pub mod js;
pub mod live;
pub mod store;
pub mod ws;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use hub::Hub;
use store::MapStore;
use crate::map3d::sign::limits::limit_key;

/// 同時につなげる WebSocket (ブラウザ) の数の上限。ふつうの要求の同時処理の枠とは別に数える
pub const MAX_WS_CONNECTIONS: usize = 2048;
/// 1 つの接続元 IP あたりの WebSocket / SSE 同時接続の上限
pub const MAX_WS_PER_IP: usize = 64;
const MAX_TRACKED_WS_IPS: usize = 4096;
/// 同時につなげる `/live/plugin` の数の上限 (本番では dynmap-live の 1 本)
pub const MAX_LIVE_CONNECTIONS: usize = 8;

pub struct WsIpPermit {
    counts: Arc<Mutex<HashMap<String, usize>>>,
    key: String,
}

impl Drop for WsIpPermit {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.counts.lock()
            && let Some(n) = guard.get_mut(&self.key)
        {
            if *n <= 1 {
                guard.remove(&self.key);
            } else {
                *n -= 1;
            }
        }
    }
}

pub struct WsPermit {
    _global: OwnedSemaphorePermit,
    _per_ip: Option<WsIpPermit>,
}

pub struct MapService {
    pub store: Arc<dyn MapStore>,
    pub hub: Arc<Hub>,
    /// ブラウザとの WebSocket の枠
    pub ws_slots: Arc<Semaphore>,
    /// 接続元 IP ごとの WebSocket 同時接続数
    pub ws_per_ip: Arc<Mutex<HashMap<String, usize>>>,
    /// `/live/plugin` の枠
    pub live_slots: Arc<Semaphore>,
    /// `/live/plugin` の `X-Toro-Token` の期待値 (空なら、どんな値も通さない)
    pub live_token: String,
    next_peer: AtomicU64,
}

impl MapService {
    pub fn new(store: Arc<dyn MapStore>, live_token: String) -> Arc<Self> {
        let hub = Hub::new(store.clone());
        Self::with_hub(store, hub, live_token)
    }

    pub fn with_hub(store: Arc<dyn MapStore>, hub: Arc<Hub>, live_token: String) -> Arc<Self> {
        Self::with_limits(
            store,
            hub,
            live_token,
            MAX_WS_CONNECTIONS,
            MAX_LIVE_CONNECTIONS,
        )
    }

    /// 接続数の上限を指定して作る (試験用)
    pub fn with_limits(
        store: Arc<dyn MapStore>,
        hub: Arc<Hub>,
        live_token: String,
        max_ws: usize,
        max_live: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            hub,
            ws_slots: Arc::new(Semaphore::new(max_ws)),
            ws_per_ip: Arc::new(Mutex::new(HashMap::new())),
            live_slots: Arc::new(Semaphore::new(max_live)),
            live_token,
            next_peer: AtomicU64::new(1),
        })
    }

    pub fn acquire_ws(&self, headers: &axum::http::HeaderMap) -> Option<WsPermit> {
        let raw_ip = headers
            .get("x-toro-client-ip")
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty())
            .or_else(|| {
                headers
                    .get("cf-connecting-ip")
                    .and_then(|v| v.to_str().ok())
                    .filter(|v| !v.is_empty())
            });
        let ip_permit = if let Some(ip) = raw_ip {
            let key = limit_key(ip);
            let mut guard = self.ws_per_ip.lock().ok()?;
            let current = guard.get(&key).copied().unwrap_or(0);
            if current >= MAX_WS_PER_IP {
                return None;
            }
            if current == 0 && guard.len() >= MAX_TRACKED_WS_IPS {
                return None;
            }
            guard.insert(key.clone(), current + 1);
            Some(WsIpPermit {
                counts: self.ws_per_ip.clone(),
                key,
            })
        } else {
            None
        };
        let global = self.ws_slots.clone().try_acquire_owned().ok()?;
        Some(WsPermit {
            _global: global,
            _per_ip: ip_permit,
        })
    }

    pub fn next_peer_id(&self) -> u64 {
        self.next_peer.fetch_add(1, Ordering::Relaxed)
    }
}
