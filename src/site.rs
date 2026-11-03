//! 各サービスの URL と役割。URL は `Config` だけが持ち、API の応答もここから作る
//!
//! - auth: だれなのかを確かめる (ログイン、トークンの発行、OAuth の認可)
//! - api: データを読み書きする (REST API。このサービス)
//! - mcp: AI クライアントがつなぐ (Remote MCP)

use serde_json::{Value, json};

use crate::config::Config;
use crate::util::encode_uri_component;

const ROLES_PAGE: &str = "サービスのURLと役割";

impl Config {
    /// 機械向けの URL の一覧
    pub fn services(&self) -> Value {
        json!({ "auth": self.issuer, "api": self.api_url, "mcp": self.mcp_url })
    }

    /// 人向けの、URL と役割のページ。末尾の / まで付ける (無いと Wiki が 301 で付け直す)
    pub fn roles_page(&self) -> String {
        format!(
            "{}/wiki/{}/",
            self.wiki_url,
            encode_uri_component(ROLES_PAGE)
        )
    }
}
