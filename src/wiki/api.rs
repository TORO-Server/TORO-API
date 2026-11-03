//! Wiki の原稿の API (/docs/*)。認証は Bearer だけ (Cookie を見ないので CSRF は成り立たない)

use axum::http::header::{self, HeaderMap, HeaderValue};
use axum::response::Response;
use serde_json::{Value, json};

use super::github::{self, Commit};
use super::path;
use crate::App;
use crate::api::unavailable;
use crate::http::{self, Ctx};
use crate::identity::{self, Identity};

/// GET /docs/raw
pub async fn raw(app: &App, ctx: &Ctx) -> Response {
    let Some(requested) = ctx.q("path").filter(|p| !p.is_empty()) else {
        return http::error(ctx, "Missing required query parameter \"path\"", 400);
    };
    let Some(clean_path) = path::normalize(requested) else {
        return http::error(ctx, "Invalid path", 400);
    };

    match github::get_file(app, &clean_path).await {
        Ok(None) => http::error(ctx, &format!("File not found: {requested}"), 404),
        Err(message) => http::error(ctx, &message, 500),
        Ok(Some(file)) if ctx.q("format") == Some("raw") => {
            let mut headers = HeaderMap::new();
            if let Ok(etag) = HeaderValue::from_str(&format!("\"{}\"", file.sha)) {
                headers.insert(header::ETAG, etag);
            }
            http::respond(200, headers, "text/markdown; charset=utf-8", file.content)
        }
        Ok(Some(file)) => http::json(
            ctx,
            &json!({ "path": file.path, "sha": file.sha, "content": file.content, "size": file.size }),
            200,
        ),
    }
}

/// GET /docs/list: docs/ の下の原稿の一覧 (docs/ を除いたパス)。`category` で先頭を絞る
pub async fn list(app: &App, ctx: &Ctx) -> Response {
    let pages = github::list_pages(app, ctx.q("category").filter(|c| !c.is_empty())).await;
    http::json(ctx, &json!({ "count": pages.len(), "pages": pages }), 200)
}

/// GET /docs/search: 語をすべて含む原稿のパスを、よく一致する順に返す
pub async fn search(app: &App, ctx: &Ctx) -> Response {
    let query = ctx.q("q").unwrap_or("");
    match app.wiki_index.search(app, query).await {
        Some(hits) => http::json(ctx, &json!({ "query": query, "hits": hits }), 200),
        None => http::error(ctx, "Wiki search is unavailable", 503),
    }
}

/// POST /docs/save
pub async fn save(app: &App, ctx: &Ctx) -> Response {
    let user = match identity::identify(app, ctx).await {
        Identity::User(user) => user,
        Identity::Anonymous | Identity::Invalid => {
            return http::error(ctx, "Authentication required (Bearer token)", 401);
        }
        Identity::Unavailable => return unavailable(ctx),
    };
    if !user.can_write() {
        return http::error(
            ctx,
            "You do not have permission to edit wiki documents",
            403,
        );
    }
    let Ok(body) = serde_json::from_slice::<Value>(&ctx.body) else {
        return http::error(ctx, "Invalid JSON body", 400);
    };

    let Some(requested) = body["path"].as_str().filter(|p| !p.is_empty()) else {
        return http::error(ctx, "Field \"path\" is required", 400);
    };
    let Some(content) = body["content"].as_str() else {
        return http::error(ctx, "Field \"content\" is required", 400);
    };
    if requested.starts_with('/') || path::normalize(requested).is_none() {
        return http::error(ctx, "Invalid path", 400);
    }

    let commit = Commit {
        path: requested,
        content,
        sha: body["sha"].as_str(),
        message: body["message"].as_str(),
        user: &user,
    };
    match github::commit_file(app, &commit).await {
        Ok((commit_sha, content_sha)) => http::json(
            ctx,
            &json!({
                "ok": true,
                "path": requested,
                "commitSha": commit_sha,
                "contentSha": content_sha,
                "message": "Wiki document updated successfully",
            }),
            200,
        ),
        // 画面は文言の 409 / does not match を見て、競合の案内を出す
        Err(message) => http::error(ctx, &format!("Failed to commit to GitHub: {message}"), 500),
    }
}
