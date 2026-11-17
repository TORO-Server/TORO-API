//! `resolve` / `proxy` / `<地図>.json` の試験。外へはつながず、手元の偽のサーバーを使う
//! (BlueMap の web ルートの代わりと、看板の URL の先の代わり。名前は `test_hosts` で手元のアドレスへ向ける)。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::header::{HeaderMap, HeaderName, HeaderValue};
use axum::http::{Method, StatusCode};
use axum::response::Response;
use futures_util::future::BoxFuture;

use super::*;
use crate::map::store::StoreResult;
use crate::map3d::store::BlueData;

const PNG: &[u8] =
    b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0\x1f\x15\xc4\x89";
const PAGE: &str = "https://img.example.com/a.png";
const OK_ORIGIN: &str = "https://3d-map.torosaba.net";

struct NoStore;

impl BluemapStore for NoStore {
    fn tile<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: i64,
        _: i64,
    ) -> BoxFuture<'a, StoreResult<Option<BlueData>>> {
        Box::pin(async { Ok(None) })
    }
    fn item<'a>(&'a self, _: &'a str, _: &'a str) -> BoxFuture<'a, StoreResult<Option<BlueData>>> {
        Box::pin(async { Ok(None) })
    }
    fn map_exists<'a>(&'a self, _: &'a str) -> BoxFuture<'a, StoreResult<bool>> {
        Box::pin(async { Ok(true) })
    }
    fn map_ids<'a>(&'a self) -> BoxFuture<'a, StoreResult<Vec<String>>> {
        Box::pin(async { Ok(vec![]) })
    }
}

fn plain(status: u16, ty: &str, body: Vec<u8>) -> Response {
    let mut res = Response::new(Body::from(body));
    *res.status_mut() = StatusCode::from_u16(status).unwrap();
    res.headers_mut()
        .insert("content-type", HeaderValue::from_str(ty).unwrap());
    res
}

/// 看板の URL の先 (`img.example.com`) と、BlueMap の web ルートを 1 つのサーバーで兼ねる
async fn fake(request: Request) -> Response {
    let path = request.uri().path().to_string();
    match path.as_str() {
        "/settings.json" => plain(
            200,
            "application/json",
            br#"{"maps":["overworld"]}"#.to_vec(),
        ),
        "/marumasign/overworld.json" => {
            let data = serde_json::json!({
                "format": 1,
                "signs": [
                    [0, 0, 0, 0, 0, PAGE, "https://img.example.com/page.html"],
                    [0, 0, 0, 0, 0, "https://img.example.com/redir", "https://img.example.com/loop"],
                    [0, 0, 0, 0, 0, "https://img.example.com/big", "https://img.example.com/priv"],
                ]
            });
            plain(200, "application/json", data.to_string().into_bytes())
        }
        "/a.png" => {
            let mut body = PNG.to_vec();
            body.extend(std::iter::repeat_n(7u8, 100));
            plain(200, "image/png", body)
        }
        "/page.html" => plain(
            200,
            "text/html",
            b"<!DOCTYPE html><html><script>x</script>".to_vec(),
        ),
        "/redir" => {
            let mut res = plain(302, "text/plain", vec![]);
            res.headers_mut().insert(
                "location",
                HeaderValue::from_static("https://img.example.com/a.png"),
            );
            res
        }
        "/loop" => {
            let mut res = plain(302, "text/plain", vec![]);
            res.headers_mut().insert(
                "location",
                HeaderValue::from_static("https://img.example.com/loop"),
            );
            res
        }
        "/priv" => {
            let mut res = plain(302, "text/plain", vec![]);
            res.headers_mut().insert(
                "location",
                HeaderValue::from_static("https://10.0.0.5/secret.png"),
            );
            res
        }
        "/big" => {
            // 画像の上限 (12 MiB) を超えるもの
            let mut body = PNG.to_vec();
            body.resize(13 * 1024 * 1024, 0);
            plain(200, "image/png", body)
        }
        _ => plain(404, "text/plain", b"no".to_vec()),
    }
}

async fn setup() -> (Arc<Sign>, SocketAddr) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = axum::Router::new().fallback(fake);
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let cfg = SignConfig {
        test_hosts: HashMap::from([("img.example.com".to_string(), addr)]),
        ..Default::default()
    };
    let sign = Sign::new(
        &cfg,
        reqwest::Client::new(),
        Arc::new(NoStore),
        format!("http://{addr}"),
    );
    (sign, addr)
}

fn ctx(path: &str, query: &[(&str, &str)], headers: &[(&str, &str)]) -> Ctx {
    let mut map = HeaderMap::new();
    for (k, v) in headers {
        map.insert(
            HeaderName::from_bytes(k.as_bytes()).unwrap(),
            HeaderValue::from_str(v).unwrap(),
        );
    }
    Ctx {
        method: Method::GET,
        path: path.to_string(),
        query: query
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        headers: map,
        origin_trusted: false,
        body: Default::default(),
    }
}

/// 画面 (別オリジン、同じサイト) からの要求
fn page(extra: &[(&str, &str)]) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        ("sec-fetch-site", "same-site".to_string()),
        ("origin", OK_ORIGIN.to_string()),
    ];
    for (k, v) in extra {
        headers.push((Box::leak(k.to_string().into_boxed_str()), v.to_string()));
    }
    headers
}

async fn call(
    sign: &Arc<Sign>,
    rest: &str,
    query: &[(&str, &str)],
    headers: Vec<(&'static str, String)>,
) -> Response {
    let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let c = ctx(&format!("/map3d/v1/marumasign/{rest}"), query, &pairs);
    handle(sign, &c, rest).await
}

async fn text(res: Response) -> (u16, HeaderMap, Vec<u8>) {
    let status = res.status().as_u16();
    let headers = res.headers().clone();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, body.to_vec())
}

#[tokio::test]
async fn proxy_serves_a_sign_image_to_the_other_origin() {
    let (sign, _) = setup().await;
    let (status, h, body) = text(call(&sign, "proxy", &[("u", PAGE)], page(&[])).await).await;
    assert_eq!(status, 200);
    assert!(body.starts_with(PNG));
    assert_eq!(h["content-type"], "image/png");
    assert_eq!(h["x-marumasign-kind"], "image");
    assert_eq!(h["x-marumasign-type"], "image/png");
    assert_eq!(h["x-marumasign-url"], PAGE);
    assert_eq!(h["accept-ranges"], "bytes");
    assert_eq!(h["content-disposition"], "attachment");
    assert_eq!(h["cross-origin-resource-policy"], "same-site");
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert_eq!(h["cache-control"], "private, max-age=300");
}

#[tokio::test]
async fn proxy_supports_ranges() {
    let (sign, _) = setup().await;
    // 1 回目は全体 (キャッシュに入る)。2 回目からは範囲を切り出す
    let (status, _, all) = text(call(&sign, "proxy", &[("u", PAGE)], page(&[])).await).await;
    assert_eq!(status, 200);
    let (status, h, body) = text(
        call(
            &sign,
            "proxy",
            &[("u", PAGE)],
            page(&[("range", "bytes=0-99")]),
        )
        .await,
    )
    .await;
    assert_eq!(status, 206);
    assert_eq!(body.len(), 100);
    assert_eq!(h["content-range"], format!("bytes 0-99/{}", all.len()));
    let (status, _, _) = text(
        call(
            &sign,
            "proxy",
            &[("u", PAGE)],
            page(&[("range", "bytes=5-2")]),
        )
        .await,
    )
    .await;
    assert_eq!(status, 416);
    let (status, h, _) = text(
        call(
            &sign,
            "proxy",
            &[("u", PAGE)],
            page(&[("range", "bytes=9999-")]),
        )
        .await,
    )
    .await;
    assert_eq!(status, 416);
    assert_eq!(h["content-range"], format!("bytes */{}", all.len()));
}

#[tokio::test]
async fn proxy_only_serves_from_the_map_pages() {
    let (sign, _) = setup().await;
    let cases: Vec<(&str, Vec<(&'static str, String)>)> = vec![
        ("no headers", vec![]),
        (
            "cross-site",
            vec![
                ("sec-fetch-site", "cross-site".into()),
                ("origin", OK_ORIGIN.into()),
            ],
        ),
        (
            "same-site, other origin",
            vec![
                ("sec-fetch-site", "same-site".into()),
                ("origin", "https://evil.example".into()),
            ],
        ),
        (
            "same-site, no origin",
            vec![("sec-fetch-site", "same-site".into())],
        ),
        (
            "look-alike origin",
            vec![
                ("sec-fetch-site", "same-site".into()),
                ("origin", "https://torosaba.net.evil.example".into()),
            ],
        ),
        (
            "other origin only",
            vec![("origin", "https://evil.example".into())],
        ),
    ];
    for (name, headers) in cases {
        let (status, _, _) = text(call(&sign, "proxy", &[("u", PAGE)], headers).await).await;
        assert_eq!(status, 403, "{name}");
    }
    // same-origin / none は、Origin が無くても通る (直接開いたとき)
    for site in ["same-origin", "none"] {
        let (status, _, _) = text(
            call(
                &sign,
                "proxy",
                &[("u", PAGE)],
                vec![("sec-fetch-site", site.into())],
            )
            .await,
        )
        .await;
        assert_eq!(status, 200, "{site}");
    }
}

#[tokio::test]
async fn proxy_rejects_bad_requests() {
    let (sign, _) = setup().await;
    let (status, _, _) = text(call(&sign, "proxy", &[], page(&[])).await).await;
    assert_eq!(status, 400);
    let (status, _, _) = text(
        call(
            &sign,
            "proxy",
            &[("u", "http://img.example.com/a.png")],
            page(&[]),
        )
        .await,
    )
    .await;
    assert_eq!(status, 400);
    let four = [
        ("u", PAGE),
        ("p", PAGE),
        ("p", PAGE),
        ("p", PAGE),
        ("p", PAGE),
    ];
    let (status, _, _) = text(call(&sign, "proxy", &four, page(&[])).await).await;
    assert_eq!(status, 400);
    // 看板にない URL
    let (status, _, _) = text(
        call(
            &sign,
            "proxy",
            &[("u", "https://img.example.com/other.png")],
            page(&[]),
        )
        .await,
    )
    .await;
    assert_eq!(status, 403);
}

#[tokio::test]
async fn proxy_refuses_html_and_oversized_files() {
    let (sign, _) = setup().await;
    let (status, _, _) = text(
        call(
            &sign,
            "proxy",
            &[("u", "https://img.example.com/page.html")],
            page(&[]),
        )
        .await,
    )
    .await;
    assert_eq!(status, 415);
    let (status, _, _) = text(
        call(
            &sign,
            "proxy",
            &[("u", "https://img.example.com/big")],
            page(&[]),
        )
        .await,
    )
    .await;
    assert_eq!(status, 413);
}

#[tokio::test]
async fn proxy_limits_requests_per_client() {
    let (sign, _) = setup().await;
    // 別の IP は影響を受けない
    sign.request_limit.hit("203.0.113.7", PROXY_REQUESTS_PER_IP);
    let headers = page(&[("x-toro-client-ip", "203.0.113.7")]);
    let (status, _, _) = text(call(&sign, "proxy", &[("u", PAGE)], headers).await).await;
    assert_eq!(status, 429);
    let headers = page(&[("x-toro-client-ip", "203.0.113.8")]);
    let (status, _, _) = text(call(&sign, "proxy", &[("u", PAGE)], headers).await).await;
    assert_eq!(status, 200);
    // 量の上限も効く
    sign.byte_limit.hit("203.0.113.9", BYTES_PER_IP + 1);
    let headers = page(&[("x-toro-client-ip", "203.0.113.9")]);
    let (status, _, _) = text(call(&sign, "proxy", &[("u", PAGE)], headers).await).await;
    assert_eq!(status, 429);
}

#[tokio::test]
async fn proxy_uses_its_own_concurrency_gate() {
    let (sign, _) = setup().await;
    let permits: Vec<_> = (0..GATE_PER_IP)
        .map(|_| sign.gate.enter("203.0.113.5").unwrap())
        .collect();
    // その IP は満杯。ただし、キャッシュにあるものは枠を使わずに返せる (まだ無いので 503)
    let headers = page(&[("x-toro-client-ip", "203.0.113.5")]);
    let (status, _, _) = text(call(&sign, "proxy", &[("u", PAGE)], headers).await).await;
    assert_eq!(status, 503);
    drop(permits);
    let headers = page(&[("x-toro-client-ip", "203.0.113.5")]);
    let (status, _, _) = text(call(&sign, "proxy", &[("u", PAGE)], headers).await).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn resolve_follows_redirects_and_checks_each_hop() {
    let (sign, _) = setup().await;
    let (status, h, body) = text(
        call(
            &sign,
            "resolve",
            &[("u", "https://img.example.com/redir")],
            page(&[]),
        )
        .await,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(h["cache-control"], "private, max-age=300");
    assert_eq!(h["cross-origin-resource-policy"], "same-site");
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["url"], PAGE);
    assert_eq!(json["redirected"], true);
    assert_eq!(json["status"], 200);
    assert_eq!(json["type"], "image/png");
    assert_eq!(json["cors"], false);
    // 行き先も読んでよくなる
    assert!(sign.allowed(PAGE).is_some());

    // 転送の行き先が家の中のアドレスなら読まない
    let (status, _, _) = text(
        call(
            &sign,
            "resolve",
            &[("u", "https://img.example.com/priv")],
            page(&[]),
        )
        .await,
    )
    .await;
    assert_eq!(status, 403);
    // 転送のくり返しは 5 回まで
    let (status, _, _) = text(
        call(
            &sign,
            "resolve",
            &[("u", "https://img.example.com/loop")],
            page(&[]),
        )
        .await,
    )
    .await;
    assert_eq!(status, 502);
    // 看板にない URL、ブラウザーでないもの
    let (status, _, _) = text(
        call(
            &sign,
            "resolve",
            &[("u", "https://img.example.com/nothing")],
            page(&[]),
        )
        .await,
    )
    .await;
    assert_eq!(status, 403);
    let (status, _, _) = text(call(&sign, "resolve", &[("u", PAGE)], vec![]).await).await;
    assert_eq!(status, 403);
}

#[tokio::test]
async fn sign_data_has_etag_and_no_cache() {
    let (sign, _) = setup().await;
    let (status, h, body) = text(call(&sign, "overworld.json", &[], page(&[])).await).await;
    assert_eq!(status, 200);
    assert_eq!(h["cache-control"], "no-cache");
    assert!(serde_json::from_slice::<serde_json::Value>(&body).is_ok());
    let etag = h["etag"].to_str().unwrap().to_string();
    let c = ctx(
        "/map3d/v1/marumasign/overworld.json",
        &[],
        &[("if-none-match", &etag)],
    );
    let res = handle(&sign, &c, "overworld.json").await;
    assert_eq!(res.status(), 304);
    // 地図の ID の形が違う、無い地図
    for rest in ["../x.json", "a%2Fb.json", "x.txt"] {
        let (status, _, _) = text(call(&sign, rest, &[], page(&[])).await).await;
        assert_eq!(status, 404, "{rest}");
    }
    let (status, _, _) = text(call(&sign, "nomap.json", &[], page(&[])).await).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn disabled_serves_nothing() {
    let cfg = SignConfig {
        enabled: false,
        ..Default::default()
    };
    let sign = Sign::new(
        &cfg,
        reqwest::Client::new(),
        Arc::new(NoStore),
        "http://127.0.0.1:9".into(),
    );
    let (status, _, _) = text(call(&sign, "proxy", &[("u", PAGE)], page(&[])).await).await;
    assert_eq!(status, 404);
}
