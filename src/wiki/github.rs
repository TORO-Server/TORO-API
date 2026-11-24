//! GitHub API (Wiki 原稿のリポジトリ)

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::StatusCode;
use reqwest::header;
use serde_json::{Map, Value, json};
use tokio::sync::OnceCell;

use super::path;
use crate::App;
use crate::identity::Session;
use crate::util::encode_uri_component;

/// コミットメッセージの 1 行目の長さの上限
const MAX_SUMMARY_CHARS: usize = 200;
const WIKI_CACHE_TTL: Duration = Duration::from_secs(15);
const WIKI_NOT_FOUND_TTL: Duration = Duration::from_secs(5);
const MAX_CACHED_WIKI_FILES: usize = 512;

fn request(app: &App, method: reqwest::Method, url: String) -> reqwest::RequestBuilder {
    app.http
        .request(method, url)
        .bearer_auth(&app.cfg.github_token)
        .header("Accept", "application/vnd.github.v3+json")
}

fn repo_url(app: &App) -> String {
    format!(
        "{}/repos/{}/{}",
        app.cfg.github_api, app.cfg.github_owner, app.cfg.github_repo
    )
}

fn encode_segments(path: &str) -> String {
    path.split('/')
        .map(encode_uri_component)
        .collect::<Vec<_>>()
        .join("/")
}

#[derive(Clone)]
pub struct WikiFile {
    pub path: String,
    pub sha: String,
    pub content: String,
    pub size: u64,
}

#[derive(Clone)]
struct CachedWikiFile {
    at: Instant,
    ttl: Duration,
    value: Option<WikiFile>,
    etag: Option<String>,
}

#[derive(Clone)]
struct CachedDocBlobs {
    at: Instant,
    value: Vec<DocBlob>,
    etag: Option<String>,
}

type FileInflightMap =
    HashMap<String, Arc<OnceCell<Result<Option<WikiFile>, String>>>>;

struct FileInflightGuard<'a> {
    map: &'a std::sync::Mutex<FileInflightMap>,
    key: String,
    cell: Arc<OnceCell<Result<Option<WikiFile>, String>>>,
}

impl Drop for FileInflightGuard<'_> {
    fn drop(&mut self) {
        let _ = self.cell.set(Err("aborted".to_string()));
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.key);
    }
}

pub struct WikiCache {
    files: std::sync::Mutex<HashMap<String, CachedWikiFile>>,
    file_inflight: std::sync::Mutex<FileInflightMap>,
    blobs: std::sync::Mutex<Option<CachedDocBlobs>>,
    blobs_inflight: tokio::sync::Mutex<()>,
}

impl WikiCache {
    pub fn new() -> Self {
        Self {
            files: std::sync::Mutex::new(HashMap::new()),
            file_inflight: std::sync::Mutex::new(HashMap::new()),
            blobs: std::sync::Mutex::new(None),
            blobs_inflight: tokio::sync::Mutex::new(()),
        }
    }

    pub fn invalidate(&self, clean_path: &str) {
        self.files
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(clean_path);
        *self.blobs.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// `path` は wiki::path::normalize を通したもの
pub async fn get_file(app: &App, clean_path: &str) -> Result<Option<WikiFile>, String> {
    let max_ttl = Duration::from_secs(app.cfg.wiki_index_ttl_secs).min(WIKI_CACHE_TTL);
    let stale_entry = {
        let guard = app
            .wiki_cache
            .files
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = guard.get(clean_path) {
            if !max_ttl.is_zero() && entry.at.elapsed() < entry.ttl.min(max_ttl) {
                return Ok(entry.value.clone());
            }
            Some(entry.clone())
        } else {
            None
        }
    };

    let (cell, leader) = {
        let mut inflight = app
            .wiki_cache
            .file_inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = inflight.get(clean_path) {
            (existing.clone(), false)
        } else {
            let created = Arc::new(OnceCell::new());
            inflight.insert(clean_path.to_string(), created.clone());
            (created, true)
        }
    };
    if !leader {
        return cell
            .get_or_init(|| async { Err("aborted".to_string()) })
            .await
            .clone();
    }
    let _guard = FileInflightGuard {
        map: &app.wiki_cache.file_inflight,
        key: clean_path.to_string(),
        cell: cell.clone(),
    };

    let res = fetch_file_uncached(app, clean_path, stale_entry.as_ref()).await;
    let _ = cell.set(res.clone());
    drop(_guard);
    res
}

async fn fetch_file_uncached(
    app: &App,
    clean_path: &str,
    stale_entry: Option<&CachedWikiFile>,
) -> Result<Option<WikiFile>, String> {
    let url = format!(
        "{}/contents/{}?ref={}",
        repo_url(app),
        encode_segments(clean_path),
        encode_uri_component(&app.cfg.github_branch)
    );
    let mut req = request(app, reqwest::Method::GET, url);
    if let Some(stale) = stale_entry
        && let Some(tag) = stale.etag.as_deref()
    {
        req = req.header(header::IF_NONE_MATCH, tag);
    }
    let res = req.send().await.map_err(|e| e.to_string())?;
    let status = res.status();
    if status == StatusCode::NOT_MODIFIED
        && let Some(stale) = stale_entry
    {
        let mut updated = stale.clone();
        updated.at = Instant::now();
        app.wiki_cache
            .files
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(clean_path.to_string(), updated.clone());
        return Ok(updated.value);
    }
    let etag = res
        .headers()
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if status == StatusCode::NOT_FOUND {
        store_cached_wiki_file(
            app,
            clean_path,
            CachedWikiFile {
                at: Instant::now(),
                ttl: WIKI_NOT_FOUND_TTL,
                value: None,
                etag,
            },
        );
        return Ok(None);
    }
    if !status.is_success() {
        let text = res.text().await.unwrap_or_default();
        return Err(format!("GitHub API error ({}): {text}", status.as_u16()));
    }
    let data: Value = res.json().await.map_err(|e| e.to_string())?;
    let encoded: String = data["content"]
        .as_str()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let bytes = STANDARD.decode(encoded).map_err(|e| e.to_string())?;
    let file = WikiFile {
        path: data["path"].as_str().unwrap_or(clean_path).to_string(),
        sha: data["sha"].as_str().unwrap_or("").to_string(),
        content: String::from_utf8_lossy(&bytes).into_owned(),
        size: data["size"].as_u64().unwrap_or(0),
    };
    store_cached_wiki_file(
        app,
        clean_path,
        CachedWikiFile {
            at: Instant::now(),
            ttl: WIKI_CACHE_TTL,
            value: Some(file.clone()),
            etag,
        },
    );
    Ok(Some(file))
}

fn store_cached_wiki_file(app: &App, clean_path: &str, entry: CachedWikiFile) {
    let mut guard = app
        .wiki_cache
        .files
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if guard.len() >= MAX_CACHED_WIKI_FILES && !guard.contains_key(clean_path) {
        guard.retain(|_, v| v.at.elapsed() < v.ttl);
        if guard.len() >= MAX_CACHED_WIKI_FILES
            && let Some(oldest) = guard.keys().next().cloned()
        {
            guard.remove(&oldest);
        }
    }
    guard.insert(clean_path.to_string(), entry);
}

pub struct Commit<'a> {
    pub path: &'a str,
    pub content: &'a str,
    pub sha: Option<&'a str>,
    pub message: Option<&'a str>,
    pub user: &'a Session,
}

/// コミットメッセージの 1 行目だけを使う。だれの編集かは下の trailer だけが示す (別の人の名前を足させない)
fn summary_line(message: &str) -> String {
    message
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_SUMMARY_CHARS)
        .collect()
}

/// 原稿を保存してコミットする。戻り値は (コミットの SHA, ファイルの SHA)
pub async fn commit_file(app: &App, commit: &Commit<'_>) -> Result<(String, String), String> {
    let clean_path =
        path::normalize_for_save(commit.path).ok_or_else(|| "Invalid path".to_string())?;

    let current_sha = match commit.sha.filter(|s| !s.is_empty()) {
        Some(sha) => Some(sha.to_string()),
        None => fetch_file_uncached(app, &clean_path, None)
            .await?
            .map(|file| file.sha),
    };

    let user = commit.user;
    let display_name = user.display_name();
    let email = format!("{}+discord@users.noreply.torosaba.net", user.id());
    let summary = commit.message.map(summary_line).unwrap_or_default();
    let message = match summary.as_str() {
        "" => format!(
            "Wiki update: {clean_path}\n\nEdited via Browser/API by Discord user {display_name} ({})",
            user.id()
        ),
        summary => format!("{summary}\n\nCo-authored-by: {display_name} <{email}>"),
    };
    let author = json!({ "name": format!("{display_name} (via Discord)"), "email": email });

    let mut payload = Map::new();
    payload.insert("message".into(), json!(message));
    payload.insert("content".into(), json!(STANDARD.encode(commit.content)));
    payload.insert("branch".into(), json!(app.cfg.github_branch));
    payload.insert("committer".into(), author.clone());
    payload.insert("author".into(), author);
    if let Some(sha) = current_sha {
        payload.insert("sha".into(), json!(sha));
    }

    let url = format!(
        "{}/contents/{}",
        repo_url(app),
        encode_segments(&clean_path)
    );
    let res = request(app, reqwest::Method::PUT, url)
        .json(&Value::Object(payload))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = res.status();
    if !status.is_success() {
        let text = res.text().await.unwrap_or_default();
        return Err(format!("GitHub Commit error ({}): {text}", status.as_u16()));
    }
    let data: Value = res.json().await.map_err(|e| e.to_string())?;
    app.wiki_cache.invalidate(&clean_path);
    Ok((
        data["commit"]["sha"].as_str().unwrap_or("").to_string(),
        data["content"]["sha"].as_str().unwrap_or("").to_string(),
    ))
}

#[derive(Clone)]
pub struct DocBlob {
    /// docs/ から始まるパス
    pub path: String,
    pub sha: String,
}

/// docs/ の下の原稿のパスと SHA。取得に失敗したら None
pub async fn doc_blobs(app: &App) -> Option<Vec<DocBlob>> {
    let ttl = Duration::from_secs(app.cfg.wiki_index_ttl_secs).min(WIKI_CACHE_TTL);
    {
        let guard = app
            .wiki_cache
            .blobs
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = guard.as_ref()
            && !ttl.is_zero()
            && cached.at.elapsed() < ttl
        {
            return Some(cached.value.clone());
        }
    }
    let _inflight = app.wiki_cache.blobs_inflight.lock().await;
    let stale = {
        let guard = app
            .wiki_cache
            .blobs
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = guard.as_ref()
            && !ttl.is_zero()
            && cached.at.elapsed() < ttl
        {
            return Some(cached.value.clone());
        }
        guard.clone()
    };

    let url = format!(
        "{}/git/trees/{}?recursive=1",
        repo_url(app),
        encode_uri_component(&app.cfg.github_branch)
    );
    let mut req = request(app, reqwest::Method::GET, url);
    if let Some(ref entry) = stale
        && let Some(tag) = entry.etag.as_deref()
    {
        req = req.header(header::IF_NONE_MATCH, tag);
    }
    let res = req.send().await.ok()?;
    if res.status() == StatusCode::NOT_MODIFIED
        && let Some(mut entry) = stale
    {
        entry.at = Instant::now();
        let val = entry.value.clone();
        *app.wiki_cache
            .blobs
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(entry);
        return Some(val);
    }
    if !res.status().is_success() {
        return None;
    }
    let etag = res
        .headers()
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let data: Value = res.json().await.ok()?;
    let blobs: Vec<DocBlob> = data["tree"]
        .as_array()?
        .iter()
        .filter(|item| item["type"] == "blob")
        .filter_map(|item| {
            let path = item["path"].as_str()?;
            let is_doc = path.starts_with("docs/") && path.ends_with(".md");
            is_doc.then(|| DocBlob {
                path: path.to_string(),
                sha: item["sha"].as_str().unwrap_or("").to_string(),
            })
        })
        .collect();
    *app.wiki_cache
        .blobs
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(CachedDocBlobs {
        at: Instant::now(),
        value: blobs.clone(),
        etag,
    });
    Some(blobs)
}

/// docs/ の下の原稿の一覧 (docs/ を除いたパス)。取得に失敗したら空
pub async fn list_pages(app: &App, category: Option<&str>) -> Vec<String> {
    doc_blobs(app)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|blob| {
            // 先頭の docs/ を 1 つだけ外す (docs/docs/a.md を別のパスとして見せない)
            blob.path
                .strip_prefix("docs/")
                .unwrap_or(&blob.path)
                .to_string()
        })
        .filter(|path| category.is_none_or(|c| path.starts_with(c)))
        .collect()
}

/// 原稿の本文を SHA で読む
pub async fn blob_text(app: &App, sha: &str) -> Option<String> {
    let url = format!("{}/git/blobs/{sha}", repo_url(app));
    let res = request(app, reqwest::Method::GET, url).send().await.ok()?;
    if !res.status().is_success() {
        return None;
    }
    let data: Value = res.json().await.ok()?;
    decode_content(&data["content"]).ok()
}

fn decode_content(content: &Value) -> Result<String, String> {
    let encoded: String = content
        .as_str()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let bytes = STANDARD.decode(encoded).map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_messages_are_one_short_line() {
        assert_eq!(summary_line("直した"), "直した");
        assert_eq!(summary_line("1 行目\n\n2 行目\nTrailer: x"), "1 行目");
        assert_eq!(summary_line("1 行目\r\n2 行目"), "1 行目");
        assert_eq!(summary_line("a\tb\u{7f}c"), "abc");
        assert_eq!(summary_line("\n本文"), "");
        assert_eq!(
            summary_line(&"あ".repeat(MAX_SUMMARY_CHARS + 1)),
            "あ".repeat(MAX_SUMMARY_CHARS)
        );
    }
}
