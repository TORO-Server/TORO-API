//! 2D 地図の API (Dynmap の DB を読んで返す)。移管前の TORO-WebMap のサーバー側 (toro-webmap) の置き換え。
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

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::Semaphore;

use hub::Hub;
use store::MapStore;

/// 同時につなげる WebSocket (ブラウザ) の数の上限。ふつうの要求の同時処理の枠とは別に数える
pub const MAX_WS_CONNECTIONS: usize = 2048;
/// 同時につなげる `/live/plugin` の数の上限 (本番では dynmap-live の 1 本)
pub const MAX_LIVE_CONNECTIONS: usize = 8;

pub struct MapService {
    pub store: Arc<dyn MapStore>,
    pub hub: Arc<Hub>,
    /// ブラウザとの WebSocket の枠
    pub ws_slots: Arc<Semaphore>,
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
            live_slots: Arc::new(Semaphore::new(max_live)),
            live_token,
            next_peer: AtomicU64::new(1),
        })
    }

    pub fn next_peer_id(&self) -> u64 {
        self.next_peer.fetch_add(1, Ordering::Relaxed)
    }
}
