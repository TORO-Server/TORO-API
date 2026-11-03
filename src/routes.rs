//! URL から各機能への振り分け。どのホストで受けても同じ (このサービスの役割は api だけ)

use std::sync::Arc;

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::Method;
use axum::http::header::{self, HeaderValue};
use axum::response::Response;
use serde_json::json;

use crate::http::{self, Ctx};
use crate::{App, api, openapi, wiki};

const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

pub fn router(app: Arc<App>) -> Router {
    Router::new().fallback(dispatch).with_state(app)
}

async fn dispatch(State(app): State<Arc<App>>, request: Request) -> Response {
    // 同時に処理する数を限り、ボディの分も含めてメモリの使用量に上限を設ける
    let Ok(_permit) = app.requests.try_acquire() else {
        return http::busy();
    };

    let (parts, body) = request.into_parts();
    let body = match to_bytes(body, MAX_BODY_BYTES).await {
        Ok(body) => body,
        Err(_) => {
            let ctx = Ctx::new(&parts, Default::default(), &app.cfg);
            return http::json(&ctx, &json!({ "error": "Payload Too Large" }), 413);
        }
    };
    let ctx = Ctx::new(&parts, body, &app.cfg);

    let mut res = route(&app, &ctx).await;
    let headers = res.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // 利用者ごとに違う応答を Cloudflare にためさせない。ためてよいものは、自分で Cache-Control を付ける
    headers
        .entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    res
}

async fn route(app: &App, ctx: &Ctx) -> Response {
    if ctx.method == Method::OPTIONS {
        return http::no_content(http::cors_headers(ctx));
    }

    match ctx.path.as_str() {
        "/healthz" => http::json(ctx, &json!({ "ok": true }), 200),
        "/" => api::index(app, ctx),
        "/me" => api::me(app, ctx).await,
        "/docs/raw" => wiki::api::raw(app, ctx).await,
        "/docs/list" => wiki::api::list(app, ctx).await,
        "/docs/search" => wiki::api::search(app, ctx).await,
        "/docs/save" => wiki::api::save(app, ctx).await,
        "/server/status" => api::server_status(app, ctx).await,
        "/openapi.json" => openapi::handle(app, ctx),
        _ => http::json(ctx, &json!({ "error": "Not Found", "path": ctx.path }), 404),
    }
}
