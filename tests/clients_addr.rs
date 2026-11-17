//! 追加の内部専用の待受 (`MAP_CLIENTS_ADDR`): `POST /live/clients` と `GET /healthz` だけを受ける

mod common;

use std::sync::Arc;

use common::fake_map::*;
use futures_util::future::BoxFuture;
use toro_api::map::MapService;
use toro_api::map::hub::Hub;
use toro_api::map::live::clients_router;
use toro_api::map3d::store::{BlueData, BluemapStore};
use toro_api::map3d::{Map3d, Map3dConfig};

const TOKEN: &str = "clients-secret";

struct EmptyBlue;

impl BluemapStore for EmptyBlue {
    fn tile<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: i64,
        _: i64,
    ) -> BoxFuture<'a, toro_api::map::store::StoreResult<Option<BlueData>>> {
        Box::pin(async { Ok(None) })
    }
    fn item<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, toro_api::map::store::StoreResult<Option<BlueData>>> {
        Box::pin(async { Ok(None) })
    }
    fn map_exists<'a>(
        &'a self,
        _: &'a str,
    ) -> BoxFuture<'a, toro_api::map::store::StoreResult<bool>> {
        Box::pin(async { Ok(false) })
    }
    fn map_ids<'a>(&'a self) -> BoxFuture<'a, toro_api::map::store::StoreResult<Vec<String>>> {
        Box::pin(async { Ok(vec![]) })
    }
}

struct Extra {
    base: String,
    map3d: Arc<Map3d>,
    client: reqwest::Client,
}

async fn start() -> Extra {
    let store = Arc::new(FakeStore::default());
    let map = MapService::with_limits(store.clone(), Hub::manual(store), "live".to_string(), 16, 4);
    let cfg = Map3dConfig {
        clients_token: TOKEN.to_string(),
        grace_minutes: 10,
        ..Default::default()
    };
    let map3d = Map3d::with_parts(cfg, map, Arc::new(EmptyBlue), reqwest::Client::new(), None);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = clients_router(map3d.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Extra {
        base,
        map3d,
        client: reqwest::Client::new(),
    }
}

impl Extra {
    async fn post(&self, token: Option<&str>, body: &str) -> reqwest::Response {
        let mut req = self
            .client
            .post(format!("{}/live/clients", self.base))
            .body(body.to_string());
        if let Some(token) = token {
            req = req.header("x-toro-token", token);
        }
        req.send().await.unwrap()
    }

    async fn players(&self) -> u64 {
        let res = self
            .client
            .get(format!("{}/healthz", self.base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let body: serde_json::Value = res.json().await.unwrap();
        assert_eq!(body["ok"], true);
        body["players"].as_u64().unwrap()
    }
}

#[tokio::test]
async fn missing_or_wrong_token_is_403() {
    let s = start().await;
    assert_eq!(
        s.post(None, r#"{"ips":["203.0.113.7"]}"#).await.status(),
        403
    );
    assert_eq!(
        s.post(Some("wrong"), r#"{"ips":["203.0.113.7"]}"#)
            .await
            .status(),
        403
    );
    assert_eq!(s.players().await, 0);
}

#[tokio::test]
async fn correct_token_is_204_and_healthz_counts_players_only() {
    let s = start().await;
    let res = s
        .post(Some(TOKEN), r#"{"ips":["203.0.113.7","203.0.113.8"]}"#)
        .await;
    assert_eq!(res.status(), 204);
    assert!(s.map3d.clients.is_player("203.0.113.7"));
    assert_eq!(s.players().await, 2);
    // 人数だけで、IP は出さない
    let text = s
        .client
        .get(format!("{}/healthz", s.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!text.contains("203.0.113"));
}

#[tokio::test]
async fn empty_array_is_204_and_keeps_the_grace() {
    let s = start().await;
    s.post(Some(TOKEN), r#"{"ips":["203.0.113.7"]}"#).await;
    assert_eq!(s.post(Some(TOKEN), r#"{"ips":[]}"#).await.status(), 204);
    assert_eq!(s.players().await, 0);
    // 猶予の間は、まだ読める
    assert!(s.map3d.clients.is_player("203.0.113.7"));
}

#[tokio::test]
async fn other_paths_are_404() {
    let s = start().await;
    for path in [
        "/",
        "/live/plugin",
        "/map/v1/configuration",
        "/_assets/abc.png",
        "/live/",
    ] {
        let res = s
            .client
            .get(format!("{}{path}", s.base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404, "{path}");
    }
}
