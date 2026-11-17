//! 環境変数からの設定。秘密でない値の既定は、本番の値にしてある (秘密だけを env ファイルに書けばよい)

#[derive(Clone)]
pub struct Config {
    pub port: u16,
    /// 認可サーバーの公開 URL (末尾の / なし)
    pub issuer: String,
    /// REST API の公開 URL (末尾の / なし)
    pub api_url: String,
    /// MCP サーバーの公開 URL (末尾の / なし)
    pub mcp_url: String,
    /// Wiki の公開 URL (末尾の / なし)
    pub wiki_url: String,
    /// トークンの確認を頼む、auth の内部の URL (末尾の / なし。公開しない)
    pub auth_internal_url: String,
    /// Cookie 付きで API を呼べる Origin (他のサブドメインは信じない)
    pub cors_origins: Vec<String>,

    pub github_token: String,
    pub github_owner: String,
    pub github_repo: String,
    pub github_branch: String,
    pub github_api: String,
    /// Wiki の検索が、原稿の変更を見に行く間隔
    pub wiki_index_ttl_secs: u64,
    /// リクエストのボディを読み切るまで待つ秒数。遅い送信で同時処理の枠を埋めさせない
    pub body_read_timeout_secs: u64,

    /// 稼働状態を尋ねる先 (host:port)
    pub mc_status_addr: String,
    /// 尋ねるときに名乗るホスト名 (Velocity の forced-hosts 用)
    pub mc_status_host: String,

    /// 地図 API (/map/v1 と /live/plugin) の設定
    pub map: MapConfig,
    /// 3D 地図 API (/map3d/v1、/_assets、/live/clients) の設定
    pub map3d: crate::map3d::Map3dConfig,
}

/// 地図 API が読む Dynmap の DB (TiDB)。読み取り専用の利用者を使う
#[derive(Clone)]
pub struct MapDb {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    /// Dynmap のデータベース名
    pub database: String,
}

/// 地図 API の設定。`MAP_DB_HOST` が無ければ、地図 API は動かさない (`/map/v1/*` は 404)
#[derive(Clone, Default)]
pub struct MapConfig {
    pub db: Option<MapDb>,
    /// `/live/plugin` を受ける内部専用の待受 (host:port)。無ければ内部の待受を起動しない
    pub live_addr: Option<String>,
    /// `/live/plugin` の `X-Toro-Token` の期待値。空ならどんな値も通さない
    pub live_token: String,
}

impl MapConfig {
    pub fn from_env() -> Result<Self, String> {
        let db = match optional("MAP_DB_HOST") {
            None => None,
            Some(host) => Some(MapDb {
                host,
                port: or_default("MAP_DB_PORT", "4000")
                    .parse()
                    .map_err(|_| "MAP_DB_PORT must be a number".to_string())?,
                user: optional("MAP_DB_USER").ok_or_else(|| {
                    "missing environment variables: MAP_DB_USER (required when MAP_DB_HOST is set)"
                        .to_string()
                })?,
                // 空のパスワードも許す (開発用の TiDB)
                password: std::env::var("MAP_DB_PASSWORD").unwrap_or_default(),
                database: or_default("MAP_DYNMAP_DATABASE", "dynmap"),
            }),
        };
        let live_addr = optional("MAP_LIVE_ADDR");
        if live_addr.is_some() && db.is_none() {
            return Err("MAP_LIVE_ADDR requires MAP_DB_HOST".to_string());
        }
        Ok(Self {
            db,
            live_addr,
            live_token: std::env::var("MAP_LIVE_TOKEN").unwrap_or_default(),
        })
    }
}

pub(crate) fn optional(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

pub(crate) fn or_default(name: &str, default: &str) -> String {
    optional(name).unwrap_or_else(|| default.to_string())
}

fn trimmed(name: &str, default: &str) -> String {
    or_default(name, default).trim_end_matches('/').to_string()
}

impl Config {
    /// Cookie 付きのリクエストを受けてよい送り元か (auth と、指定の Origin)
    pub fn is_trusted_origin(&self, origin: &str) -> bool {
        origin == self.issuer || self.cors_origins.iter().any(|o| o == origin)
    }

    pub fn from_env() -> Result<Self, String> {
        // 必須の環境変数。足りないものは、まとめてエラーにする
        let mut missing = Vec::new();
        let mut required = |name: &'static str| {
            optional(name).unwrap_or_else(|| {
                missing.push(name);
                String::new()
            })
        };
        let github_token = required("GITHUB_TOKEN");
        let auth_internal_url = required("AUTH_INTERNAL_URL")
            .trim_end_matches('/')
            .to_string();
        let mc_status_addr = required("MC_STATUS_ADDR");
        if !missing.is_empty() {
            return Err(format!(
                "missing environment variables: {}",
                missing.join(", ")
            ));
        }

        Ok(Self {
            port: or_default("PORT", "8080")
                .parse()
                .map_err(|_| "PORT must be a number".to_string())?,
            issuer: trimmed("PUBLIC_AUTH_URL", "https://auth.torosaba.net"),
            api_url: trimmed("PUBLIC_API_URL", "https://api.torosaba.net"),
            mcp_url: trimmed("PUBLIC_MCP_URL", "https://mcp.torosaba.net"),
            wiki_url: trimmed("PUBLIC_WIKI_URL", "https://wiki.torosaba.net"),
            auth_internal_url,
            cors_origins: or_default("CORS_ORIGINS", "https://wiki.torosaba.net")
                .split(',')
                .map(|origin| origin.trim().to_string())
                .filter(|origin| !origin.is_empty())
                .collect(),
            github_token,
            github_owner: or_default("GITHUB_OWNER", "TORO-Server"),
            github_repo: or_default("GITHUB_REPO", "TORO-Wiki"),
            github_branch: or_default("GITHUB_BRANCH", "main"),
            github_api: trimmed("GITHUB_API", "https://api.github.com"),
            wiki_index_ttl_secs: 300,
            body_read_timeout_secs: 30,
            mc_status_addr,
            mc_status_host: or_default("MC_STATUS_HOST", "torosaba.net"),
            map: MapConfig::from_env()?,
            map3d: crate::map3d::Map3dConfig::from_env()?,
        })
    }
}
