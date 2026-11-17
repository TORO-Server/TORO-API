//! `/map3d/v1/mcassets/...`: 構造物の看板を描くための Minecraft のリソース。(実装は別のコミットで置き換える)

use std::sync::Arc;

use axum::response::Response;

use crate::http::Ctx;

/// mcassets の設定 (環境変数 `MAP_MCASSETS_*`)
#[derive(Clone, Default)]
pub struct McAssetsConfig {}

impl McAssetsConfig {
    pub fn from_env() -> Result<Self, String> {
        Ok(Self {})
    }
}

pub struct McAssets {}

impl McAssets {
    /// jar からの生成は、ここで背景の仕事として始める
    pub fn new(_cfg: &McAssetsConfig, _http: reqwest::Client) -> Arc<Self> {
        Arc::new(Self {})
    }
}

/// `rest` は `/map3d/v1/mcassets/` より後ろ (`index.json`、`skin`、`<hash>/...`)
pub async fn handle(_mc: &Arc<McAssets>, _ctx: &Ctx, _rest: &str) -> Response {
    super::error(404, "Not Found")
}
