//! TORO Server の REST API (api.torosaba.net)。URL と役割は `site` を見てください。
//!
//! - `api`: 入り口 (URL の一覧と、トークンの持ち主)
//! - `wiki`: GitHub 上の Wiki 原稿の読み書きと検索
//! - `minecraft`: Minecraft サーバーの稼働状態の取得
//! - `openapi`: OpenAPI 仕様
//! - `identity`: Bearer トークンの確認 (auth に尋ねる)
//!
//! 共通の部品は `config` / `site` / `http` / `util`、URL から各機能への振り分けは `routes` にある。

pub mod api;
pub mod config;
pub mod http;
pub mod identity;
pub mod minecraft;
pub mod openapi;
pub mod routes;
pub mod site;
pub mod util;
pub mod wiki;

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
}

impl App {
    pub fn new(cfg: Config) -> Result<Self, reqwest::Error> {
        let wiki_index_ttl = Duration::from_secs(cfg.wiki_index_ttl_secs);
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .user_agent("TORO-API")
            .build()?;
        Ok(Self {
            cfg,
            http,
            requests: Semaphore::new(MAX_CONCURRENT_REQUESTS),
            wiki_index: wiki::search::Index::new(wiki_index_ttl),
            server_status: Mutex::new(None),
        })
    }
}
