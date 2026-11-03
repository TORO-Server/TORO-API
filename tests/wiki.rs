//! Wiki の REST API (/docs/raw, /docs/list, /docs/save)

mod common;

use axum::http::header;
use common::*;
use serde_json::{Value, json};
use toro_api::util::encode_uri_component;

const JP_PATH: &str = "07_System/日本語のページ.md";

fn encoded_jp_path() -> String {
    let encoded: Vec<String> = format!("docs/{JP_PATH}")
        .split('/')
        .map(encode_uri_component)
        .collect();
    encoded.join("/")
}

#[tokio::test]
async fn raw_returns_decoded_content() {
    let server = start().await;
    let long_body = "本文 ".repeat(100);
    server.put_file(&encoded_jp_path(), "abc", &long_body);

    let res = server
        .get(&format!("/docs/raw?path={}", encode_uri_component(JP_PATH)))
        .await;
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["sha"], "abc");
    assert_eq!(body["content"], long_body.as_str());
    assert_eq!(body["size"], long_body.len());

    let res = server
        .get(&format!(
            "/docs/raw?format=raw&path={}",
            encode_uri_component(JP_PATH)
        ))
        .await;
    assert_eq!(res.headers()[header::ETAG], "\"abc\"");
    assert_eq!(
        res.headers()[header::CONTENT_TYPE],
        "text/markdown; charset=utf-8"
    );
    assert_eq!(res.text().await.unwrap(), long_body);
}

#[tokio::test]
async fn raw_errors() {
    let server = start().await;
    assert_eq!(server.get("/docs/raw").await.status(), 400);
    assert_eq!(server.get("/docs/raw?path=missing").await.status(), 404);
}

#[tokio::test]
async fn list_returns_the_pages_under_docs() {
    let server = start().await;
    let body: Value = server.get("/docs/list").await.json().await.unwrap();
    assert_eq!(body["count"], 2);
    assert_eq!(body["pages"], json!(["01_About/a.md", "02_Guide/b.md"]));

    let body: Value = server
        .get("/docs/list?category=02_Guide")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["count"], 1);
    assert_eq!(body["pages"], json!(["02_Guide/b.md"]));

    let body: Value = server
        .get("/docs/list?category=none")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["count"], 0);
    assert_eq!(body["pages"], json!([]));
}

#[tokio::test]
async fn paths_outside_docs_never_reach_github() {
    let server = start().await;
    for bad in ["../secret", "a/../../secret", "a\\b", "a//b"] {
        let encoded = encode_uri_component(bad);
        assert_eq!(
            server
                .get(&format!("/docs/raw?path={encoded}"))
                .await
                .status(),
            400,
            "{bad}"
        );

        let res = server
            .save(Some(MEMBER_TOKEN), &json!({ "path": bad, "content": "x" }))
            .await;
        assert_eq!(res.status(), 400, "{bad}");
    }
    assert!(server.github.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn save_requires_login_and_permission() {
    let server = start().await;
    let body = json!({ "path": "a", "content": "x" });

    assert_eq!(server.save(None, &body).await.status(), 401);
    assert_eq!(server.save(Some(READONLY_TOKEN), &body).await.status(), 403);
    // OAuth の scope が read だけなら、編集できる利用者でも保存できない
    assert_eq!(
        server.save(Some(READ_SCOPE_TOKEN), &body).await.status(),
        403
    );
    assert!(server.github.commits.lock().unwrap().is_empty());
}

#[tokio::test]
async fn save_rejects_bad_bodies() {
    let server = start().await;
    let res = server
        .client
        .post(server.url("/docs/save"))
        .bearer_auth(MEMBER_TOKEN)
        .body("{broken")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    assert_eq!(
        server
            .save(Some(MEMBER_TOKEN), &json!({ "content": "x" }))
            .await
            .status(),
        400
    );
    assert_eq!(
        server
            .save(Some(MEMBER_TOKEN), &json!({ "path": "a" }))
            .await
            .status(),
        400
    );
    assert_eq!(
        server
            .save(Some(MEMBER_TOKEN), &json!({ "path": "/a", "content": "x" }))
            .await
            .status(),
        400
    );
}

#[tokio::test]
async fn save_commits_as_the_discord_user() {
    let server = start().await;
    server.put_file("docs/a.md", "old-sha", "old");

    let res = server
        .save(
            Some(MEMBER_TOKEN),
            &json!({ "path": "a", "content": "新しい本文", "sha": "old-sha", "message": "直した" }),
        )
        .await;
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["commitSha"], "commit-1");

    let commits = server.github.commits.lock().unwrap();
    let commit = &commits[0];
    assert_eq!(commit["branch"], "main");
    assert_eq!(commit["sha"], "old-sha");
    assert_eq!(commit["author"]["name"], "member_user (via Discord)");
    assert_eq!(
        commit["committer"]["email"],
        "1001+discord@users.noreply.torosaba.net"
    );
    assert!(
        commit["message"]
            .as_str()
            .unwrap()
            .starts_with("直した\n\nCo-authored-by: member_user <1001+discord@")
    );
    assert_eq!(
        server.github.files.lock().unwrap()["docs/a.md"].1,
        "新しい本文"
    );
}

#[tokio::test]
async fn save_without_sha_uses_the_latest_and_new_files_have_none() {
    let server = start().await;
    server.put_file("docs/a.md", "latest", "old");

    for (path, expected_sha) in [("a", json!("latest")), ("new-page", Value::Null)] {
        let res = server
            .save(Some(MEMBER_TOKEN), &json!({ "path": path, "content": "x" }))
            .await;
        assert_eq!(res.status(), 200);
        assert_eq!(
            server.github.commits.lock().unwrap().last().unwrap()["sha"],
            expected_sha
        );
    }
    let commits = server.github.commits.lock().unwrap();
    assert!(
        commits[0]["message"]
            .as_str()
            .unwrap()
            .starts_with("Wiki update: docs/a.md\n\nEdited via Browser/API")
    );
}

#[tokio::test]
async fn conflicts_keep_the_text_the_editor_looks_for() {
    let server = start().await;
    server.put_file("docs/a.md", "current", "old");

    let res = server
        .save(
            Some(MEMBER_TOKEN),
            &json!({ "path": "a", "content": "x", "sha": "stale" }),
        )
        .await;
    assert_eq!(res.status(), 500);
    let error = res.json::<Value>().await.unwrap()["error"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        error.starts_with("Failed to commit to GitHub: GitHub Commit error (409)"),
        "{error}"
    );
    assert!(error.contains("does not match"));
}
