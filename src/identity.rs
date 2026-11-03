//! 呼び出し元の確認
//!
//! このサービスはトークンの署名を確かめない (鍵を持たない)。Bearer トークンは auth の内部の
//! `/introspect` に渡して、有効かどうかと中身 (claims) を教えてもらう。
//! 90 日トークンの世代の確認も auth が行うので、再作成で失効したトークンはここでも通らない。
//! Cookie は見ない (ブラウザのセッションは auth だけのもの)。

use std::time::Duration;

use axum::http::header;
use serde_json::{Map, Value, json};

use crate::App;
use crate::http::Ctx;

/// auth への問い合わせを待つ長さ
const INTROSPECT_TIMEOUT: Duration = Duration::from_secs(3);

pub struct Session {
    pub claims: Map<String, Value>,
}

impl Session {
    pub fn id(&self) -> &str {
        self.str_claim("id")
    }

    pub fn username(&self) -> &str {
        self.str_claim("username")
    }

    pub fn display_name(&self) -> &str {
        match self.str_claim("global_name") {
            "" => self.username(),
            name => name,
        }
    }

    pub fn can_edit(&self) -> bool {
        self.claims
            .get("can_edit")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// Wiki を変更してよいか。OAuth のトークンは、許可された scope (write か mcp) の中だけ
    pub fn can_write(&self) -> bool {
        let scope_allows = match self.claims.get("scope").and_then(Value::as_str) {
            Some(scope) => scope_allows_write(scope),
            None => true,
        };
        self.can_edit() && scope_allows
    }

    fn str_claim(&self, name: &str) -> &str {
        self.claims.get(name).and_then(Value::as_str).unwrap_or("")
    }
}

/// scope が Wiki の変更を許しているか
pub fn scope_allows_write(scope: &str) -> bool {
    scope.split_whitespace().any(|s| s == "write" || s == "mcp")
}

/// Bearer トークンの確認の結果
pub enum Identity {
    /// トークンなし
    Anonymous,
    User(Session),
    /// 期限切れ、失効、署名の誤りなど。auth が通さなかったもの
    Invalid,
    /// auth に尋ねられなかった。トークンが無効だとは言えない
    Unavailable,
}

pub fn bearer_token(ctx: &Ctx) -> Option<&str> {
    ctx.header(header::AUTHORIZATION)
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

pub async fn identify(app: &App, ctx: &Ctx) -> Identity {
    match bearer_token(ctx) {
        Some(token) => introspect(app, token).await,
        None => Identity::Anonymous,
    }
}

async fn introspect(app: &App, token: &str) -> Identity {
    let url = format!("{}/introspect", app.cfg.auth_internal_url);
    let response = app
        .http
        .post(&url)
        .timeout(INTROSPECT_TIMEOUT)
        .json(&json!({ "token": token }))
        .send()
        .await;
    let body = match response {
        Ok(res) if res.status().is_success() => res.json::<Value>().await.ok(),
        Ok(res) => {
            eprintln!("introspect: auth answered {}", res.status());
            None
        }
        Err(e) => {
            eprintln!("introspect: {e}");
            None
        }
    };
    let Some(body) = body else {
        return Identity::Unavailable;
    };
    match body["active"].as_bool() {
        Some(false) => Identity::Invalid,
        Some(true) => match body["claims"].as_object() {
            Some(claims) if claims.get("id").and_then(Value::as_str).is_some() => {
                Identity::User(Session {
                    claims: claims.clone(),
                })
            }
            Some(_) => Identity::Invalid,
            None => Identity::Unavailable,
        },
        None => Identity::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_needs_edit_permission_and_scope() {
        let session = |claims: Value| Session {
            claims: claims.as_object().unwrap().clone(),
        };
        assert!(session(json!({ "can_edit": true })).can_write());
        assert!(session(json!({ "can_edit": true, "scope": "read write" })).can_write());
        assert!(session(json!({ "can_edit": true, "scope": "mcp" })).can_write());
        assert!(!session(json!({ "can_edit": true, "scope": "read" })).can_write());
        assert!(!session(json!({ "can_edit": false, "scope": "write" })).can_write());
    }
}
