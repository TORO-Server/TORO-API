//! 結合試験の共通部品: 偽の auth / GitHub と、試験用に起動した本体
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::to_bytes;
use axum::extract::Request;
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use toro_api::App;
use toro_api::config::Config;

pub const WIKI_ORIGIN: &str = "https://wiki.torosaba.net";

/// 編集できる利用者のトークン
pub const MEMBER_TOKEN: &str = "member-token";
/// 読むだけの利用者のトークン
pub const READONLY_TOKEN: &str = "readonly-token";
/// scope が read だけの、編集できる利用者のトークン
pub const READ_SCOPE_TOKEN: &str = "read-scope-token";

async fn spawn(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

fn json_response(status: u16, body: Value) -> Response {
    (
        StatusCode::from_u16(status).unwrap(),
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// auth の内部の `/introspect` の代わり。登録したトークンだけを有効にする
#[derive(Default)]
pub struct FakeAuth {
    pub tokens: Mutex<HashMap<String, Value>>,
    /// true の間は、何を聞かれても 500 を返す
    pub broken: AtomicBool,
    pub requests: Mutex<Vec<String>>,
}

async fn start_fake_auth(state: Arc<FakeAuth>) -> String {
    let router = Router::new().fallback(move |req: Request| {
        let state = state.clone();
        async move {
            let (parts, body) = req.into_parts();
            let body = to_bytes(body, usize::MAX).await.unwrap();
            if state.broken.load(Ordering::SeqCst) {
                return json_response(500, json!({ "error": "broken" }));
            }
            if parts.method != Method::POST || parts.uri.path() != "/introspect" {
                return json_response(404, json!({ "error": "not found" }));
            }
            let token = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|v| v["token"].as_str().map(str::to_string))
                .unwrap_or_default();
            state.requests.lock().unwrap().push(token.clone());
            match state.tokens.lock().unwrap().get(&token) {
                Some(claims) => json_response(200, json!({ "active": true, "claims": claims })),
                None => json_response(200, json!({ "active": false })),
            }
        }
    });
    spawn(router).await
}

/// 原稿を覚えておくだけの GitHub
#[derive(Default)]
pub struct FakeGitHub {
    /// エンコードされたままのパス → (sha, 本文)
    pub files: Mutex<HashMap<String, (String, String)>>,
    pub commits: Mutex<Vec<Value>>,
    pub requests: Mutex<Vec<String>>,
    /// ツリーに載せる原稿: パス → (sha, 本文)
    pub docs: Mutex<BTreeMap<String, (String, String)>>,
}

impl FakeGitHub {
    pub fn blob_requests(&self) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|uri| uri.contains("/git/blobs/"))
            .count()
    }
}

async fn start_fake_github(state: Arc<FakeGitHub>) -> String {
    let router = Router::new().fallback(move |req: Request| {
        let state = state.clone();
        async move {
            let (parts, body) = req.into_parts();
            let body = to_bytes(body, usize::MAX).await.unwrap();
            let path = parts.uri.path().to_string();
            state.requests.lock().unwrap().push(parts.uri.to_string());

            if path.ends_with("/git/trees/main") {
                let mut tree: Vec<Value> = state
                    .docs
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(path, (sha, _))| json!({ "path": path, "type": "blob", "sha": sha }))
                    .collect();
                tree.push(json!({ "path": "docs/image.png", "type": "blob", "sha": "png" }));
                tree.push(json!({ "path": "docs/01_About", "type": "tree", "sha": "dir" }));
                tree.push(json!({ "path": "README.md", "type": "blob", "sha": "readme" }));
                return json_response(200, json!({ "tree": tree }));
            }
            if let Some(sha) = path.split("/git/blobs/").nth(1) {
                let docs = state.docs.lock().unwrap();
                return match docs.values().find(|(s, _)| s == sha) {
                    Some((_, content)) => json_response(
                        200,
                        json!({ "sha": sha, "encoding": "base64", "content": STANDARD.encode(content) }),
                    ),
                    None => json_response(404, json!({ "message": "Not Found" })),
                };
            }
            let Some(file_path) = path.strip_prefix("/repos/TORO-Server/TORO-Wiki/contents/") else {
                return json_response(404, json!({ "message": "Not Found" }));
            };
            let file_path = file_path.to_string();
            if parts.method == Method::GET {
                let files = state.files.lock().unwrap();
                return match files.get(&file_path) {
                    Some((sha, content)) => {
                        // 本物と同じく、60 文字ごとに改行が入った base64
                        let encoded = STANDARD.encode(content);
                        let wrapped: Vec<String> = encoded
                            .as_bytes()
                            .chunks(60)
                            .map(|c| String::from_utf8_lossy(c).into_owned())
                            .collect();
                        json_response(
                            200,
                            json!({ "path": file_path, "sha": sha, "size": content.len(), "content": wrapped.join("\n") }),
                        )
                    }
                    None => json_response(404, json!({ "message": "Not Found" })),
                };
            }

            let payload: Value = serde_json::from_slice(&body).unwrap();
            let mut files = state.files.lock().unwrap();
            if let Some((sha, _)) = files.get(&file_path)
                && payload["sha"].as_str() != Some(sha)
            {
                return json_response(
                    409,
                    json!({ "message": format!("{file_path} does not match {sha}") }),
                );
            }
            let new_sha = format!("sha-{}", state.commits.lock().unwrap().len() + 1);
            let content =
                String::from_utf8(STANDARD.decode(payload["content"].as_str().unwrap()).unwrap())
                    .unwrap();
            files.insert(file_path, (new_sha.clone(), content));
            state.commits.lock().unwrap().push(payload);
            json_response(
                200,
                json!({ "commit": { "sha": "commit-1" }, "content": { "sha": new_sha } }),
            )
        }
    });
    spawn(router).await
}

pub struct TestServer {
    pub base: String,
    /// api として話す。リダイレクトは追わない
    pub client: reqwest::Client,
    pub github: Arc<FakeGitHub>,
    pub auth: Arc<FakeAuth>,
}

pub async fn start() -> TestServer {
    start_with(|_| {}).await
}

pub async fn start_with(customize: impl FnOnce(&mut Config)) -> TestServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());

    let github = Arc::new(FakeGitHub::default());
    github.docs.lock().unwrap().extend([
        (
            "docs/01_About/a.md".to_string(),
            (
                "sha-a".to_string(),
                "# 概要`nTrainCarts と 電車 の話".to_string(),
            ),
        ),
        (
            "docs/02_Guide/b.md".to_string(),
            (
                "sha-b".to_string(),
                "# ガイド`nmarumasign の使い方".to_string(),
            ),
        ),
    ]);

    let auth = Arc::new(FakeAuth::default());
    let server = TestServer {
        base: base.clone(),
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        github: github.clone(),
        auth: auth.clone(),
    };
    server.register_token(
        MEMBER_TOKEN,
        json!({ "id": "1001", "username": "member_user", "global_name": null, "can_edit": true, "roles": ["role-a"] }),
    );
    server.register_token(
        READONLY_TOKEN,
        json!({ "id": "9", "username": "ro", "can_edit": false }),
    );
    server.register_token(
        READ_SCOPE_TOKEN,
        json!({ "id": "1001", "username": "member_user", "can_edit": true, "scope": "read" }),
    );

    let mut cfg = Config {
        port: 0,
        issuer: "https://auth.torosaba.net".into(),
        api_url: "https://api.torosaba.net".into(),
        mcp_url: "https://mcp.torosaba.net".into(),
        wiki_url: WIKI_ORIGIN.into(),
        auth_internal_url: start_fake_auth(auth).await,
        cors_origins: vec![WIKI_ORIGIN.into()],
        github_token: "gh-token".into(),
        github_owner: "TORO-Server".into(),
        github_repo: "TORO-Wiki".into(),
        github_branch: "main".into(),
        github_api: start_fake_github(github).await,
        wiki_index_ttl_secs: 300,
        body_read_timeout_secs: 30,
        // 誰も待っていないポート
        mc_status_addr: "127.0.0.1:9".into(),
        mc_status_host: "torosaba.net".into(),
    };
    customize(&mut cfg);

    let router = toro_api::routes::router(Arc::new(App::new(cfg).unwrap()));
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    server
}

impl TestServer {
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub async fn get(&self, path: &str) -> reqwest::Response {
        self.client.get(self.url(path)).send().await.unwrap()
    }

    /// auth が有効だと答えるトークンを足す
    pub fn register_token(&self, token: &str, claims: Value) {
        self.auth
            .tokens
            .lock()
            .unwrap()
            .insert(token.to_string(), claims);
    }

    /// auth の応答を壊す (true) / 直す (false)
    pub fn break_auth(&self, broken: bool) {
        self.auth.broken.store(broken, Ordering::SeqCst);
    }

    pub fn put_file(&self, encoded_path: &str, sha: &str, content: &str) {
        self.github.files.lock().unwrap().insert(
            encoded_path.to_string(),
            (sha.to_string(), content.to_string()),
        );
    }

    pub async fn save(&self, token: Option<&str>, body: &Value) -> reqwest::Response {
        let mut request = self.client.post(self.url("/docs/save")).json(body);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        request.send().await.unwrap()
    }
}

pub fn location(res: &reqwest::Response) -> String {
    res.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .to_string()
}
