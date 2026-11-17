//! 地図 API の試験用の DB (SELECT の代わりに、メモリの表を引く)
#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use toro_api::App;
use toro_api::config::Config;
use toro_api::map::MapService;
use toro_api::map::hub::Hub;
use toro_api::map::store::{MapStore, StoreError, StoreResult, TileKey, TileRow};

#[derive(Default)]
pub struct FakeStore {
    pub files: Mutex<HashMap<String, String>>,
    pub tiles: Mutex<HashMap<String, TileRow>>,
    pub markers: Mutex<HashMap<String, String>>,
    pub icons: Mutex<HashMap<String, Vec<u8>>>,
    pub faces: Mutex<HashMap<(String, i32), Vec<u8>>>,
    /// 読んだ StandaloneFiles の名前 (順番どおり)
    pub reads: Mutex<Vec<String>>,
    /// 問い合わせたタイルの鍵 (`world|map|variant|x|y|zoom`)
    pub tile_queries: Mutex<Vec<String>>,
    pub marker_queries: Mutex<Vec<String>>,
    pub icon_queries: Mutex<Vec<String>>,
    pub face_queries: Mutex<Vec<(String, i32)>>,
    /// true の間は、どの問い合わせも失敗する
    pub failing: AtomicBool,
    /// 入れておくと、StandaloneFiles を読む問い合わせは、この枠をもらえるまで待つ (読んでいる最中の状態を作る)
    pub gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
}

pub fn tile_key(world: &str, map: &str, variant: &str, x: i64, y: i64, zoom: i64) -> String {
    format!("{world}|{map}|{variant}|{x}|{y}|{zoom}")
}

impl FakeStore {
    pub fn set_file(&self, name: &str, content: &str) {
        self.files
            .lock()
            .unwrap()
            .insert(name.to_string(), content.to_string());
    }

    pub fn remove_file(&self, name: &str) {
        self.files.lock().unwrap().remove(name);
    }

    pub fn set_failing(&self, failing: bool) {
        self.failing.store(failing, Ordering::SeqCst);
    }

    /// 名前の StandaloneFiles を読んだ回数
    pub fn read_count(&self, name: &str) -> usize {
        self.reads
            .lock()
            .unwrap()
            .iter()
            .filter(|n| *n == name)
            .count()
    }

    fn check(&self) -> StoreResult<()> {
        if self.failing.load(Ordering::SeqCst) {
            Err(StoreError("db down".into()))
        } else {
            Ok(())
        }
    }
}

impl MapStore for FakeStore {
    fn standalone_file<'a>(&'a self, name: &'a str) -> BoxFuture<'a, StoreResult<Option<String>>> {
        Box::pin(async move {
            self.reads.lock().unwrap().push(name.to_string());
            let gate = self.gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.acquire().await.unwrap().forget();
            }
            self.check()?;
            Ok(self.files.lock().unwrap().get(name).cloned())
        })
    }

    fn tile<'a>(&'a self, key: TileKey<'a>) -> BoxFuture<'a, StoreResult<Option<TileRow>>> {
        Box::pin(async move {
            let k = tile_key(key.world, key.map, key.variant, key.x, key.y, key.zoom);
            self.tile_queries.lock().unwrap().push(k.clone());
            self.check()?;
            Ok(self.tiles.lock().unwrap().get(&k).cloned())
        })
    }

    fn marker_file<'a>(&'a self, name: &'a str) -> BoxFuture<'a, StoreResult<Option<String>>> {
        Box::pin(async move {
            self.marker_queries.lock().unwrap().push(name.to_string());
            self.check()?;
            Ok(self.markers.lock().unwrap().get(name).cloned())
        })
    }

    fn marker_icon<'a>(&'a self, name: &'a str) -> BoxFuture<'a, StoreResult<Option<Vec<u8>>>> {
        Box::pin(async move {
            self.icon_queries.lock().unwrap().push(name.to_string());
            self.check()?;
            Ok(self.icons.lock().unwrap().get(name).cloned())
        })
    }

    fn face<'a>(
        &'a self,
        player: &'a str,
        type_id: i32,
    ) -> BoxFuture<'a, StoreResult<Option<Vec<u8>>>> {
        Box::pin(async move {
            self.face_queries
                .lock()
                .unwrap()
                .push((player.to_string(), type_id));
            self.check()?;
            Ok(self
                .faces
                .lock()
                .unwrap()
                .get(&(player.to_string(), type_id))
                .cloned())
        })
    }
}

pub const LIVE_TOKEN: &str = "test-live-token";

pub const CONFIG_JSON: &str = r#"{"worlds":[{"name":"main"},{"name":"flat"}]}"#;

pub fn update_json(timestamp: u64, players: &str) -> String {
    format!(r#"{{"currentcount": 0,"players": {players},"timestamp": {timestamp}}}"#)
}

/// 試験用の設定 (地図 API は、あとから足す)
pub fn base_config() -> Config {
    Config {
        port: 0,
        issuer: "https://auth.torosaba.net".into(),
        api_url: "https://api.torosaba.net".into(),
        mcp_url: "https://mcp.torosaba.net".into(),
        wiki_url: "https://wiki.torosaba.net".into(),
        auth_internal_url: "http://127.0.0.1:9".into(),
        cors_origins: vec!["https://wiki.torosaba.net".into()],
        github_token: "gh-token".into(),
        github_owner: "TORO-Server".into(),
        github_repo: "TORO-Wiki".into(),
        github_branch: "main".into(),
        github_api: "http://127.0.0.1:9".into(),
        wiki_index_ttl_secs: 300,
        body_read_timeout_secs: 30,
        mc_status_addr: "127.0.0.1:9".into(),
        mc_status_host: "torosaba.net".into(),
        map: Default::default(),
        map3d: Default::default(),
    }
}

pub struct MapServer {
    pub addr: SocketAddr,
    /// 内部専用の待受 (`/live/plugin`)
    pub internal: SocketAddr,
    pub store: Arc<FakeStore>,
    pub map: Arc<MapService>,
    pub client: reqwest::Client,
}

impl MapServer {
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub async fn get(&self, path: &str) -> reqwest::Response {
        self.client.get(self.url(path)).send().await.unwrap()
    }

    pub async fn get_with(&self, path: &str, name: &str, value: &str) -> reqwest::Response {
        self.client
            .get(self.url(path))
            .header(name, value)
            .send()
            .await
            .unwrap()
    }
}

/// 地図 API つきで起動する。`manual` なら、ハブは自動では読まない
pub async fn start_map(manual: bool) -> MapServer {
    start_map_with_limits(manual, 2048, 8).await
}

pub async fn start_map_with_limits(manual: bool, max_ws: usize, max_live: usize) -> MapServer {
    let store = Arc::new(FakeStore::default());
    store.set_file("dynmap_config.json", CONFIG_JSON);
    store.set_file("dynmap_main.json", &update_json(1, "[]"));
    store.set_file("dynmap_flat.json", &update_json(1, "[]"));
    let hub = if manual {
        Hub::manual(store.clone())
    } else {
        Hub::new(store.clone())
    };
    let map = MapService::with_limits(store.clone(), hub, LIVE_TOKEN.to_string(), max_ws, max_live);

    let mut app = App::new(base_config()).unwrap();
    app.map = Some(map.clone());
    let router = toro_api::routes::router(Arc::new(app));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let internal_router = toro_api::map::live::router(map.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let internal = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, internal_router).await.unwrap() });

    MapServer {
        addr,
        internal,
        store,
        map,
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
    }
}
