//! API の入り口: URL の一覧、仕様書、CORS、認証 (auth への問い合わせ)、稼働状態

mod common;

use axum::http::header;
use common::*;
use serde_json::{Value, json};

#[tokio::test]
async fn root_lists_every_service_from_the_config() {
    let server = start().await;
    let body: Value = server.get("/").await.json().await.unwrap();
    assert_eq!(body["service"], "TORO Server API");
    assert_eq!(body["services"]["auth"], "https://auth.torosaba.net");
    assert_eq!(body["services"]["api"], "https://api.torosaba.net");
    assert_eq!(body["services"]["mcp"], "https://mcp.torosaba.net");
    assert_eq!(body["openapi"], "https://api.torosaba.net/openapi.json");
    assert!(
        body["documentation"]
            .as_str()
            .unwrap()
            .starts_with("https://wiki.torosaba.net/wiki/")
    );

    let server = start_with(|cfg| {
        cfg.issuer = "https://auth.example.test".into();
        cfg.api_url = "https://api.example.test".into();
        cfg.mcp_url = "https://mcp.example.test".into();
        cfg.wiki_url = "https://wiki.example.test".into();
    })
    .await;
    let body: Value = server.get("/").await.json().await.unwrap();
    assert_eq!(body["services"]["auth"], "https://auth.example.test");
    assert_eq!(body["openapi"], "https://api.example.test/openapi.json");
    assert!(
        body["documentation"]
            .as_str()
            .unwrap()
            .starts_with("https://wiki.example.test/wiki/")
    );
}

#[tokio::test]
async fn answers_for_any_host_and_only_for_its_own_role() {
    let server = start().await;

    for host in ["api.torosaba.net", "localhost", "auth.torosaba.net"] {
        let res = server
            .client
            .get(server.url("/"))
            .header(header::HOST, host)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200, "{host}");
    }

    // 認証と MCP は、このサービスの役割ではない
    for path in [
        "/api/auth/me",
        "/api/auth/login",
        "/oauth/token",
        "/mcp",
        "/sse",
        "/introspect",
        "/api/docs/raw",
    ] {
        let res = server.get(path).await;
        assert_eq!(res.status(), 404, "{path}");
        let body: Value = res.json().await.unwrap();
        assert_eq!(body["error"], "Not Found");
        assert_eq!(body["path"], path);
    }
}

#[tokio::test]
async fn healthz_and_default_headers() {
    let server = start().await;
    let res = server.get("/healthz").await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(res.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(res.json::<Value>().await.unwrap(), json!({ "ok": true }));
}

#[tokio::test]
async fn errors_are_json_even_for_browsers() {
    let server = start().await;
    let res = server
        .client
        .get(server.url("/docs/raw"))
        .header(header::ACCEPT, "text/html")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    assert!(
        res.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
}

#[tokio::test]
async fn oversized_bodies_are_refused() {
    let server = start().await;
    let res = server
        .client
        .post(server.url("/docs/save"))
        .bearer_auth(MEMBER_TOKEN)
        .body(vec![b'x'; 2 * 1024 * 1024 + 1])
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 413);
}

#[tokio::test]
async fn slow_bodies_are_cut_off_and_the_slot_comes_back() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let server = start_with(|cfg| cfg.body_read_timeout_secs = 1).await;
    let address = server.base.strip_prefix("http://").unwrap();
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    // 宣言した長さのボディを送らないまま待つ
    stream
        .write_all(b"POST /docs/save HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\n")
        .await
        .unwrap();
    let mut head = [0u8; 12];
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read_exact(&mut head),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(&head, b"HTTP/1.1 408");
    assert_eq!(server.get("/healthz").await.status(), 200);
}

#[tokio::test]
async fn openapi_describes_every_endpoint_with_the_configured_url() {
    let server = start().await;
    let res = server.get("/openapi.json").await;
    assert_eq!(res.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    let spec: Value = res.json().await.unwrap();
    assert_eq!(spec["servers"][0]["url"], "https://api.torosaba.net");
    assert_eq!(
        spec["paths"]["/docs/save"]["post"]["operationId"],
        "saveWikiDoc"
    );
    assert_eq!(
        spec["paths"]["/docs/list"]["get"]["operationId"],
        "listWikiDocs"
    );
    assert_eq!(
        spec["paths"]["/docs/search"]["get"]["operationId"],
        "searchWikiDocs"
    );
    assert_eq!(
        spec["paths"]["/server/status"]["get"]["operationId"],
        "getServerStatus"
    );
    assert!(spec["paths"]["/docs/raw"]["get"].is_object());
    assert!(spec["paths"]["/me"]["get"].is_object());
    assert_eq!(spec["paths"].as_object().unwrap().len(), 6);

    let server = start_with(|cfg| cfg.api_url = "https://api.example.test".into()).await;
    let spec: Value = server.get("/openapi.json").await.json().await.unwrap();
    assert_eq!(spec["servers"][0]["url"], "https://api.example.test");
}

#[tokio::test]
async fn cors_trusts_only_configured_origins() {
    let server = start().await;

    let res = server
        .client
        .request(reqwest::Method::OPTIONS, server.url("/docs/save"))
        .header(header::ORIGIN, WIKI_ORIGIN)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
    assert_eq!(
        res.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        WIKI_ORIGIN
    );
    assert_eq!(
        res.headers()[header::ACCESS_CONTROL_ALLOW_CREDENTIALS],
        "true"
    );
    assert!(
        res.headers()[header::ACCESS_CONTROL_ALLOW_HEADERS]
            .to_str()
            .unwrap()
            .contains("Authorization")
    );

    for origin in ["https://assets.torosaba.net", "https://evil.example"] {
        let res = server
            .client
            .get(server.url("/"))
            .header(header::ORIGIN, origin)
            .send()
            .await
            .unwrap();
        assert_eq!(res.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
        assert!(
            !res.headers()
                .contains_key(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
        );
    }

    // auth 自身の Origin も信じる
    let res = server
        .client
        .get(server.url("/"))
        .header(header::ORIGIN, "https://auth.torosaba.net")
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "https://auth.torosaba.net"
    );
}

#[tokio::test]
async fn me_reports_the_owner_of_a_bearer_token() {
    let server = start().await;
    let me = |token: Option<&'static str>| {
        let mut request = server.client.get(server.url("/me"));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        async move { request.send().await.unwrap().json::<Value>().await.unwrap() }
    };

    let body = me(Some(MEMBER_TOKEN)).await;
    assert_eq!(body["authenticated"], true);
    assert_eq!(body["user"]["id"], "1001");
    assert_eq!(body["user"]["username"], "member_user");
    assert_eq!(body["user"]["can_edit"], true);

    for token in [None, Some("unknown-token")] {
        let body = me(token).await;
        assert_eq!(body["authenticated"], false, "{token:?}");
        assert!(body["user"].is_null());
    }

    // 尋ねたトークンが、そのまま auth に渡る
    assert_eq!(
        *server.auth.requests.lock().unwrap(),
        [MEMBER_TOKEN, "unknown-token"]
    );
}

#[tokio::test]
async fn cookies_are_never_read() {
    let server = start().await;
    // ブラウザのセッションがあっても、Bearer が無ければ誰でもない (auth へも尋ねない)
    let cookie = format!("__Host-toro_session={MEMBER_TOKEN}");

    let body: Value = server
        .client
        .get(server.url("/me"))
        .header(header::COOKIE, &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["authenticated"], false);

    let res = server
        .client
        .post(server.url("/docs/save"))
        .header(header::COOKIE, &cookie)
        .header(header::ORIGIN, WIKI_ORIGIN)
        .json(&json!({ "path": "a", "content": "x" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    assert!(server.github.commits.lock().unwrap().is_empty());
    assert!(server.auth.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_inactive_token_is_a_401_not_an_anonymous_save() {
    let server = start().await;
    let res = server
        .save(
            Some("revoked-token"),
            &json!({ "path": "a", "content": "x" }),
        )
        .await;
    assert_eq!(res.status(), 401);
    assert_eq!(
        res.json::<Value>().await.unwrap()["error"],
        "Authentication required (Bearer token)"
    );
    assert!(server.github.commits.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_token_without_an_id_is_not_accepted() {
    let server = start().await;
    server.register_token("no-id", json!({ "username": "x", "can_edit": true }));
    let res = server
        .save(Some("no-id"), &json!({ "path": "a", "content": "x" }))
        .await;
    assert_eq!(res.status(), 401);
}

#[tokio::test]
async fn a_failing_auth_service_is_a_503_and_never_a_401() {
    let server = start().await;
    server.break_auth(true);

    let res = server
        .client
        .get(server.url("/me"))
        .bearer_auth(MEMBER_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 503);
    // トークンが無ければ、auth には尋ねない
    assert_eq!(server.get("/me").await.status(), 200);

    let res = server
        .save(Some(MEMBER_TOKEN), &json!({ "path": "a", "content": "x" }))
        .await;
    assert_eq!(res.status(), 503);
    assert_eq!(
        res.json::<Value>().await.unwrap()["error"],
        "Authentication service unavailable"
    );
    assert!(server.github.commits.lock().unwrap().is_empty());

    // 読み取りは auth を使わない
    server.put_file("docs/a.md", "abc", "本文");
    assert_eq!(server.get("/docs/raw?path=a").await.status(), 200);

    server.break_auth(false);
    let res = server
        .save(Some(MEMBER_TOKEN), &json!({ "path": "a", "content": "x" }))
        .await;
    assert_eq!(res.status(), 200);
}

#[tokio::test]
async fn an_unreachable_auth_service_is_a_503() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let server = start_with(|cfg| cfg.auth_internal_url = dead).await;

    let res = server.get("/me").await;
    assert_eq!(res.status(), 200);
    let res = server
        .client
        .get(server.url("/me"))
        .bearer_auth(MEMBER_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 503);
}

#[tokio::test]
async fn server_status_reports_offline_when_unreachable() {
    let server = start().await;
    let res = server.get("/server/status").await;
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(
        body,
        json!({
            "online": false,
            "version": "",
            "players": { "online": 0, "max": 0, "names": [] },
            "motd": [],
        })
    );
}

#[tokio::test]
async fn server_status_reports_a_running_server() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut stream, _) = listener.accept().await.unwrap();
        // ハンドシェイクと状態の要求を読み捨てる
        let mut length = [0u8; 1];
        stream.read_exact(&mut length).await.unwrap();
        let mut handshake = vec![0u8; length[0] as usize];
        stream.read_exact(&mut handshake).await.unwrap();
        let mut request = [0u8; 2];
        stream.read_exact(&mut request).await.unwrap();
        let text = r#"{"version":{"name":"Velocity 26.3"},"players":{"online":1,"max":100,"sample":[{"name":"a","id":"069a79f4-44e9-4726-a5be-fca90e38aaf5"}]},"description":"§aTORO"}"#;
        let mut body = vec![
            0x00,
            text.len() as u8 & 0x7f | 0x80,
            (text.len() >> 7) as u8,
        ];
        body.extend_from_slice(text.as_bytes());
        let mut packet = vec![];
        let length = body.len();
        packet.push(length as u8 & 0x7f | 0x80);
        packet.push((length >> 7) as u8);
        packet.extend_from_slice(&body);
        stream.write_all(&packet).await.unwrap();
    });
    let server = start_with(|cfg| cfg.mc_status_addr = addr).await;

    let body: Value = server.get("/server/status").await.json().await.unwrap();
    assert_eq!(body["online"], true);
    assert_eq!(body["version"], "Velocity 26.3");
    assert_eq!(body["players"]["online"], 1);
    assert_eq!(body["players"]["max"], 100);
    assert_eq!(body["players"]["names"], json!(["a"]));
    assert_eq!(body["motd"], json!(["TORO"]));

    // 続けて聞かれても、つなぎ直さない (偽のサーバーは 1 回しか受けない)
    let again: Value = server.get("/server/status").await.json().await.unwrap();
    assert_eq!(again, body);
}
