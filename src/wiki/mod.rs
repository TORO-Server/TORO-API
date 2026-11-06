//! GitHub 上の Wiki 原稿の読み書き
//!
//! - `path`: 原稿のパスの検査と正規化
//! - `content`: 原稿の本文の検査
//! - `github`: GitHub API
//! - `api`: REST API (/docs/*)
//! - `search`: 全文検索

pub mod api;
pub mod content;
pub mod github;
pub mod path;
pub mod search;
