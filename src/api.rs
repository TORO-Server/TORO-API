//! api.torosaba.net の入り口。Wiki の原稿は `wiki::api`、仕様書は `openapi` にある

use axum::response::Response;
use serde_json::json;

use crate::App;
use crate::http::{self, Ctx};
use crate::identity::{self, Identity};
use crate::minecraft;

/// 各サービスの URL を返す。プログラムはハードコードせず、これを読む
pub fn index(app: &App, ctx: &Ctx) -> Response {
    let api = &app.cfg.api_url;
    http::pretty_json(
        http::cors_headers(ctx),
        &json!({
            "service": "TORO Server API",
            "services": app.cfg.services(),
            "documentation": app.cfg.roles_page(),
            "openapi": format!("{api}/openapi.json"),
        }),
        200,
    )
}

pub fn unavailable(ctx: &Ctx) -> Response {
    http::error(ctx, "Authentication service unavailable", 503)
}

/// トークンの持ち主
pub async fn me(app: &App, ctx: &Ctx) -> Response {
    let body = match identity::identify(app, ctx).await {
        Identity::User(session) => json!({
            "authenticated": true,
            "user": {
                "id": session.id(),
                "username": session.username(),
                "can_edit": session.can_edit(),
            },
        }),
        Identity::Anonymous | Identity::Invalid => {
            json!({ "authenticated": false, "user": null })
        }
        Identity::Unavailable => return unavailable(ctx),
    };
    http::json(ctx, &body, 200)
}

/// Minecraft サーバーの稼働状態。尋ねられなければ、オフラインとして答える
pub async fn server_status(app: &App, ctx: &Ctx) -> Response {
    let cfg = &app.cfg;
    let body = match minecraft::ping(&cfg.mc_status_addr, &cfg.mc_status_host).await {
        Ok(status) => json!({
            "online": true,
            "version": status.version,
            "players": { "online": status.online, "max": status.max, "names": status.players },
            "motd": status.motd,
        }),
        Err(_) => json!({
            "online": false,
            "version": "",
            "players": { "online": 0, "max": 0, "names": [] },
            "motd": [],
        }),
    };
    http::json(ctx, &body, 200)
}
