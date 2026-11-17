//! (実装中)
use std::sync::Arc;
use axum::response::Response;
use crate::http::Ctx;
use super::Map3d;
pub async fn handle(_svc: &Arc<Map3d>, _ctx: &Ctx, _path: &str) -> Response { super::error(404, "Not Found") }
