//! Wiki の全文検索 (/docs/search)

mod common;

use common::*;
use serde_json::{Value, json};
use toro_api::util::encode_uri_component;

async fn search(server: &TestServer, query: &str) -> Value {
    let res = server
        .get(&format!("/docs/search?q={}", encode_uri_component(query)))
        .await;
    assert_eq!(res.status(), 200, "{query}");
    res.json().await.unwrap()
}

#[tokio::test]
async fn finds_text_in_documents() {
    let server = start().await;

    let body = search(&server, "traincarts").await;
    assert_eq!(body["query"], "traincarts");
    assert_eq!(body["hits"], json!(["01_About/a.md"]));
    assert_eq!(
        search(&server, "電車 TrainCarts").await["hits"],
        json!(["01_About/a.md"])
    );
    assert_eq!(
        search(&server, "02_guide").await["hits"],
        json!(["02_Guide/b.md"])
    );
    assert_eq!(search(&server, "無い語").await["hits"], json!([]));
    // GitHub の検索の修飾子は、ただの語として扱う
    assert_eq!(
        search(&server, "traincarts repo:TORO-Server/Private").await["hits"],
        json!([])
    );
    assert_eq!(search(&server, "   ").await["hits"], json!([]));
    assert_eq!(server.get("/docs/search").await.status(), 200);
}

#[tokio::test]
async fn oversized_queries_are_refused_before_they_cost_anything() {
    let server = start().await;
    let words: Vec<String> = (0..17).map(|i| format!("w{i}")).collect();
    for query in [
        "x".repeat(513),
        "あ".repeat(171),
        words.join(" "),
        " ".repeat(513),
    ] {
        let res = server
            .get(&format!("/docs/search?q={}", encode_uri_component(&query)))
            .await;
        assert_eq!(res.status(), 400, "{query}");
        assert_eq!(
            res.json::<Value>().await.unwrap()["error"],
            "Search query is too long"
        );
    }
    // 原稿を読みにも行かない
    assert!(server.github.requests.lock().unwrap().is_empty());

    // 上限ちょうどと、同じ語の繰り返しは通る
    search(&server, &"x".repeat(512)).await;
    search(&server, &words[..16].join(" ")).await;
    assert_eq!(
        search(&server, &"電車 ".repeat(50)).await["hits"],
        json!(["01_About/a.md"])
    );
}

#[tokio::test]
async fn concurrent_searches_share_one_index() {
    let server = start().await;
    search(&server, "電車").await;
    let searches = (0..32).map(|_| search(&server, "電車 traincarts"));
    for body in futures_util::future::join_all(searches).await {
        assert_eq!(body["hits"], json!(["01_About/a.md"]));
    }
    // 原稿を読み直すのは最初の 1 回だけ
    assert_eq!(server.github.blob_requests(), 2);
}

#[tokio::test]
async fn reads_each_document_once() {
    let server = start().await;
    for _ in 0..3 {
        search(&server, "電車").await;
    }
    assert_eq!(server.github.blob_requests(), 2);
}

#[tokio::test]
async fn picks_up_changes_after_the_interval() {
    let server = start_with(|cfg| cfg.wiki_index_ttl_secs = 0).await;
    search(&server, "x").await;
    assert_eq!(server.github.blob_requests(), 2);

    {
        let mut docs = server.github.docs.lock().unwrap();
        docs.insert(
            "docs/01_About/a.md".to_string(),
            ("sha-a2".to_string(), "新しい本文".to_string()),
        );
        docs.remove("docs/02_Guide/b.md");
    }
    assert_eq!(
        search(&server, "新しい").await["hits"],
        json!(["01_About/a.md"])
    );
    // 変わった 1 件だけを読み直す
    assert_eq!(server.github.blob_requests(), 3);

    assert_eq!(search(&server, "marumasign").await["hits"], json!([]));
    assert_eq!(search(&server, "電車").await["hits"], json!([]));
}

#[tokio::test]
async fn is_unavailable_when_no_document_could_be_read() {
    let server = start().await;
    server.github.docs.lock().unwrap().clear();
    let res = server.get("/docs/search?q=x").await;
    assert_eq!(res.status(), 503);
    assert_eq!(
        res.json::<Value>().await.unwrap()["error"],
        "Wiki search is unavailable"
    );
}
