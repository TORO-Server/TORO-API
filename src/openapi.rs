//! ChatGPT Actions / Gemini 向けの OpenAPI 仕様

use axum::response::Response;

use crate::App;
use crate::http::{self, Ctx};

/// 仕様の中の `{{SERVER_URL}}` に、設定の URL を入れる
pub fn handle(app: &App, _ctx: &Ctx) -> Response {
    let spec =
        include_str!("../assets/openapi/openapi.json").replace("{{SERVER_URL}}", &app.cfg.api_url);
    http::respond(
        200,
        http::public_headers(),
        http::JSON_UTF8,
        spec.trim_end().to_string(),
    )
}
