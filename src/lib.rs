//! TORO Server の REST API (api.torosaba.net)。URL と役割は `site` を見てください。
//!
//! - `api`: 入り口 (URL の一覧と、トークンの持ち主)
//! - `wiki`: GitHub 上の Wiki 原稿の読み書きと検索
//! - `minecraft`: Minecraft サーバーの稼働状態の取得
//! - `map`: 2D 地図の API (`/map/v1/*`。Dynmap の DB を読む) と、dynmap-live からの位置の受信 (内部専用)
//! - `openapi`: OpenAPI 仕様
//! - `identity`: Bearer トークンの確認 (auth に尋ねる)
//!
//! 共通の部品は `config` / `site` / `http` / `util`、URL から各機能への振り分けは `routes` にある。

pub mod api;
pub mod config;
pub mod http;
pub mod identity;
pub mod map;
pub mod minecraft;
pub mod openapi;
pub mod routes;
pub mod site;
pub mod util;
pub mod wiki;

use std::sync::Arc;
use std::time::{Duration, Instant};

use config::Config;
use serde_json::Value;
use tokio::sync::{Mutex, Semaphore};

/// 同時に処理するリクエストの上限 (ボディは最大 2 MiB なので、メモリは約 512 MiB まで)
const MAX_CONCURRENT_REQUESTS: usize = 256;

/// リクエストをまたいで共有するもの
pub struct App {
    pub cfg: Config,
    pub http: reqwest::Client,
    pub requests: Semaphore,
    pub wiki_index: wiki::search::Index,
    /// 最後に尋ねた Minecraft サーバーの稼働状態 (要求のたびにつながないため)
    pub server_status: Mutex<Option<(Instant, Value)>>,
    /// 地図 API。MAP_DB_HOST が無ければ None (/map/v1/* は 404)
    pub map: Option<Arc<map::MapService>>,
}

impl App {
    pub fn new(cfg: Config) -> Result<Self, reqwest::Error> {
        let wiki_index_ttl = Duration::from_secs(cfg.wiki_index_ttl_secs);
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .user_agent("TORO-API")
            .build()?;
        // 接続は最初の問い合わせまで張らない (DB が先に起動していなくても、サービスは立ち上がる)
        let map = cfg.map.db.as_ref().map(|db| {
            map::MapService::new(
                Arc::new(map::store::SqlxStore::new(db)),
                cfg.map.live_token.clone(),
            )
        });
        Ok(Self {
            cfg,
            http,
            requests: Semaphore::new(MAX_CONCURRENT_REQUESTS),
            wiki_index: wiki::search::Index::new(wiki_index_ttl),
            server_status: Mutex::new(None),
            map,
        })
    }
}
