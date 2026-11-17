//! `/map3d/v1/marumasign/...`: 看板データ、転送先の確認 (resolve)、中継 (proxy)。(実装は別のコミットで置き換える)

pub mod core;
pub mod manifest;
pub mod xml_head;

use std::sync::Arc;

use axum::response::Response;

use super::store::BluemapStore;
use crate::http::Ctx;

/// 看板中継の設定 (環境変数 `MAP_MARUMASIGN_*`)
#[derive(Clone, Default)]
pub struct SignConfig {}

impl SignConfig {
    pub fn from_env() -> Result<Self, String> {
        Ok(Self {})
    }
}

pub struct Sign {}

impl Sign {
    pub fn new(
        _cfg: &SignConfig,
        _http: reqwest::Client,
        _store: Arc<dyn BluemapStore>,
        _live_url: String,
    ) -> Arc<Self> {
        Arc::new(Self {})
    }
}

/// `rest` は `/map3d/v1/marumasign/` より後ろ (`resolve`、`proxy`、`<地図>.json`)
pub async fn handle(_sign: &Arc<Sign>, _ctx: &Ctx, _rest: &str) -> Response {
    super::error(404, "Not Found")
}
