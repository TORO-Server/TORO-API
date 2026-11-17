//! (実装中)
use axum::response::Response;
pub fn upgrade_required() -> Response { super::error(426, "Upgrade Required") }
