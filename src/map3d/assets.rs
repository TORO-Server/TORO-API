//! `/_assets/{id}.{拡張子}` (assets.torosaba.net)。MarumaSign Companion がホストした看板の画像など。
//! 前段のプロキシが、assets.torosaba.net の `/<id>.<拡張子>` を `/_assets/<id>.<拡張子>` に付け替えて送ってくる。
//!
//! 読めるのは、次のどちらかだけ (ほかのサイトに貼られたり、ただの置き場として使われたりしないように):
//! - いま TORO で遊んでいるプレイヤーの IP から (MarumaSign の Mod が看板を読む)。IP は前段のプロキシが `X-Toro-Client-Ip` で
//!   渡し、遊んでいる人の IP は 入口のプロキシ が `/live/clients` で知らせてくる (`clients`)
//! - 地図のページ (map. / 3d-map.torosaba.net) から。ブラウザが付ける Origin (3D の看板は CORS で読む) か Referer
//!
//! Origin と Referer はブラウザでないものなら自由に書けるので、これはほかのサイトへの貼り付けの抑止で、非公開にする仕組みではない。
//! 看板の画像は地図で誰でも見られる前提のもの。確かめるのは毎回ここなので、Cloudflare にはキャッシュさせない (ブラウザには持たせる)。
//!
//! 表は MarumaSign Companion が作って書く。ここは読むだけ (`msc_hosted_file` と `msc_hosted_chunk`)。

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::http::header::{self, HeaderValue};
use axum::http::{Method, StatusCode};
use axum::response::Response;
use futures_util::future::BoxFuture;
use futures_util::stream;
use sqlx::mysql::{MySqlConnectOptions, MySqlPool, MySqlPoolOptions, MySqlSslMode};
use sqlx::{ConnectOptions, Row};

use super::Map3d;
use super::budget::ASSET_CHUNK_CACHE_BYTES;
use super::clients::ip_bytes;
use crate::config::MapDb;
use crate::http::Ctx;
use crate::map::store::{StoreError, StoreResult};

const FILE_TTL: Duration = Duration::from_secs(60);
const MISSING_TTL: Duration = Duration::from_secs(30);
const MAX_FILES: usize = 10_000;
/// 1 つの IP から読める数の上限 (10 分あたり)。看板の多い場所に入ると一度に数百枚を読むので、多めにしている
const LIMIT_WINDOW: Duration = Duration::from_secs(10 * 60);
const LIMIT_COUNT: u64 = 3000;
const LIMIT_MAX_KEYS: usize = 50_000;
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONNECTIONS: u32 = 8;

/// ホストしているファイル (この行があるものだけ配る。`chunks` と `size` を信じる)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostedFile {
    pub id: String,
    pub ext: String,
    pub content_type: String,
    pub size: u64,
    pub chunk_size: u64,
    pub chunks: u64,
}

pub trait AssetStore: Send + Sync {
    fn file<'a>(&'a self, id: &'a str) -> BoxFuture<'a, StoreResult<Option<HostedFile>>>;
    fn chunk<'a>(&'a self, id: &'a str, seq: u64) -> BoxFuture<'a, StoreResult<Option<Vec<u8>>>>;
}

const SQL_FILE: &str = "SELECT id, ext, content_type, CAST(size AS SIGNED) AS size, CAST(chunk_size AS SIGNED) AS chunk_size, \
     CAST(chunks AS SIGNED) AS chunks FROM msc_hosted_file WHERE id = ?";
const SQL_CHUNK: &str = "SELECT data FROM msc_hosted_chunk WHERE id = ? AND seq = ?";

pub struct SqlxAssetStore {
    pool: MySqlPool,
}

impl SqlxAssetStore {
    /// 接続は最初の問い合わせまで張らない。`db` の接続先を使い、データベースだけを `database` にする
    pub fn new(db: &MapDb, database: &str) -> Self {
        let mut options = MySqlConnectOptions::new()
            .host(&db.host)
            .port(db.port)
            .username(&db.user)
            .database(database)
            .ssl_mode(MySqlSslMode::Disabled)
            .disable_statement_logging();
        if !db.password.is_empty() {
            options = options.password(&db.password);
        }
        Self {
            pool: MySqlPoolOptions::new()
                .max_connections(MAX_CONNECTIONS)
                .acquire_timeout(QUERY_TIMEOUT)
                .connect_lazy_with(options),
        }
    }

    async fn fetch(
        &self,
        query: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments>,
    ) -> StoreResult<Option<sqlx::mysql::MySqlRow>> {
        match tokio::time::timeout(QUERY_TIMEOUT, query.fetch_optional(&self.pool)).await {
            Ok(Ok(row)) => Ok(row),
            Ok(Err(e)) => Err(StoreError(e.to_string())),
            Err(_) => Err(StoreError("query timeout".into())),
        }
    }
}

fn text(row: &sqlx::mysql::MySqlRow, index: usize) -> StoreResult<String> {
    row.try_get_unchecked::<Vec<u8>, _>(index)
        .map(|raw| String::from_utf8_lossy(&raw).into_owned())
        .map_err(|e| StoreError(e.to_string()))
}

fn number(row: &sqlx::mysql::MySqlRow, index: usize) -> StoreResult<u64> {
    row.try_get_unchecked::<i64, _>(index)
        .map(|n| u64::try_from(n).unwrap_or(0))
        .map_err(|e| StoreError(e.to_string()))
}

impl AssetStore for SqlxAssetStore {
    fn file<'a>(&'a self, id: &'a str) -> BoxFuture<'a, StoreResult<Option<HostedFile>>> {
        Box::pin(async move {
            let Some(row) = self.fetch(sqlx::query(SQL_FILE).bind(id)).await? else {
                return Ok(None);
            };
            Ok(Some(HostedFile {
                id: text(&row, 0)?,
                ext: text(&row, 1)?,
                content_type: text(&row, 2)?,
                size: number(&row, 3)?,
                chunk_size: number(&row, 4)?,
                chunks: number(&row, 5)?,
            }))
        })
    }

    fn chunk<'a>(&'a self, id: &'a str, seq: u64) -> BoxFuture<'a, StoreResult<Option<Vec<u8>>>> {
        Box::pin(async move {
            let seq = i64::try_from(seq).unwrap_or(i64::MAX);
            let row = self
                .fetch(sqlx::query(SQL_CHUNK).bind(id).bind(seq))
                .await?;
            row.map(|r| {
                r.try_get_unchecked::<Vec<u8>, _>(0)
                    .map_err(|e| StoreError(e.to_string()))
            })
            .transpose()
        })
    }
}

/// 使ったものを後ろへ回し、古いものから消す入れ物 (合計の大きさで上限を決める)
struct ChunkCache {
    entries: HashMap<String, (Arc<Vec<u8>>, u64)>,
    order: BTreeMap<u64, String>,
    bytes: usize,
    tick: u64,
    cap: usize,
}

impl ChunkCache {
    fn new(cap: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: BTreeMap::new(),
            bytes: 0,
            tick: 0,
            cap,
        }
    }

    fn get(&mut self, key: &str) -> Option<Arc<Vec<u8>>> {
        self.tick += 1;
        let tick = self.tick;
        let (data, old) = self.entries.get_mut(key)?;
        let old_tick = std::mem::replace(old, tick);
        let data = data.clone();
        if let Some(k) = self.order.remove(&old_tick) {
            self.order.insert(tick, k);
        }
        Some(data)
    }

    fn put(&mut self, key: String, data: Arc<Vec<u8>>) {
        if data.len() > self.cap {
            return;
        }
        if let Some((old, tick)) = self.entries.remove(&key) {
            self.bytes -= old.len();
            self.order.remove(&tick);
        }
        self.tick += 1;
        self.bytes += data.len();
        self.order.insert(self.tick, key.clone());
        self.entries.insert(key, (data, self.tick));
        while self.bytes > self.cap {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            if let Some((removed, _)) = self.entries.remove(&oldest) {
                self.bytes -= removed.len();
            }
        }
    }
}

/// 1 つの IP から読める数の上限 (決まった時間ごとに数え直す)。IPv6 は /64 にまとめる
struct WindowLimit {
    counts: HashMap<String, (Instant, u64)>,
    order: VecDeque<String>,
}

/// 上限を数える単位。IPv6 は /64 にまとめる。IPv4 を写した IPv6 は IPv4 と同じ。IP アドレスでないものはそのまま
pub fn limit_key(ip: &str) -> String {
    match ip_bytes(ip) {
        None => ip.trim().chars().take(64).collect(),
        Some(b) if b.len() == 4 => format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3]),
        Some(b) => {
            let head: String = b[..8].iter().map(|x| format!("{x:02x}")).collect();
            format!("{head}/64")
        }
    }
}

impl WindowLimit {
    fn new() -> Self {
        Self {
            counts: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// 1 つ足して、上限を超えたら true
    fn hit(&mut self, key: &str, now: Instant) -> bool {
        let fresh = self
            .counts
            .get(key)
            .is_none_or(|(start, _)| now.duration_since(*start) > LIMIT_WINDOW);
        if fresh {
            if !self.counts.contains_key(key) {
                self.order.push_back(key.to_string());
            }
            self.counts.insert(key.to_string(), (now, 0));
            // 鍵の数に上限を設ける (ばらばらの鍵でメモリを増やさせない。超えたら古いものから忘れる)
            while self.counts.len() > LIMIT_MAX_KEYS {
                match self.order.pop_front() {
                    Some(old) => {
                        self.counts.remove(&old);
                    }
                    None => break,
                }
            }
        }
        let entry = self.counts.entry(key.to_string()).or_insert((now, 0));
        entry.1 += 1;
        entry.1 > LIMIT_COUNT
    }
}

pub struct Assets {
    store: Arc<dyn AssetStore>,
    origins: Vec<String>,
    files: Mutex<HashMap<String, (Instant, Option<HostedFile>)>>,
    chunks: Mutex<ChunkCache>,
    limit: Mutex<WindowLimit>,
}

impl Assets {
    pub fn new(store: Arc<dyn AssetStore>, origins: Vec<String>) -> Self {
        Self::with_cache_bytes(store, origins, ASSET_CHUNK_CACHE_BYTES)
    }

    pub fn with_cache_bytes(
        store: Arc<dyn AssetStore>,
        origins: Vec<String>,
        cache_bytes: usize,
    ) -> Self {
        Self {
            store,
            origins,
            files: Mutex::new(HashMap::new()),
            chunks: Mutex::new(ChunkCache::new(cache_bytes)),
            limit: Mutex::new(WindowLimit::new()),
        }
    }

    async fn hosted_file(&self, id: &str) -> StoreResult<Option<HostedFile>> {
        {
            let files = self.files.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((at, file)) = files.get(id) {
                let ttl = if file.is_some() {
                    FILE_TTL
                } else {
                    MISSING_TTL
                };
                if at.elapsed() < ttl {
                    return Ok(file.clone());
                }
            }
        }
        let file = self.store.file(id).await?;
        let mut files = self.files.lock().unwrap_or_else(|e| e.into_inner());
        if files.len() >= MAX_FILES {
            files.retain(|_, (at, f)| {
                at.elapsed() < if f.is_some() { FILE_TTL } else { MISSING_TTL }
            });
            if files.len() >= MAX_FILES {
                files.clear();
            }
        }
        files.insert(id.to_string(), (Instant::now(), file.clone()));
        Ok(file)
    }

    async fn chunk(&self, file: &HostedFile, seq: u64) -> StoreResult<Arc<Vec<u8>>> {
        let key = format!("{}:{seq}", file.id);
        if let Some(hit) = self
            .chunks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            return Ok(hit);
        }
        let Some(data) = self.store.chunk(&file.id, seq).await? else {
            return Err(StoreError(format!("missing chunk {key}")));
        };
        let data = Arc::new(data);
        self.chunks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .put(key, data.clone());
        Ok(data)
    }

    fn over_limit(&self, ip: &str) -> bool {
        self.limit
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .hit(&limit_key(ip), Instant::now())
    }

    /// 地図のページの Origin (または Referer の Origin) か。許す一覧にあれば、その Origin
    fn allowed_origin(&self, origin: Option<&str>, referer: Option<&str>) -> Option<String> {
        if let Some(origin) = origin.filter(|o| !o.is_empty()) {
            return self.origins.iter().find(|o| *o == origin).cloned();
        }
        let referer = referer.filter(|r| !r.is_empty())?;
        let url = url::Url::parse(referer).ok()?;
        let origin = url.origin().ascii_serialization();
        self.origins.iter().find(|o| **o == origin).cloned()
    }
}

/// `<12 文字>.<拡張子>` (`^([a-z2-7]{12})\.([a-z0-9]{1,10})$`)
pub fn parse_file_name(name: &str) -> Option<(&str, &str)> {
    let (id, ext) = name.split_once('.')?;
    let id_ok = id.len() == 12 && id.bytes().all(|b| matches!(b, b'a'..=b'z' | b'2'..=b'7'));
    let ext_ok = (1..=10).contains(&ext.len())
        && ext
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    (id_ok && ext_ok).then_some((id, ext))
}

/// Range (1 つの範囲だけ)。なし・対応しない形は `Ok(None)` (全体)、範囲外は `Err`。Companion の HostingServer と同じ
/// 範囲が合わない (416)
#[derive(Debug, PartialEq, Eq)]
pub struct Unsatisfiable;

/// Range (1 つの範囲だけ)。なし・対応しない形は Ok(None) (全体)、範囲外は Err。Companion の HostingServer と同じ
pub fn parse_range(header: Option<&str>, size: u64) -> Result<Option<(u64, u64)>, Unsatisfiable> {
    let Some(spec) = header.map(str::trim).and_then(|h| h.strip_prefix("bytes=")) else {
        return Ok(None);
    };
    let Some((a, b)) = spec.split_once('-') else {
        return Ok(None);
    };
    let digits = |s: &str| s.len() <= 19 && s.bytes().all(|c| c.is_ascii_digit());
    if !digits(a) || !digits(b) {
        return Ok(None);
    }
    if a.is_empty() && b.is_empty() {
        return Ok(None);
    }
    if size == 0 {
        return Err(Unsatisfiable);
    }
    if a.is_empty() {
        let suffix: u64 = b.parse().map_err(|_| Unsatisfiable)?;
        return if suffix == 0 {
            Err(Unsatisfiable)
        } else {
            Ok(Some((size.saturating_sub(suffix), size - 1)))
        };
    }
    let first: u64 = a.parse().map_err(|_| Unsatisfiable)?;
    if first >= size {
        return Err(Unsatisfiable);
    }
    if b.is_empty() {
        return Ok(Some((first, size - 1)));
    }
    let last: u64 = b.parse().map_err(|_| Unsatisfiable)?;
    if last < first {
        return Err(Unsatisfiable);
    }
    Ok(Some((first, last.min(size - 1))))
}

fn deny(status: u16, message: &'static str, origin: Option<&str>) -> Response {
    let mut res = Response::new(Body::from(message));
    *res.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN);
    base_headers(&mut res, origin);
    let headers = res.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

fn base_headers(res: &mut Response, origin: Option<&str>) {
    let headers = res.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    // SVG などにスクリプトがあっても動かさない
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; sandbox"),
    );
    headers.insert(header::VARY, HeaderValue::from_static("Origin"));
    headers.insert("cdn-cache-control", HeaderValue::from_static("no-store"));
    if let Some(origin) = origin
        && let Ok(value) = HeaderValue::from_str(origin)
    {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
    }
}

/// `/_assets/<id>.<拡張子>`
pub async fn handle(svc: &Arc<Map3d>, ctx: &Ctx) -> Response {
    let Some(assets) = &svc.assets else {
        return deny(404, "not found", None);
    };
    let origin_header = ctx
        .headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok());
    let referer_header = ctx
        .headers
        .get(header::REFERER)
        .and_then(|v| v.to_str().ok());
    let origin = assets.allowed_origin(origin_header, referer_header);

    if ctx.method != Method::GET && ctx.method != Method::HEAD {
        let mut res = deny(405, "method not allowed", origin.as_deref());
        res.headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        return res;
    }
    let name = ctx.path.strip_prefix("/_assets/").unwrap_or("");
    let Some((id, ext)) = parse_file_name(name) else {
        return deny(404, "not found", origin.as_deref());
    };

    let ip = ctx.header(header::HeaderName::from_static("x-toro-client-ip"));
    if assets.over_limit(ip) {
        return deny(429, "too many requests", origin.as_deref());
    }

    // MarumaSign の Mod は Origin も Referer も付けない。付いていて地図のページでなければ、ほかのサイトに貼られたもの
    // (遊んでいる人のブラウザでも断る)
    let from_other_site = origin.is_none() && (origin_header.is_some() || referer_header.is_some());
    if from_other_site || (origin.is_none() && !svc.clients.is_player(ip)) {
        return deny(
            403,
            "TORO Server で遊んでいるプレイヤーか、TORO Server の地図からだけ読めます",
            None,
        );
    }
    let file = match assets.hosted_file(id).await {
        Ok(file) => file,
        Err(e) => {
            // DB が止まっている、Companion がまだ表を作っていない、など
            eprintln!("assets: {e}");
            return deny(503, "temporarily unavailable", origin.as_deref());
        }
    };
    let Some(file) = file.filter(|f| f.ext == ext) else {
        return deny(404, "not found", origin.as_deref());
    };

    let etag = format!("\"{}\"", file.id);
    let mut res = Response::new(Body::empty());
    base_headers(&mut res, origin.as_deref());
    let headers = res.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&file.content_type) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    if let Ok(value) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, value);
    }
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if ctx.header(header::IF_NONE_MATCH) == etag {
        *res.status_mut() = StatusCode::NOT_MODIFIED;
        return res;
    }

    let range_header = ctx.headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let range = match parse_range(range_header, file.size) {
        Ok(range) => range,
        Err(Unsatisfiable) => {
            let mut res = deny(416, "range not satisfiable", origin.as_deref());
            if let Ok(value) = HeaderValue::from_str(&format!("bytes */{}", file.size)) {
                res.headers_mut().insert(header::CONTENT_RANGE, value);
            }
            return res;
        }
    };
    let (start, end) = range.unwrap_or((0, file.size.saturating_sub(1)));
    let length = if file.size == 0 { 0 } else { end - start + 1 };
    if range.is_some() {
        *res.status_mut() = StatusCode::PARTIAL_CONTENT;
        if let Ok(value) = HeaderValue::from_str(&format!("bytes {start}-{end}/{}", file.size)) {
            res.headers_mut().insert(header::CONTENT_RANGE, value);
        }
    }
    res.headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    if ctx.method == Method::HEAD || length == 0 {
        return res;
    }
    *res.body_mut() = Body::from_stream(slices(svc.clone(), file, start, end));
    res
}

/// 範囲に入るかたまりを、1 つずつ読んで流す
fn slices(
    svc: Arc<Map3d>,
    file: HostedFile,
    start: u64,
    end: u64,
) -> impl futures_util::Stream<Item = Result<Bytes, io::Error>> {
    let chunk_size = file.chunk_size.max(1);
    let first = start / chunk_size;
    stream::unfold((svc, file, first), move |(svc, file, seq)| async move {
        if seq >= file.chunks || seq * chunk_size > end {
            return None;
        }
        let Some(assets) = &svc.assets else {
            return None;
        };
        let item = match assets.chunk(&file, seq).await {
            Err(e) => Err(io::Error::other(e.0)),
            Ok(chunk) => {
                let base = seq * chunk_size;
                let from = usize::try_from(start.saturating_sub(base)).unwrap_or(usize::MAX);
                let to = usize::try_from(end + 1 - base)
                    .unwrap_or(usize::MAX)
                    .min(chunk.len());
                if from >= to {
                    Ok(Bytes::new())
                } else {
                    Ok(Bytes::copy_from_slice(&chunk[from..to]))
                }
            }
        };
        let next = if item.is_err() { u64::MAX / 2 } else { seq + 1 };
        Some((item, (svc, file, next)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names() {
        assert_eq!(
            parse_file_name("abcdefghijkl.png"),
            Some(("abcdefghijkl", "png"))
        );
        assert_eq!(
            parse_file_name("abcdefghij27.webp"),
            Some(("abcdefghij27", "webp"))
        );
        assert_eq!(parse_file_name("abcdefghijk.png"), None);
        assert_eq!(parse_file_name("abcdefghijkl1.png"), None); // 1 は使わない
        assert_eq!(parse_file_name("ABCDEFGHIJKL.png"), None);
        assert_eq!(parse_file_name("abcdefghijkl."), None);
        assert_eq!(parse_file_name("abcdefghijkl.PNG"), None);
        assert_eq!(parse_file_name("abcdefghijkl.abcdefghijk"), None);
        assert_eq!(parse_file_name("abcdefghijkl"), None);
        assert_eq!(parse_file_name("abcdefghijkl.png/x"), None);
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range(None, 100), Ok(None));
        assert_eq!(parse_range(Some("bytes=0-9"), 100), Ok(Some((0, 9))));
        assert_eq!(parse_range(Some("bytes=90-"), 100), Ok(Some((90, 99))));
        assert_eq!(parse_range(Some("bytes=-10"), 100), Ok(Some((90, 99))));
        assert_eq!(parse_range(Some("bytes=-500"), 100), Ok(Some((0, 99))));
        assert_eq!(parse_range(Some("bytes=50-500"), 100), Ok(Some((50, 99))));
        assert_eq!(parse_range(Some("bytes=100-"), 100), Err(Unsatisfiable));
        assert_eq!(parse_range(Some("bytes=5-2"), 100), Err(Unsatisfiable));
        assert_eq!(parse_range(Some("bytes=-0"), 100), Err(Unsatisfiable));
        assert_eq!(parse_range(Some("bytes=0-0"), 0), Err(Unsatisfiable));
        // 対応しない形は、全体として返す
        assert_eq!(parse_range(Some("bytes=-"), 100), Ok(None));
        assert_eq!(parse_range(Some("bytes=0-1,5-6"), 100), Ok(None));
        assert_eq!(parse_range(Some("items=0-1"), 100), Ok(None));
        assert_eq!(parse_range(Some("bytes=a-b"), 100), Ok(None));
    }

    #[test]
    fn limit_keys_group_ipv6_by_64() {
        assert_eq!(limit_key("203.0.113.5"), "203.0.113.5");
        assert_eq!(limit_key("::ffff:203.0.113.5"), "203.0.113.5");
        assert_eq!(
            limit_key("2001:db8:1:2:aaaa::1"),
            limit_key("2001:db8:1:2:bbbb::2")
        );
        assert_ne!(limit_key("2001:db8:1:2::1"), limit_key("2001:db8:1:3::1"));
        assert_eq!(limit_key("unknown"), "unknown");
    }

    #[test]
    fn window_limit_counts_per_key_and_resets() {
        let mut limit = WindowLimit::new();
        let t0 = Instant::now();
        for _ in 0..LIMIT_COUNT {
            assert!(!limit.hit("a", t0));
        }
        assert!(limit.hit("a", t0));
        assert!(!limit.hit("b", t0));
        assert!(!limit.hit("a", t0 + LIMIT_WINDOW + Duration::from_secs(1)));
    }

    #[test]
    fn chunk_cache_evicts_oldest_and_respects_budget() {
        let mut cache = ChunkCache::new(10);
        cache.put("a".into(), Arc::new(vec![0; 4]));
        cache.put("b".into(), Arc::new(vec![0; 4]));
        assert!(cache.get("a").is_some()); // a を使った → b が古い
        cache.put("c".into(), Arc::new(vec![0; 4]));
        assert!(cache.get("b").is_none());
        assert!(cache.get("a").is_some());
        assert!(cache.get("c").is_some());
        assert!(cache.bytes <= 10);
        // 上限より大きいものは入れない
        cache.put("big".into(), Arc::new(vec![0; 11]));
        assert!(cache.get("big").is_none());
        assert!(cache.bytes <= 10);
    }
}
