//! 3D 地図 (BlueMap) の API。移管前の TORO-WebMap のサーバー側 (toro-webmap) の 3D の部分の置き換え。
//!
//! 3D の「唯一の情報元」は BlueMap のバックエンド (TiDB の `bluemap` データベースと、toro-main の内蔵サーバー) で、
//! 唯一の読み手がこのサービス。
//!
//! - `store`: `bluemap` データベースの読み取り (SELECT だけ)
//! - `maps`: `/map3d/v1/maps/{地図}/...` (設定、テクスチャ、タイル、プレイヤーの顔、ライブ情報)
//! - `live`: toro-main の内蔵サーバーからの取得 (プレイヤー、マーカー)
//! - `hub`: toro-main の SSE を地図ごとに 1 本だけ購読し、WebSocket と SSE の閲覧者へ配る
//! - `ws`: `/map3d/v1/ws` (WebSocket)
//! - `sign`: `/map3d/v1/marumasign/...` (看板データ、転送先の確認、中継)
//! - `mcassets`: `/map3d/v1/mcassets/...` (構造物の看板を描くための Minecraft のリソース)
//! - `assets`: `/_assets/{id}.{拡張子}` (MarumaSign Companion がホストした看板の画像など)
//! - `clients`: いまつながっているプレイヤーの IP (`/live/clients`。内部専用の待受だけ)
//! - `budget`: キャッシュのメモリの上限 (一元管理)

pub mod assets;
pub mod budget;
pub mod clients;
pub mod hub;
pub mod live;
pub mod maps;
pub mod markers;
pub mod mcassets;
pub mod sign;
pub mod store;
pub mod ws;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::body::Body;
use axum::http::header::{self, HeaderMap, HeaderValue};
use axum::http::{Method, StatusCode};
use axum::response::Response;
use md5::{Digest, Md5};
use serde_json::json;

use crate::config::{optional, or_default};
use crate::http::Ctx;
use crate::map::MapService;
use store::BluemapStore;

/// 画面 (別オリジン) の JavaScript が読めるようにするヘッダー
pub const EXPOSE_HEADERS: &str = "X-MarumaSign-Url, X-MarumaSign-Type, X-MarumaSign-Kind, Accept-Ranges, Content-Range, Content-Length";
const ALLOW_HEADERS: &str = "Range, Content-Type, If-None-Match";

/// 3D 地図 API の設定。`MAP_BLUEMAP_DATABASE` が無ければ、3D は動かさない (`/map3d/v1/*` は 404)
#[derive(Clone, Default)]
pub struct Map3dConfig {
    /// BlueMap の `bluemap` データベースの名前 (`MAP_BLUEMAP_DATABASE`)。無ければ 3D を動かさない
    pub bluemap_database: Option<String>,
    /// BlueMap 内蔵の Web サーバーの URL (末尾の / なし。`MAP_BLUEMAP_LIVE`)
    pub bluemap_live: String,
    /// BlueMap の地図 → マーカーの一覧へ地点を足す Dynmap のワールド (`MAP_BLUEMAP_DYNMAP_WORLDS`。`overworld=main,...`)
    pub dynmap_worlds: Vec<(String, String)>,
    /// MarumaSign Companion のデータベースの名前 (`MAP_ASSETS_DATABASE`)。無ければ `/_assets/` は動かさない
    pub assets_database: Option<String>,
    /// `/_assets/` を読んでよい地図のページの Origin (`MAP_ASSETS_ORIGINS`)
    pub assets_origins: Vec<String>,
    /// ログアウトしてからも、その IP から `/_assets/` を読めるようにしておく時間 (分。`MAP_ASSETS_GRACE_MINUTES`)
    pub grace_minutes: u64,
    /// TORO-Proxy が `/live/clients` に送るときの合言葉 (`MAP_CLIENTS_TOKEN`)。空なら受け付けない
    pub clients_token: String,
    pub mcassets: mcassets::McAssetsConfig,
    pub sign: sign::SignConfig,
}

impl Map3dConfig {
    pub fn from_env() -> Result<Self, String> {
        let list = |name: &str, default: &str| -> Vec<String> {
            or_default(name, default)
                .split(',')
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .collect()
        };
        let dynmap_worlds = list("MAP_BLUEMAP_DYNMAP_WORLDS", "overworld=main")
            .iter()
            .filter_map(|pair| {
                let (map, world) = pair.split_once('=')?;
                Some((map.trim().to_string(), world.trim().to_string()))
            })
            .collect();
        Ok(Self {
            bluemap_database: optional("MAP_BLUEMAP_DATABASE"),
            bluemap_live: or_default("MAP_BLUEMAP_LIVE", "http://toro-main:8100")
                .trim_end_matches('/')
                .to_string(),
            dynmap_worlds,
            assets_database: optional("MAP_ASSETS_DATABASE"),
            assets_origins: list(
                "MAP_ASSETS_ORIGINS",
                "https://map.torosaba.net,https://3d-map.torosaba.net",
            ),
            grace_minutes: or_default("MAP_ASSETS_GRACE_MINUTES", "10")
                .parse()
                .map_err(|_| "MAP_ASSETS_GRACE_MINUTES must be a number".to_string())?,
            clients_token: std::env::var("MAP_CLIENTS_TOKEN").unwrap_or_default(),
            mcassets: mcassets::McAssetsConfig::from_env()?,
            sign: sign::SignConfig::from_env()?,
        })
    }
}

/// Dynmap の地点の変換結果 (Dynmap のワールド → 読んだ時刻と、BlueMap のマーカーの組)
pub type DynmapCache =
    Mutex<HashMap<String, (Instant, serde_json::Map<String, serde_json::Value>)>>;

/// 3D 地図 API が持つもの
pub struct Map3d {
    pub cfg: Map3dConfig,
    /// 2D の地図 API (Dynmap の DB の読み取りと、WebSocket の枠を共用する)
    pub map: Arc<MapService>,
    pub store: Arc<dyn BluemapStore>,
    pub hub: Arc<hub::BlueHub>,
    pub live: live::Live,
    pub sign: Arc<sign::Sign>,
    pub mcassets: Arc<mcassets::McAssets>,
    /// `/_assets/`。`MAP_ASSETS_DATABASE` が無ければ None
    pub assets: Option<assets::Assets>,
    pub clients: Arc<clients::Clients>,
    /// Dynmap の地点の変換結果 (30 秒。BlueMap の地図の id に対応する Dynmap のワールドごと)
    pub dynmap_markers: DynmapCache,
}

impl Map3d {
    pub fn new(
        cfg: &crate::config::Config,
        map: Arc<MapService>,
        http: reqwest::Client,
    ) -> Option<Arc<Self>> {
        let database = cfg.map.db.as_ref()?;
        let bluemap = cfg.map3d.bluemap_database.as_ref()?;
        let store: Arc<dyn BluemapStore> =
            Arc::new(store::SqlxBluemapStore::new(database, bluemap));
        let assets = cfg.map3d.assets_database.as_ref().map(|name| {
            assets::Assets::new(
                Arc::new(assets::SqlxAssetStore::new(database, name)),
                cfg.map3d.assets_origins.clone(),
            )
        });
        Some(Self::with_parts(
            cfg.map3d.clone(),
            map,
            store,
            http,
            assets,
        ))
    }

    /// 部品を差し替えて作る (試験用)
    pub fn with_parts(
        cfg: Map3dConfig,
        map: Arc<MapService>,
        store: Arc<dyn BluemapStore>,
        http: reqwest::Client,
        assets: Option<assets::Assets>,
    ) -> Arc<Self> {
        let hub = hub::BlueHub::new(cfg.bluemap_live.clone());
        let live = live::Live::new(http.clone(), cfg.bluemap_live.clone());
        let sign = sign::Sign::new(
            &cfg.sign,
            http.clone(),
            store.clone(),
            cfg.bluemap_live.clone(),
        );
        let mcassets = mcassets::McAssets::new(&cfg.mcassets, http);
        let clients = clients::Clients::new(cfg.grace_minutes);
        Arc::new(Self {
            cfg,
            map,
            store,
            hub,
            live,
            sign,
            mcassets,
            assets,
            clients,
            dynmap_markers: Mutex::new(HashMap::new()),
        })
    }
}

/// `/map3d/v1/` 以下の経路か
pub fn is_path(path: &str) -> bool {
    path.starts_with("/map3d/v1/")
}

/// `/map3d/v1/ws` か (末尾の / は 1 つだけ無視する)
pub fn is_ws_path(path: &str) -> bool {
    path.strip_suffix('/').unwrap_or(path) == "/map3d/v1/ws"
}

/// `/map3d/v1/` 以下の振り分け。どの応答 (204、304、4xx、5xx を含む) にも `Access-Control-Allow-Origin` を付ける
pub async fn handle(svc: &Arc<Map3d>, ctx: &Ctx) -> Response {
    let mut res = route(svc, ctx).await;
    finish(&mut res);
    res
}

fn finish(res: &mut Response) {
    let headers = res.headers_mut();
    headers
        .entry(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        .or_insert(HeaderValue::from_static("*"));
    headers
        .entry(header::ACCESS_CONTROL_EXPOSE_HEADERS)
        .or_insert(HeaderValue::from_static(EXPOSE_HEADERS));
}

async fn route(svc: &Arc<Map3d>, ctx: &Ctx) -> Response {
    if ctx.method == Method::OPTIONS {
        return options();
    }
    if ctx.method != Method::GET && ctx.method != Method::HEAD {
        let mut res = error(405, "Method Not Allowed");
        res.headers_mut().insert(
            header::ALLOW,
            HeaderValue::from_static("GET, HEAD, OPTIONS"),
        );
        return res;
    }
    let rest = ctx.path.strip_prefix("/map3d/v1/").unwrap_or("");
    if let Some(path) = rest.strip_prefix("maps/") {
        return maps::handle(svc, ctx, path).await;
    }
    if let Some(path) = rest.strip_prefix("marumasign/") {
        return sign::handle(&svc.sign, ctx, path).await;
    }
    if let Some(path) = rest.strip_prefix("mcassets/") {
        return mcassets::handle(&svc.mcassets, ctx, path).await;
    }
    if rest.strip_suffix('/').unwrap_or(rest) == "ws" {
        return ws::upgrade_required();
    }
    error(404, "Not Found")
}

/// OPTIONS の応答 (事前確認。`Range` を許可する)
pub fn options() -> Response {
    let mut res = Response::new(Body::empty());
    *res.status_mut() = StatusCode::NO_CONTENT;
    let headers = res.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, HEAD, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static(ALLOW_HEADERS),
    );
    headers.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static(EXPOSE_HEADERS),
    );
    headers.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("86400"),
    );
    res
}

/// エラー (JSON)。`Cache-Control: no-cache`。`Access-Control-Allow-Origin` は `handle` が付ける
pub fn error(status: u16, message: &str) -> Response {
    let mut res = Response::new(Body::from(json!({ "error": message }).to_string()));
    *res.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let headers = res.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    res
}

/// 本文の MD5 の ETag (`"<hex>"`)
pub fn md5_etag(data: &[u8]) -> String {
    let mut etag = String::with_capacity(34);
    etag.push('"');
    for byte in Md5::digest(data) {
        etag.push_str(&format!("{byte:02x}"));
    }
    etag.push('"');
    etag
}

/// 返す本文 (ETag と 304 つき)
pub struct Body3 {
    pub data: Vec<u8>,
    pub content_type: &'static str,
    pub cache: &'static str,
    pub etag: Option<String>,
    /// `Content-Encoding` (保存済みの圧縮をそのまま返すとき)
    pub encoding: Option<&'static str>,
}

/// 本文を返す。`If-None-Match` が ETag と**まったく同じ**なら 304 (`W/` や複数の値は解かない。移管前と同じ)
pub fn respond(ctx: &Ctx, body: Body3) -> Response {
    let etag = body.etag.unwrap_or_else(|| md5_etag(&body.data));
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(body.content_type),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(body.cache));
    if let Ok(value) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, value);
    }
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    if let Some(encoding) = body.encoding {
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static(encoding));
        headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    let mut res = Response::new(Body::empty());
    *res.headers_mut() = headers;
    if ctx.header(header::IF_NONE_MATCH) == etag {
        *res.status_mut() = StatusCode::NOT_MODIFIED;
    } else {
        *res.body_mut() = Body::from(body.data);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert!(is_path("/map3d/v1/maps/overworld/settings.json"));
        assert!(!is_path("/map3d/v2/x"));
        assert!(!is_path("/map3d/v1"));
        assert!(is_ws_path("/map3d/v1/ws"));
        assert!(is_ws_path("/map3d/v1/ws/"));
        assert!(!is_ws_path("/map/v1/ws"));
    }

    #[test]
    fn options_allow_range_and_expose_headers() {
        let res = options();
        assert_eq!(res.status(), 204);
        let allow = res.headers()[header::ACCESS_CONTROL_ALLOW_HEADERS]
            .to_str()
            .unwrap();
        assert!(allow.contains("Range"));
        let expose = res.headers()[header::ACCESS_CONTROL_EXPOSE_HEADERS]
            .to_str()
            .unwrap();
        for name in [
            "X-MarumaSign-Url",
            "X-MarumaSign-Type",
            "X-MarumaSign-Kind",
            "Accept-Ranges",
            "Content-Range",
            "Content-Length",
        ] {
            assert!(expose.contains(name), "{name}");
        }
    }

    #[test]
    fn every_response_gets_cors_headers() {
        for status in [204u16, 304, 404, 500] {
            let mut res = error(status, "x");
            finish(&mut res);
            assert_eq!(res.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
            assert!(
                res.headers()
                    .contains_key(header::ACCESS_CONTROL_EXPOSE_HEADERS)
            );
        }
    }
}
