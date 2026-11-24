//! WebSocket の統合試験: `/map/v1/ws` (Origin 検査、購読、レート制限、無視されるメッセージ、接続数の上限) と、
//! `/live/plugin` (内部専用の待受、トークン、位置の取り込み)

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use common::fake_map::*;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{Error, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn connect(
    addr: SocketAddr,
    path: &str,
    origin: Option<&str>,
    token: Option<&str>,
) -> Result<Client, Error> {
    let mut request = format!("ws://{addr}{path}").into_client_request().unwrap();
    if let Some(origin) = origin {
        request
            .headers_mut()
            .insert("Origin", origin.parse().unwrap());
    }
    if let Some(token) = token {
        request
            .headers_mut()
            .insert("X-Toro-Token", token.parse().unwrap());
    }
    connect_async(request).await.map(|(client, _)| client)
}

async fn browser(server: &MapServer) -> Client {
    connect(
        server.addr,
        "/map/v1/ws",
        Some("https://map.torosaba.net"),
        None,
    )
    .await
    .unwrap()
}

async fn plugin(server: &MapServer) -> Client {
    connect(server.internal, "/live/plugin", None, Some(LIVE_TOKEN))
        .await
        .unwrap()
}

/// 次のテキスト (期限までに来なければ None)
async fn next_text(client: &mut Client, wait: Duration) -> Option<String> {
    tokio::time::timeout(wait, async {
        loop {
            match client.next().await? {
                Ok(Message::Text(text)) => return Some(text.as_str().to_string()),
                Ok(Message::Ping(_) | Message::Pong(_)) => continue,
                _ => return None,
            }
        }
    })
    .await
    .ok()
    .flatten()
}

/// 期間のあいだに届いたテキストを全部集める
async fn collect(client: &mut Client, span: Duration) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + span;
    let mut out = Vec::new();
    while let Some(left) = deadline.checked_duration_since(tokio::time::Instant::now()) {
        match next_text(client, left).await {
            Some(text) => out.push(text),
            None => break,
        }
    }
    out
}

async fn send(client: &mut Client, text: &str) {
    client.send(Message::Text(text.into())).await.unwrap();
}

const SHORT: Duration = Duration::from_millis(500);
const LONG: Duration = Duration::from_secs(5);

#[tokio::test]
async fn subscribing_sends_the_current_update_at_once() {
    let server = start_map(false).await;
    let raw = update_json(5, "[]");
    server.store.set_file("dynmap_main.json", &raw);
    let mut client = browser(&server).await;
    send(&mut client, r#"{"subscribe":"main"}"#).await;
    let first = next_text(&mut client, LONG).await.unwrap();
    // 外側は `{"type":"update","world":"main","update":<原文>}`。原文はそのまま
    assert_eq!(
        first,
        format!(r#"{{"type":"update","world":"main","update":{raw}}}"#)
    );
}

#[tokio::test]
async fn updates_follow_the_timestamp_only() {
    let server = start_map(false).await;
    let mut client = browser(&server).await;
    send(&mut client, r#"{"subscribe":"main"}"#).await;
    assert!(next_text(&mut client, LONG).await.is_some());
    // 変わっていなければ、何も来ない (1 秒ごとに読むが、送らない)
    assert!(
        collect(&mut client, Duration::from_millis(2300))
            .await
            .is_empty()
    );
    // 変われば 1 秒以内に 1 度だけ来る
    let changed = update_json(2, "[]");
    server.store.set_file("dynmap_main.json", &changed);
    let got = collect(&mut client, Duration::from_millis(2300)).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].contains(&changed));
}

#[tokio::test]
async fn players_arrive_from_the_internal_plugin_and_are_not_repeated() {
    let server = start_map(false).await;
    server.store.set_file(
        "dynmap_main.json",
        &update_json(
            1,
            r#"[{"account":"a","world":"main","health":20,"armor":5}]"#,
        ),
    );
    let mut client = browser(&server).await;
    send(&mut client, r#"{"subscribe":"main"}"#).await;
    assert!(
        next_text(&mut client, LONG)
            .await
            .unwrap()
            .contains(r#""type":"update""#)
    );

    let mut plugin = plugin(&server).await;
    let positions = json!({ "t": "players", "players": [
        { "account": "a", "world": "main", "x": 1.0, "y": 64, "z": -2.5, "health": 18, "armor": 7 },
        { "account": "b", "world": "main", "x": 0, "y": 0, "z": 0, "health": 20, "armor": 0 }
    ]});
    send(&mut plugin, &positions.to_string()).await;
    let players = next_text(&mut client, LONG).await.unwrap();
    // JavaScript の `JSON.stringify` と同じ書き方 (1.0 は 1)。Dynmap が出していない b は載せない
    assert_eq!(
        players,
        r#"{"type":"players","players":[{"account":"a","world":"main","x":1,"y":64,"z":-2.5,"health":18,"armor":7}]}"#
    );
    // 同じ内容は送らない。ハートビートでも同じ
    send(&mut plugin, &positions.to_string()).await;
    send(&mut plugin, &positions.to_string()).await;
    assert!(next_text(&mut client, SHORT).await.is_none());
    // 変われば送る
    let moved = json!({ "t": "players", "players": [
        { "account": "a", "world": "main", "x": 2.0, "y": 64, "z": -2.5, "health": 18, "armor": 7 }
    ]});
    send(&mut plugin, &moved.to_string()).await;
    assert!(
        next_text(&mut client, LONG)
            .await
            .unwrap()
            .contains(r#""x":2,"#)
    );
}

#[tokio::test]
async fn disallowed_origin_is_closed_with_1008() {
    let server = start_map(false).await;
    for origin in [
        "https://example.invalid",
        "https://torosaba.net.evil.com",
        "https://a.b.torosaba.net",
        "ftp://map.torosaba.net",
    ] {
        let mut client = connect(server.addr, "/map/v1/ws", Some(origin), None)
            .await
            .unwrap();
        match client.next().await {
            Some(Ok(Message::Close(Some(frame)))) => {
                assert_eq!(frame.code, CloseCode::Policy, "{origin}");
                assert_eq!(frame.reason.as_str(), "Forbidden");
            }
            other => panic!("{origin}: {other:?}"),
        }
    }
}

#[tokio::test]
async fn allowed_origins_can_subscribe() {
    let server = start_map(false).await;
    for origin in [
        None,
        Some("http://localhost:3000"),
        Some("http://127.0.0.1"),
        Some("https://torosaba.net"),
        Some("https://map.torosaba.net"),
        Some("https://MAP.TOROSABA.NET"),
    ] {
        let mut client = connect(server.addr, "/map/v1/ws", origin, None)
            .await
            .unwrap();
        send(&mut client, r#"{"subscribe":"flat"}"#).await;
        assert!(next_text(&mut client, LONG).await.is_some(), "{origin:?}");
    }
}

#[tokio::test]
async fn more_than_five_messages_in_two_seconds_are_ignored() {
    let server = start_map(false).await;
    let mut client = browser(&server).await;
    for _ in 0..8 {
        send(&mut client, r#"{"subscribe":"main"}"#).await;
    }
    // 受けた 5 通の分だけ、すぐに update が返る
    let got = collect(&mut client, Duration::from_millis(1500)).await;
    assert_eq!(got.len(), 5, "{got:?}");
    // 窓を過ぎれば、また受ける
    tokio::time::sleep(Duration::from_millis(2100)).await;
    send(&mut client, r#"{"subscribe":"main"}"#).await;
    assert_eq!(collect(&mut client, SHORT).await.len(), 1);
}

#[tokio::test]
async fn unknown_world_is_refused_silently_and_the_connection_stays() {
    let server = start_map(false).await;
    server
        .store
        .set_file("dynmap_nosuch.json", &update_json(1, "[]"));
    let mut client = browser(&server).await;
    send(&mut client, r#"{"subscribe":"nosuch"}"#).await;
    assert!(
        collect(&mut client, Duration::from_millis(2500))
            .await
            .is_empty()
    );
    assert_eq!(
        server.store.read_count("dynmap_nosuch.json"),
        0,
        "DB を読まない"
    );
    // 閉じられていない: 同じ接続で、知っているワールドを購読できる
    send(&mut client, r#"{"subscribe":"main"}"#).await;
    assert!(next_text(&mut client, LONG).await.is_some());
}

#[tokio::test]
async fn messages_that_cannot_be_used_are_ignored() {
    let server = start_map(false).await;
    let mut client = browser(&server).await;
    let long_pad = "x".repeat(520);
    let name100 = "a".repeat(100);
    for text in [
        "not json".to_string(),
        r#"["subscribe","main"]"#.to_string(),
        r#"{"subscribe":1}"#.to_string(),
        r#"{"subscribe":""}"#.to_string(),
        format!(r#"{{"subscribe":"{name100}"}}"#),
        format!(r#"{{"subscribe":"main","pad":"{long_pad}"}}"#),
        // `{"bluemap":"<map>"}` は 3D 用で、この段階では未対応 (黙って無視する)
        r#"{"bluemap":"overworld"}"#.to_string(),
    ] {
        send(&mut client, &text).await;
    }
    assert!(
        collect(&mut client, Duration::from_millis(1500))
            .await
            .is_empty()
    );
    // 接続は生きていて、正しい購読は通る (レート制限の窓を過ぎてから)
    tokio::time::sleep(Duration::from_millis(2100)).await;
    send(&mut client, r#"{"subscribe":"main"}"#).await;
    assert!(next_text(&mut client, LONG).await.is_some());
}

#[tokio::test]
async fn a_message_over_256_kib_closes_the_connection() {
    let server = start_map(false).await;
    let mut client = browser(&server).await;
    let huge = format!(
        r#"{{"subscribe":"main","pad":"{}"}}"#,
        "x".repeat(300 * 1024)
    );
    let _ = client.send(Message::Text(huge.into())).await;
    let mut closed = false;
    for _ in 0..10 {
        match tokio::time::timeout(Duration::from_secs(2), client.next()).await {
            Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) | Ok(None) => {
                closed = true;
                break;
            }
            Ok(Some(Ok(_))) => continue,
            Err(_) => break,
        }
    }
    assert!(closed, "上限を超えたメッセージは、接続ごと断る");
}

#[tokio::test]
async fn plain_get_to_the_ws_path_is_426() {
    let server = start_map(false).await;
    let res = server.get("/map/v1/ws").await;
    assert_eq!(res.status(), 426);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"], "Upgrade Required");
}

#[tokio::test]
async fn connections_are_limited_separately_from_requests() {
    let server = start_map_with_limits(false, 2, 8).await;
    let a = browser(&server).await;
    let b = browser(&server).await;
    // 3 本目は断る
    let third = connect(
        server.addr,
        "/map/v1/ws",
        Some("https://map.torosaba.net"),
        None,
    )
    .await;
    match third {
        Err(Error::Http(res)) => assert_eq!(res.status(), 503),
        other => panic!("{:?}", other.map(|_| ())),
    }
    // ふつうの要求は、WebSocket の枠に関係なく通る
    assert_eq!(server.get("/map/v1/configuration").await.status(), 200);
    // 閉じれば枠が空く
    drop(a);
    drop(b);
    let mut ok = false;
    for _ in 0..20 {
        if connect(
            server.addr,
            "/map/v1/ws",
            Some("https://map.torosaba.net"),
            None,
        )
        .await
        .is_ok()
        {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ok);
}

#[tokio::test]
async fn closing_a_connection_removes_the_viewer() {
    let server = start_map(false).await;
    let mut client = browser(&server).await;
    send(&mut client, r#"{"subscribe":"main"}"#).await;
    assert!(next_text(&mut client, LONG).await.is_some());
    assert_eq!(server.map.hub.viewer_count(), 1);
    drop(client);
    for _ in 0..30 {
        if server.map.hub.viewer_count() == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("見ている人が残っている");
}

// ---- /live/plugin (内部専用)

#[tokio::test]
async fn live_plugin_requires_the_token() {
    let server = start_map(false).await;
    for token in [
        None,
        Some("wrong"),
        Some(""),
        Some(&format!("{LIVE_TOKEN}x")[..]),
    ] {
        match connect(server.internal, "/live/plugin", None, token).await {
            Err(Error::Http(res)) => assert_eq!(res.status(), 403, "{token:?}"),
            other => panic!("{token:?}: {:?}", other.map(|_| ())),
        }
    }
    assert!(
        connect(server.internal, "/live/plugin", None, Some(LIVE_TOKEN))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn live_plugin_with_an_empty_configured_token_refuses_everyone() {
    use toro_api::map::live::token_matches;
    assert!(!token_matches("", ""));
    assert!(!token_matches("x", ""));
}

#[tokio::test]
async fn live_plugin_ignores_broken_messages_and_other_paths_are_404() {
    let server = start_map(false).await;
    server.store.set_file(
        "dynmap_main.json",
        &update_json(
            1,
            r#"[{"account":"a","world":"main","health":20,"armor":5}]"#,
        ),
    );
    let mut client = browser(&server).await;
    send(&mut client, r#"{"subscribe":"main"}"#).await;
    next_text(&mut client, LONG).await.unwrap();
    let mut plugin = plugin(&server).await;
    for text in [
        "not json",
        r#"{"t":"other"}"#,
        r#"{"t":"players"}"#,
        r#"{"t":"players","players":1}"#,
        "[]",
    ] {
        send(&mut plugin, text).await;
    }
    assert!(next_text(&mut client, SHORT).await.is_none());
    send(
        &mut plugin,
        r#"{"t":"players","players":[{"account":"a","world":"main","x":1,"y":2,"z":3,"health":1,"armor":1}]}"#,
    )
    .await;
    assert!(
        next_text(&mut client, LONG)
            .await
            .unwrap()
            .contains(r#""account":"a""#)
    );

    // 内部の待受には、/live/plugin と /healthz だけ
    let base = format!("http://{}", server.internal);
    assert_eq!(
        server
            .client
            .get(format!("{base}/healthz"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        server
            .client
            .get(format!("{base}/map/v1/configuration"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    // WebSocket でない GET は 426 (トークンが合っていれば)
    let res = server
        .client
        .get(format!("{base}/live/plugin"))
        .header("X-Toro-Token", LIVE_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 426);
}

#[tokio::test]
async fn live_plugin_connections_are_limited() {
    let server = start_map_with_limits(false, 16, 1).await;
    let _first = plugin(&server).await;
    match connect(server.internal, "/live/plugin", None, Some(LIVE_TOKEN)).await {
        Err(Error::Http(res)) => assert_eq!(res.status(), 503),
        other => panic!("{:?}", other.map(|_| ())),
    }
}

#[tokio::test]
async fn rapid_resubscribe_leaves_only_one_active_subscription() {
    let server = start_map(false).await;
    let mut client = browser(&server).await;
    for world in ["main", "flat", "main", "flat"] {
        send(&mut client, &format!(r#"{{"subscribe":"{world}"}}"#)).await;
    }
    let msgs = collect(&mut client, LONG).await;
    assert_eq!(msgs.len(), 4);
    assert_eq!(server.map.hub.viewer_count(), 1);
}

#[tokio::test]
async fn per_ip_ws_limit_caps_concurrent_connections_for_same_client() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let server = start_map(false).await;
    let mut conns = Vec::new();
    for _ in 0..toro_api::map::MAX_WS_PER_IP {
        let mut req = format!("ws://{}/map/v1/ws", server.addr)
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("x-toro-client-ip", "198.51.100.42".parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        conns.push(ws);
    }
    let mut extra = format!("ws://{}/map/v1/ws", server.addr)
        .into_client_request()
        .unwrap();
    extra
        .headers_mut()
        .insert("x-toro-client-ip", "198.51.100.42".parse().unwrap());
    match tokio_tungstenite::connect_async(extra).await {
        Err(Error::Http(res)) => assert_eq!(res.status(), 503),
        other => panic!("{:?}", other.map(|_| ())),
    }
    // 別の IP からはそのまま繋げる
    assert!(browser(&server).await.send(Message::Ping(vec![].into())).await.is_ok());
    drop(conns);
}
