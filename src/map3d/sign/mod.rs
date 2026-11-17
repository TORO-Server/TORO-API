//! `/map3d/v1/marumasign/...`: 看板データ、転送先の確認 (resolve)、中継 (proxy)。
//!
//! 看板の URL は誰でも書けるので、中継がただの中継 (オープンプロキシ) にならないようにする:
//! - 読むのは、看板データにいまある URL、その転送先、それらの再生リスト (HLS, MPEG-DASH) と .gltf に書かれた URL だけ (`allow`)
//! - https だけ。行き先ごとに名前を引いたアドレスを確かめ、サーバーの中・家の中・共有アドレスなどは読まない (`net`)
//! - 中身は先頭のバイト列で種類を決め、画像・動画・音声・PDF・3D のもの・再生リストだけを返す。HTML は返さない。
//!   大きさは種類ごとに MarumaSign 本体と同じ上限 (`kinds`)
//! - 返すものは、このサイトのページとして開けない (CSP sandbox、nosniff、決まった Content-Type、Content-Disposition: attachment)
//! - 地図のページ (許可した Origin) からだけ。1 つの IP からの数と量、全体の量、同時に読む数に上限 (`limits`)
//! - Cookie・Referer・ブラウザーの頭書きは送らない
//!
//! 画面 (`3d-map.torosaba.net`) は API (`api.torosaba.net`) とは別のオリジン (同じサイト) になる。
//! そのため `Sec-Fetch-Site: same-site` は、`Origin` が許可一覧 (`torosaba.net` とそのサブドメイン、`localhost`、`127.0.0.1`)
//! にあるときに許可し、`Cross-Origin-Resource-Policy` は `same-site` にする。
//!
//! 同時に読む数は、このサービス全体の同時処理の枠とは別に数える (長い配信で枠を埋めない)。
//! 正本 (移管前) は TORO-WebMap の `server/lib/marumasign/` と `server/routes/_marumasign/`。
//! 移管前と違う点: 元のサイトが `Accept-Encoding: identity` を無視して圧縮して返したときは、展開せず 502 にする。
//!
//! # 環境変数
//! - `MAP_MARUMASIGN_ENABLED`: `false` か `0` なら、この機能を動かさない (既定は動かす)
//! - `MAP_MARUMASIGN_MAPS`: 看板データを読む地図の ID (`,` 区切り)。空なら BlueMap の `settings.json` の地図の一覧
//! - `MAP_MARUMASIGN_REFRESH_SECONDS`: 看板データを読み直す間隔 (秒。既定 60)
//! - `MAP_MARUMASIGN_CORS_ORIGIN`: 転送先の確認で、CORS を確かめるときに名乗る Origin (既定は 3D 地図のページ)

pub mod allow;
pub mod core;
pub mod kinds;
pub mod limits;
pub mod manifest;
pub mod net;
pub mod xml_head;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::http::StatusCode;
use axum::http::header::{self, HeaderName, HeaderValue};
use axum::response::Response;
use futures_util::stream;
use tokio::sync::OnceCell;

use self::allow::{AllowList, Allowed, sign_urls};
use self::core::extension_of;
use self::kinds::{
    Kind, MAX_GLTF_PARSE_BYTES, MAX_MANIFEST_BYTES, ManifestKind, classify, clean_type,
};
use self::limits::{ByteCache, Gate, GatePermit, WindowLimit, limit_key};
use self::manifest::manifest_refs;
use self::net::{Outbound, RESOLVE_MAX, UpstreamError, UpstreamRequest, build_client, check_url};
use super::budget::{SIGN_BODY_CACHE_BYTES, SIGN_BODY_CACHE_ENTRY_BYTES};
use super::live::web_url;
use super::store::BluemapStore;
use super::{Body3, error, respond};
use crate::http::Ctx;

const WINDOW: Duration = Duration::from_secs(10 * 60);
const GIB: u64 = 1024 * 1024 * 1024;
/// 1 つの IP から 10 分あたり (看板の多い場所や、ライブ配信の分割されたファイルで数が増えるので多め)
const PROXY_REQUESTS_PER_IP: u64 = 4000;
const RESOLVE_REQUESTS_PER_IP: u64 = 1500;
/// 1 つの IP から 10 分あたりに中継する量と、全体の量
const BYTES_PER_IP: u64 = 3 * GIB;
const BYTES_TOTAL: u64 = 12 * GIB;
/// 同時に元のサイトへつなぐ数 (全体、1 つの IP)。このサービス全体の同時処理の枠 (256) とは別
const GATE_TOTAL: usize = 64;
const GATE_PER_IP: usize = 16;
/// 終わらない配信を 1 回でつないでおける時間
const STREAM_MAX: Duration = Duration::from_secs(30 * 60);
/// 終わる種類のファイルを読み終えるまでの時間
const FILE_MAX: Duration = Duration::from_secs(3 * 60);
/// 転送を追って先頭のバイト列を読むまでの時間
const START_MAX: Duration = Duration::from_secs(30);
/// 再生リスト 1 つを読み終えるまでの時間
const MANIFEST_MAX: Duration = Duration::from_secs(30);
const MANIFEST_READ_REUSE: Duration = Duration::from_secs(4);
const BODY_TTL: Duration = Duration::from_secs(5 * 60);
const KIND_TTL: Duration = Duration::from_secs(30 * 60);
const OK_TTL: Duration = Duration::from_secs(10 * 60);
const FAIL_TTL: Duration = Duration::from_secs(60);
const MAX_SIGN_DATA_BYTES: usize = 64 * 1024 * 1024;
const HEAD_BYTES: usize = 4096;

/// 看板中継の設定
#[derive(Clone)]
pub struct SignConfig {
    pub enabled: bool,
    pub maps: Vec<String>,
    pub refresh_seconds: u64,
    pub cors_origin: String,
    /// 試験のときだけ (環境変数からは読まない): 決まった名前を、決まったアドレスへ http でつなぐ
    pub test_hosts: HashMap<String, SocketAddr>,
}

impl Default for SignConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            maps: Vec::new(),
            refresh_seconds: 60,
            cors_origin: "https://3d-map.torosaba.net".to_string(),
            test_hosts: HashMap::new(),
        }
    }
}

impl SignConfig {
    pub fn from_env() -> Result<Self, String> {
        let mut cfg = Self::default();
        if let Ok(v) = std::env::var("MAP_MARUMASIGN_ENABLED") {
            cfg.enabled = !matches!(v.trim().to_ascii_lowercase().as_str(), "false" | "0" | "no");
        }
        if let Ok(v) = std::env::var("MAP_MARUMASIGN_MAPS") {
            cfg.maps = v
                .split(',')
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty())
                .collect();
        }
        if let Ok(v) = std::env::var("MAP_MARUMASIGN_REFRESH_SECONDS") {
            cfg.refresh_seconds = v
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or("MAP_MARUMASIGN_REFRESH_SECONDS must be a positive number")?;
        }
        if let Ok(v) = std::env::var("MAP_MARUMASIGN_CORS_ORIGIN")
            && !v.trim().is_empty()
        {
            cfg.cors_origin = v.trim().to_string();
        }
        Ok(cfg)
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 地図の ID (`[\w.-]{1,64}`)
pub fn valid_map_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
}

/// 画面を出してよい Origin か (`torosaba.net` とそのサブドメイン、`localhost`、`127.0.0.1`。ポート可)
pub fn origin_allowed(origin: &str) -> bool {
    let Ok(url) = url::Url::parse(origin) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") || url.origin().ascii_serialization() != origin {
        return false;
    }
    match url.host_str() {
        Some(host) => {
            host == "localhost"
                || host == "127.0.0.1"
                || host == "torosaba.net"
                || host.ends_with(".torosaba.net")
        }
        None => false,
    }
}

#[derive(Clone)]
struct CachedBody {
    body: Bytes,
    ty: &'static str,
    upstream_type: String,
    url: String,
    kind: &'static str,
}

#[derive(Clone)]
struct KindEntry {
    kind: Kind,
    foreign: bool,
}

#[derive(Clone)]
struct Answer {
    json: String,
    status: u16,
}

type ManifestReads = HashMap<String, (Instant, Arc<OnceCell<bool>>)>;

#[derive(Default)]
struct LoadState {
    loaded_at: Option<Instant>,
}

pub struct Sign {
    cfg: SignConfig,
    /// 内部 (BlueMap の内蔵サーバー) へ
    http: reqwest::Client,
    live_url: String,
    /// 外向き (看板の URL の先)
    client: reqwest::Client,
    out: Outbound,
    allow: Mutex<AllowList>,
    state: tokio::sync::Mutex<LoadState>,
    request_limit: WindowLimit,
    resolve_limit: WindowLimit,
    byte_limit: WindowLimit,
    total_byte_limit: WindowLimit,
    gate: Arc<Gate>,
    bodies: ByteCache<CachedBody>,
    kinds: ByteCache<KindEntry>,
    answers: ByteCache<Answer>,
    manifest_reads: Mutex<ManifestReads>,
}

impl Sign {
    pub fn new(
        cfg: &SignConfig,
        http: reqwest::Client,
        _store: Arc<dyn BluemapStore>,
        live_url: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            cfg: cfg.clone(),
            http,
            live_url,
            client: build_client(),
            out: Outbound {
                test_hosts: cfg.test_hosts.clone(),
            },
            allow: Mutex::new(AllowList::default()),
            state: tokio::sync::Mutex::new(LoadState::default()),
            request_limit: WindowLimit::new(WINDOW, PROXY_REQUESTS_PER_IP),
            resolve_limit: WindowLimit::new(WINDOW, RESOLVE_REQUESTS_PER_IP),
            byte_limit: WindowLimit::new(WINDOW, BYTES_PER_IP),
            total_byte_limit: WindowLimit::new(WINDOW, BYTES_TOTAL),
            gate: Gate::new(GATE_TOTAL, GATE_PER_IP),
            bodies: ByteCache::new(SIGN_BODY_CACHE_BYTES, 10_000),
            kinds: ByteCache::new(4 * 1024 * 1024, 20_000),
            answers: ByteCache::new(8 * 1024 * 1024, 20_000),
            manifest_reads: Mutex::new(HashMap::new()),
        })
    }

    fn allowed(&self, url: &str) -> Option<Allowed> {
        lock(&self.allow).allowed(url)
    }

    fn alias_all(&self, chain: &[String]) {
        let mut allow = lock(&self.allow);
        for hop in chain.iter().skip(1) {
            allow.add_alias(hop);
        }
    }

    /// n バイト中継したことを数える。上限を超えたら true
    fn count_bytes(&self, ip: &str, n: u64) -> bool {
        let over_ip = self.byte_limit.hit(ip, n);
        self.total_byte_limit.hit("all", n) || over_ip
    }

    fn over_bytes(&self, ip: &str) -> bool {
        self.byte_limit.over(ip) || self.total_byte_limit.over("all")
    }

    // ------------------------------------------------------------ 看板データ

    /// BlueMap の web ルートからの取得。404 は Ok(None)
    async fn get_web(&self, segments: &[&str]) -> Result<Option<Vec<u8>>, ()> {
        let url = web_url(&self.live_url, segments).ok_or(())?;
        let mut res = self
            .http
            .get(url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|_| ())?;
        if res.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !res.status().is_success() {
            return Err(());
        }
        let mut body = Vec::new();
        while let Some(chunk) = res.chunk().await.map_err(|_| ())? {
            if body.len() + chunk.len() > MAX_SIGN_DATA_BYTES {
                return Err(());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Some(body))
    }

    async fn load_signs(&self) -> Result<(), ()> {
        let mut maps = self.cfg.maps.clone();
        if maps.is_empty()
            && let Some(settings) = self.get_web(&["settings.json"]).await?
            && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&settings)
        {
            maps = json
                .get("maps")
                .and_then(|m| m.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|m| m.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
        }
        let mut urls = std::collections::HashSet::new();
        let mut ok = 0;
        for id in maps.iter().take(64).filter(|id| valid_map_id(id)) {
            // 看板のない地図 (ファイルがない) もある
            if let Ok(Some(data)) = self.get_web(&["marumasign", &format!("{id}.json")]).await
                && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&data)
            {
                sign_urls(&json, &mut urls);
                ok += 1;
            }
        }
        if ok == 0 && !maps.is_empty() {
            return Err(());
        }
        lock(&self.allow).set_roots(urls);
        Ok(())
    }

    /// 看板データを、古ければ読み直す。`max_age` があれば、その時間より古いときに読み直す (知らない URL が来たとき。置いたばかりの看板)
    async fn ensure_signs(&self, max_age: Option<Duration>) {
        let refresh = Duration::from_secs(self.cfg.refresh_seconds);
        let mut state = self.state.lock().await;
        let age = state.loaded_at.map(|t| t.elapsed());
        let need = match (max_age, age) {
            (_, None) => true,
            (Some(max), Some(age)) => age >= max,
            (None, Some(age)) => age >= refresh,
        };
        if !need {
            return;
        }
        state.loaded_at = Some(Instant::now());
        if self.load_signs().await.is_err() {
            // 読めなければ前のものを使い続け、少し後にやり直す
            state.loaded_at =
                Instant::now().checked_sub(refresh.saturating_sub(Duration::from_secs(10)));
        }
    }

    /// 再生リストなど 1 つを読んで、書かれた URL を一覧に足す。読めたか
    async fn read_manifest(&self, url: &str) -> bool {
        let cell = {
            let mut reads = lock(&self.manifest_reads);
            if reads.len() > 2000 {
                reads.retain(|_, (at, _)| at.elapsed() < MANIFEST_READ_REUSE);
            }
            match reads.get(url) {
                Some((at, cell)) if at.elapsed() < MANIFEST_READ_REUSE => cell.clone(),
                _ => {
                    let cell = Arc::new(OnceCell::new());
                    reads.insert(url.to_string(), (Instant::now(), cell.clone()));
                    cell
                }
            }
        };
        *cell
            .get_or_init(|| async {
                matches!(
                    tokio::time::timeout(MANIFEST_MAX, self.read_manifest_inner(url)).await,
                    Ok(Ok(()))
                )
            })
            .await
    }

    async fn read_manifest_inner(&self, url: &str) -> Result<(), UpstreamError> {
        let _permit = self
            .gate
            .enter("manifest")
            .ok_or_else(|| UpstreamError::new("busy", 503))?;
        let req = UpstreamRequest {
            timeout: Some(Duration::from_secs(15)),
            ..Default::default()
        };
        let f = self.out.follow(&self.client, url, &req).await?;
        self.alias_all(&f.chain);
        let mut res = f.res;
        if res.status().as_u16() != 200 {
            return Err(UpstreamError::new("upstream status", 502));
        }
        let mut body = Vec::new();
        while let Some(chunk) = res
            .chunk()
            .await
            .map_err(|_| UpstreamError::new("upstream error", 502))?
        {
            if (body.len() + chunk.len()) as u64 > MAX_GLTF_PARSE_BYTES {
                return Err(UpstreamError::new("manifest too large", 413));
            }
            body.extend_from_slice(&chunk);
        }
        let head = &body[..body.len().min(HEAD_BYTES)];
        let Some(kind) = classify(head, "", &extension_of(&f.url), true, false) else {
            return Ok(());
        };
        let Some(manifest) = kind.manifest else {
            return Ok(());
        };
        if manifest != ManifestKind::Gltf && body.len() as u64 > MAX_MANIFEST_BYTES {
            return Ok(());
        }
        if let Some(refs) = manifest_refs(&String::from_utf8_lossy(&body), &f.url) {
            lock(&self.allow).add_refs(refs, &f.url);
        }
        Ok(())
    }

    /// 読んでよい URL か。看板の URL・その転送先・知っている再生リストに書かれた URL ならそのまま。
    /// そうでなければ、`parents` (看板の URL からの再生リストのたどり方。ブラウザーが知らせる) を、サーバーが自分で読んで確かめる
    async fn is_allowed(&self, url: &str, parents: &[String]) -> Option<Allowed> {
        let now = self.allowed(url);
        if now.is_some() {
            return now;
        }
        // 置いたばかりの看板かもしれない (看板データを 5 秒より前に読んだなら、読み直す)
        if parents.is_empty() || self.allowed(&parents[0]).is_none() {
            self.ensure_signs(Some(Duration::from_secs(5))).await;
            let now = self.allowed(url);
            if now.is_some() || parents.is_empty() {
                return now;
            }
        }
        self.allowed(&parents[0])?;
        for p in parents {
            self.allowed(p)?;
            if !self.read_manifest(p).await {
                return None;
            }
        }
        self.allowed(url)
    }

    // ------------------------------------------------------------ 経路

    async fn sign_data(&self, ctx: &Ctx, id: &str) -> Response {
        match self.get_web(&["marumasign", &format!("{id}.json")]).await {
            Ok(Some(data)) => respond(
                ctx,
                Body3 {
                    data,
                    content_type: "application/json",
                    cache: "no-cache",
                    etag: None,
                    encoding: None,
                },
            ),
            Ok(None) => error(404, "Not Found"),
            Err(()) => error(502, "Bad Gateway"),
        }
    }

    async fn resolve(&self, ctx: &Ctx) -> Response {
        if !from_map_page(ctx) {
            return deny(403, "forbidden");
        }
        let ip = client_ip(ctx);
        if self.resolve_limit.hit(&ip, 1) {
            return deny(429, "too many requests");
        }
        let Some(q) = query_urls(ctx) else {
            return deny(400, "bad url");
        };
        self.ensure_signs(None).await;
        if self.is_allowed(&q.url, &q.parents).await.is_none() {
            return deny(403, "not a sign url");
        }
        let answer = match self.answers.get(&q.url) {
            Some(answer) => answer,
            None => {
                let Some(_permit) = self.gate.enter(&ip) else {
                    return deny(503, "busy");
                };
                self.lookup(&q.url).await
            }
        };
        let mut res = Response::new(Body::from(answer.json));
        *res.status_mut() = StatusCode::from_u16(answer.status).unwrap_or(StatusCode::BAD_GATEWAY);
        let headers = res.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, max-age=300"),
        );
        res
    }

    async fn lookup(&self, url: &str) -> Answer {
        match self
            .out
            .resolve_redirects(&self.client, url, &self.cfg.cors_origin, RESOLVE_MAX)
            .await
        {
            Ok(r) => {
                // 行き先も読んでよい (中継でも)
                self.alias_all(&r.chain);
                let json = serde_json::json!({
                    "url": r.url,
                    "redirected": r.chain.len() > 1,
                    "cors": r.cors,
                    "status": r.status,
                    "type": r.ty,
                })
                .to_string();
                let answer = Answer { json, status: 200 };
                self.answers
                    .set(url, answer.clone(), answer.json.len(), OK_TTL);
                answer
            }
            Err(e) => {
                let json = serde_json::json!({ "error": e.message }).to_string();
                let answer = Answer {
                    json,
                    status: if e.status == 403 { 403 } else { 502 },
                };
                self.answers
                    .set(url, answer.clone(), answer.json.len(), FAIL_TTL);
                answer
            }
        }
    }

    async fn proxy(self: &Arc<Self>, ctx: &Ctx) -> Response {
        if !from_map_page(ctx) {
            return deny(403, "forbidden");
        }
        let ip = client_ip(ctx);
        if self.request_limit.hit(&ip, 1) || self.over_bytes(&ip) {
            return deny(429, "too many requests");
        }
        let Some(q) = query_urls(ctx) else {
            return deny(400, "bad url");
        };
        self.ensure_signs(None).await;
        let Some(why) = self.is_allowed(&q.url, &q.parents).await else {
            return deny(403, "not a sign url");
        };
        let child = matches!(why, Allowed::Child | Allowed::Foreign);
        let foreign = why == Allowed::Foreign;
        let range = match parse_range(ctx.headers.get(header::RANGE).and_then(|v| v.to_str().ok()))
        {
            Ok(range) => range,
            Err(()) => return deny(416, "bad range"),
        };
        let head_only = ctx.method == axum::http::Method::HEAD;

        if let Some(cached) = self.bodies.get(&q.url) {
            return send_body(&cached, range, false);
        }
        let Some(permit) = self.gate.enter(&ip) else {
            return deny(503, "busy");
        };
        match self
            .proxy_fetch(&q.url, child, foreign, range, head_only, &ip, permit)
            .await
        {
            Ok(res) => res,
            Err(e) => deny(
                match e.status {
                    403 => 403,
                    s if s >= 500 => s,
                    s if s == 404 || s == 413 || s == 415 || s == 429 => s,
                    _ => 502,
                },
                &e.message,
            ),
        }
    }

    /// 先頭だけを読んで種類を決める (途中から読むとき)
    async fn sniff_only(
        &self,
        url: &str,
        child: bool,
        foreign: bool,
    ) -> Result<Option<Kind>, UpstreamError> {
        let req = UpstreamRequest {
            headers: vec![("range", "bytes=0-4095".to_string())],
            timeout: Some(Duration::from_secs(15)),
            ..Default::default()
        };
        let mut f = self.out.follow(&self.client, url, &req).await?;
        let status = f.res.status().as_u16();
        if (status != 200 && status != 206) || is_encoded(&f.res) {
            return Ok(None);
        }
        let ty = clean_type(
            f.res
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
        );
        let (head, _) = read_head(&mut f.res).await?;
        let kind = classify(&head, &ty, &extension_of(&f.url), child, foreign);
        if let Some(kind) = &kind {
            self.remember_kind(url, kind, foreign);
        }
        Ok(kind)
    }

    fn remember_kind(&self, url: &str, kind: &Kind, foreign: bool) {
        self.kinds.set(
            url,
            KindEntry {
                kind: kind.clone(),
                foreign,
            },
            200,
            KIND_TTL,
        );
    }

    fn cached_kind(&self, url: &str, foreign: bool) -> Option<Kind> {
        self.kinds
            .get(url)
            .filter(|e| e.foreign == foreign)
            .map(|e| e.kind)
    }

    #[allow(clippy::too_many_arguments)]
    async fn proxy_fetch(
        self: &Arc<Self>,
        url: &str,
        child: bool,
        foreign: bool,
        range: Option<Range>,
        head_only: bool,
        ip: &str,
        permit: GatePermit,
    ) -> Result<Response, UpstreamError> {
        let started = tokio::time::timeout(START_MAX, self.start_fetch(url, child, foreign, range))
            .await
            .map_err(|_| UpstreamError::new("upstream timeout", 504))??;
        let Started {
            mut res,
            url: final_url,
            upstream_type,
            content_range,
            head,
            ended,
            kind,
            total,
        } = started;
        let from_start = content_range.is_none_or(|r| r.start == 0);
        let status = res.status().as_u16();
        if total.is_some_and(|t| t > kind.cap) {
            return Err(UpstreamError::new("too large", 413));
        }

        // 再生リストと .gltf (16 MiB まで) は全部読んで、書かれた URL を一覧に足す
        let parse = kind.manifest.is_some()
            && from_start
            && status == 200
            && (kind.manifest != Some(ManifestKind::Gltf)
                || total.is_some_and(|t| t <= MAX_GLTF_PARSE_BYTES));
        let small = status == 200
            && !kind.stream
            && total.is_some_and(|t| t <= SIGN_BODY_CACHE_ENTRY_BYTES as u64);
        if parse || small {
            let limit = if parse {
                kind.cap.min(MAX_GLTF_PARSE_BYTES)
            } else {
                SIGN_BODY_CACHE_ENTRY_BYTES as u64
            };
            let all = tokio::time::timeout(FILE_MAX, read_all(&mut res, head, ended, limit))
                .await
                .map_err(|_| UpstreamError::new("upstream timeout", 504))??;
            if parse
                && all.len() as u64 <= MAX_GLTF_PARSE_BYTES
                && let Some(refs) = manifest_refs(&String::from_utf8_lossy(&all), &final_url)
            {
                lock(&self.allow).add_refs(refs, &final_url);
            }
            if self.count_bytes(ip, all.len() as u64) {
                return Err(UpstreamError::new("too many bytes", 429));
            }
            let live = matches!(kind.manifest, Some(ManifestKind::Hls | ManifestKind::Dash));
            let cached = CachedBody {
                body: Bytes::from(all),
                ty: kind.ty,
                upstream_type,
                url: final_url,
                kind: kind.name,
            };
            // 再生リストはライブ配信で書き換わるので覚えない
            if !live && cached.body.len() <= SIGN_BODY_CACHE_ENTRY_BYTES {
                self.bodies
                    .set(url, cached.clone(), cached.body.len(), BODY_TTL);
            }
            return Ok(send_body(&cached, range, live));
        }

        // そのまま流す (動画、大きなファイル、配信)
        let mut out = Response::new(Body::empty());
        let headers = out.headers_mut();
        media_headers(
            headers,
            kind.ty,
            &upstream_type,
            &final_url,
            kind.name,
            if kind.stream && total.is_none() {
                "no-store"
            } else {
                "private, max-age=300"
            },
            if kind.stream { "bytes" } else { "none" },
        );
        let cap = match content_range {
            Some(r) => {
                *out.status_mut() = StatusCode::PARTIAL_CONTENT;
                let headers = out.headers_mut();
                let total_text = r.total.map_or("*".to_string(), |t| t.to_string());
                if let Ok(v) =
                    HeaderValue::from_str(&format!("bytes {}-{}/{}", r.start, r.end, total_text))
                {
                    headers.insert(header::CONTENT_RANGE, v);
                }
                headers.insert(header::CONTENT_LENGTH, (r.end - r.start + 1).into());
                r.end - r.start + 1
            }
            None => {
                if let Some(t) = total {
                    out.headers_mut().insert(header::CONTENT_LENGTH, t.into());
                }
                kind.cap
            }
        };
        if head_only {
            return Ok(out);
        }
        // 最初に読んだぶんを数える。上限を超えていたら、頭を返す前に断る
        let sent = head.len() as u64;
        if sent > cap || (sent > 0 && self.count_bytes(ip, sent)) {
            return Err(UpstreamError::new("too large", 413));
        }
        let deadline = Instant::now() + if kind.stream { STREAM_MAX } else { FILE_MAX };
        let state = StreamState {
            res: Some(res),
            first: (!head.is_empty()).then_some(Bytes::from(head)),
            ended,
            sign: self.clone(),
            ip: ip.to_string(),
            cap,
            sent,
            deadline,
            _permit: permit,
        };
        *out.body_mut() = Body::from_stream(stream::unfold(state, next_chunk));
        Ok(out)
    }

    async fn start_fetch(
        &self,
        url: &str,
        child: bool,
        foreign: bool,
        range: Option<Range>,
    ) -> Result<Started, UpstreamError> {
        let mut kind = self.cached_kind(url, foreign);
        // 途中から読むときは、中身の種類を先に確かめる
        if let Some(r) = range
            && r.start > 0
            && kind.is_none()
        {
            kind = Some(
                self.sniff_only(url, child, foreign)
                    .await?
                    .ok_or_else(|| UpstreamError::new("unsupported content", 415))?,
            );
        }
        let mut req = UpstreamRequest {
            timeout: Some(Duration::from_secs(15)),
            ..Default::default()
        };
        if let Some(r) = range {
            req.headers.push((
                "range",
                format!(
                    "bytes={}-{}",
                    r.start,
                    r.end.map_or(String::new(), |e| e.to_string())
                ),
            ));
        }
        let f = self.out.follow(&self.client, url, &req).await?;
        self.alias_all(&f.chain);
        let mut res = f.res;
        let status = res.status().as_u16();
        if status != 200 && status != 206 {
            return Err(UpstreamError::new(
                &format!("upstream {status}"),
                if status == 404 || status == 410 {
                    404
                } else {
                    502
                },
            ));
        }
        if is_encoded(&res) {
            return Err(UpstreamError::new("encoded content", 502));
        }
        let upstream_type = clean_type(
            res.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
        );
        let mut content_range = None;
        if status == 206 {
            content_range = Some(
                parse_content_range(
                    res.headers()
                        .get("content-range")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or(""),
                )
                .ok_or_else(|| UpstreamError::new("bad upstream range", 502))?,
            );
        }
        let from_start = content_range.is_none_or(|r| r.start == 0);
        let (mut head, mut ended) = (Vec::new(), false);
        if from_start {
            (head, ended) = read_head(&mut res).await?;
            let k = classify(&head, &upstream_type, &extension_of(&f.url), child, foreign)
                .ok_or_else(|| UpstreamError::new("unsupported content", 415))?;
            self.remember_kind(url, &k, foreign);
            kind = Some(k);
        }
        let kind = kind.ok_or_else(|| UpstreamError::new("unsupported content", 415))?;
        let total = match content_range {
            Some(r) => r.total,
            None => res.content_length(),
        };
        Ok(Started {
            res,
            url: f.url,
            upstream_type,
            content_range,
            head,
            ended,
            kind,
            total,
        })
    }
}

struct Started {
    res: reqwest::Response,
    url: String,
    upstream_type: String,
    content_range: Option<ContentRange>,
    head: Vec<u8>,
    ended: bool,
    kind: Kind,
    total: Option<u64>,
}

#[derive(Clone, Copy)]
struct ContentRange {
    start: u64,
    end: u64,
    total: Option<u64>,
}

#[derive(Clone, Copy)]
pub struct Range {
    start: u64,
    end: Option<u64>,
}

/// `Range: bytes=N-M` (1 つの範囲だけ)。対応しない形 (いくつもの範囲、末尾から) は全体を返す (Ok(None))。`end < start` は Err
fn parse_range(value: Option<&str>) -> Result<Option<Range>, ()> {
    let Some(value) = value else { return Ok(None) };
    let Some(spec) = value.trim().strip_prefix("bytes=") else {
        return Ok(None);
    };
    let Some((start, end)) = spec.split_once('-') else {
        return Ok(None);
    };
    let digits = |s: &str, min: usize| {
        s.len() >= min && s.len() <= 15 && s.bytes().all(|b| b.is_ascii_digit())
    };
    if !digits(start, 1) || !(end.is_empty() || digits(end, 1)) {
        return Ok(None);
    }
    let start: u64 = start.parse().map_err(|_| ())?;
    let end = if end.is_empty() {
        None
    } else {
        Some(end.parse::<u64>().map_err(|_| ())?)
    };
    if end.is_some_and(|e| e < start) {
        return Err(());
    }
    Ok(Some(Range { start, end }))
}

/// `Content-Range: bytes N-M/T` (T は `*` でもよい)
fn parse_content_range(value: &str) -> Option<ContentRange> {
    let rest = value.strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    Some(ContentRange {
        start: start.parse().ok()?,
        end: end.parse().ok()?,
        total: if total == "*" {
            None
        } else {
            Some(total.parse().ok()?)
        },
    })
}

fn is_encoded(res: &reqwest::Response) -> bool {
    res.headers()
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| !v.trim().eq_ignore_ascii_case("identity") && !v.trim().is_empty())
}

/// 先頭を、少なくとも `HEAD_BYTES` バイト (終わればそこまで) 読む。後ろの値は、終わりまで読んだか
async fn read_head(res: &mut reqwest::Response) -> Result<(Vec<u8>, bool), UpstreamError> {
    let mut head = Vec::new();
    while head.len() < HEAD_BYTES {
        match res.chunk().await {
            Ok(Some(chunk)) => head.extend_from_slice(&chunk),
            Ok(None) => return Ok((head, true)),
            Err(_) => return Err(UpstreamError::new("upstream error", 502)),
        }
    }
    Ok((head, false))
}

/// 残りを全部読む (`limit` を超えたら 413)
async fn read_all(
    res: &mut reqwest::Response,
    head: Vec<u8>,
    ended: bool,
    limit: u64,
) -> Result<Vec<u8>, UpstreamError> {
    let mut all = head;
    if all.len() as u64 > limit {
        return Err(UpstreamError::new("too large", 413));
    }
    if ended {
        return Ok(all);
    }
    loop {
        match res.chunk().await {
            Ok(Some(chunk)) => {
                if (all.len() + chunk.len()) as u64 > limit {
                    return Err(UpstreamError::new("too large", 413));
                }
                all.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(all),
            Err(_) => return Err(UpstreamError::new("upstream error", 502)),
        }
    }
}

struct StreamState {
    res: Option<reqwest::Response>,
    first: Option<Bytes>,
    ended: bool,
    sign: Arc<Sign>,
    ip: String,
    cap: u64,
    sent: u64,
    deadline: Instant,
    _permit: GatePermit,
}

type Chunk = Result<Bytes, std::io::Error>;

async fn next_chunk(mut st: StreamState) -> Option<(Chunk, StreamState)> {
    if let Some(first) = st.first.take() {
        return Some((Ok(first), st));
    }
    if st.ended {
        return None;
    }
    let mut res = st.res.take()?;
    let left = st.deadline.saturating_duration_since(Instant::now());
    let next = tokio::time::timeout(left, res.chunk()).await;
    let fail = |st: StreamState, msg: &str| -> Option<(Chunk, StreamState)> {
        Some((
            Err(std::io::Error::other(msg.to_string())),
            StreamState { ended: true, ..st },
        ))
    };
    match next {
        Ok(Ok(Some(chunk))) => {
            st.sent += chunk.len() as u64;
            if st.sent > st.cap || st.sign.count_bytes(&st.ip, chunk.len() as u64) {
                return fail(st, "limit");
            }
            st.res = Some(res);
            Some((Ok(chunk), st))
        }
        Ok(Ok(None)) => None,
        Ok(Err(_)) => fail(st, "upstream error"),
        Err(_) => fail(st, "deadline"),
    }
}

// ---------------------------------------------------------------- 入口の確かめ

fn header_str<'a>(ctx: &'a Ctx, name: &str) -> &'a str {
    ctx.headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

/// 上限を数える単位 (`limit_key`)。`X-Toro-Client-Ip` → `cf-connecting-ip` → 不明
pub fn client_ip(ctx: &Ctx) -> String {
    for name in ["x-toro-client-ip", "cf-connecting-ip"] {
        let v = header_str(ctx, name).split(',').next().unwrap_or("").trim();
        if !v.is_empty() {
            return limit_key(v);
        }
    }
    "unknown".to_string()
}

/// 地図のページから来たか。ほかのサイトのページから (画像や動画として貼られて) 来たものは断る。
/// `Sec-Fetch-Site: same-origin` と `none` はそのまま。`same-site` (画面 → API) は、`Origin` が許可一覧にあるときだけ。
/// ブラウザーでないものは避けられるので、貼り付けの抑止にとどまる
pub fn from_map_page(ctx: &Ctx) -> bool {
    let site = header_str(ctx, "sec-fetch-site");
    let origin = header_str(ctx, "origin");
    if site.is_empty() && origin.is_empty() {
        return false;
    }
    match site {
        "" | "same-origin" | "none" => {}
        "same-site" => {
            if !origin_allowed(origin) {
                return false;
            }
        }
        _ => return false,
    }
    if !origin.is_empty() && origin != "null" && !origin_allowed(origin) {
        return false;
    }
    true
}

pub struct QueryUrls {
    pub url: String,
    pub parents: Vec<String>,
}

/// `u` と `p` の URL (`check_url` で確かめたもの)。だめなら None
pub fn query_urls(ctx: &Ctx) -> Option<QueryUrls> {
    let url = check_url(ctx.q("u")?)?;
    let raw: Vec<&str> = ctx
        .query
        .iter()
        .filter(|(k, _)| k == "p")
        .map(|(_, v)| v.as_str())
        .collect();
    if raw.len() > 3 {
        return None;
    }
    let parents = raw.into_iter().map(check_url).collect::<Option<Vec<_>>>()?;
    Some(QueryUrls { url, parents })
}

// ---------------------------------------------------------------- 応答

/// 中継・転送の答えに付ける頭書き (返したものを、このサイトのページとして開かせない)
fn safe_headers(res: &mut Response) {
    let headers = res.headers_mut();
    for (name, value) in [
        ("x-content-type-options", "nosniff"),
        ("content-security-policy", "default-src 'none'; sandbox"),
        ("cross-origin-resource-policy", "same-site"),
        ("referrer-policy", "no-referrer"),
        ("x-frame-options", "DENY"),
        ("cdn-cache-control", "no-store"),
    ] {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
}

fn deny(status: u16, message: &str) -> Response {
    let mut res = Response::new(Body::from(message.to_string()));
    *res.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    let headers = res.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

fn media_headers(
    headers: &mut axum::http::HeaderMap,
    ty: &str,
    upstream_type: &str,
    url: &str,
    kind: &str,
    cache: &str,
    accept_ranges: &str,
) {
    let mut put = |name: &'static str, value: &str| {
        if let Ok(v) = HeaderValue::from_str(value) {
            headers.insert(HeaderName::from_static(name), v);
        }
    };
    put("content-type", ty);
    put("x-marumasign-type", upstream_type);
    put("x-marumasign-url", url);
    put("x-marumasign-kind", kind);
    put("cache-control", cache);
    put("accept-ranges", accept_ranges);
}

/// 返す (全部読んだもの。Range にも対応)
fn send_body(cached: &CachedBody, range: Option<Range>, live: bool) -> Response {
    let mut res = Response::new(Body::empty());
    media_headers(
        res.headers_mut(),
        cached.ty,
        &cached.upstream_type,
        &cached.url,
        cached.kind,
        if live {
            "no-store"
        } else {
            "private, max-age=300"
        },
        "bytes",
    );
    let len = cached.body.len() as u64;
    let mut out = cached.body.clone();
    if let Some(r) = range {
        if r.start >= len {
            let mut res = deny(416, "range not satisfiable");
            if let Ok(v) = HeaderValue::from_str(&format!("bytes */{len}")) {
                res.headers_mut().insert(header::CONTENT_RANGE, v);
            }
            return res;
        }
        let end = r.end.unwrap_or(len - 1).min(len - 1);
        out = cached.body.slice(r.start as usize..=end as usize);
        *res.status_mut() = StatusCode::PARTIAL_CONTENT;
        if let Ok(v) = HeaderValue::from_str(&format!("bytes {}-{end}/{len}", r.start)) {
            res.headers_mut().insert(header::CONTENT_RANGE, v);
        }
    }
    res.headers_mut()
        .insert(header::CONTENT_LENGTH, (out.len() as u64).into());
    *res.body_mut() = Body::from(out);
    res
}

/// `rest` は `/map3d/v1/marumasign/` より後ろ (`resolve`、`proxy`、`<地図>.json`)
pub async fn handle(sign: &Arc<Sign>, ctx: &Ctx, rest: &str) -> Response {
    if !sign.cfg.enabled {
        return error(404, "Not Found");
    }
    match rest {
        "resolve" => {
            let mut res = sign.resolve(ctx).await;
            safe_headers(&mut res);
            res
        }
        "proxy" => {
            let mut res = sign.proxy(ctx).await;
            safe_headers(&mut res);
            res.headers_mut().insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_static("attachment"),
            );
            res
        }
        _ => match rest.strip_suffix(".json") {
            Some(id) if valid_map_id(id) => sign.sign_data(ctx, id).await,
            _ => error(404, "Not Found"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_ids() {
        assert!(valid_map_id("overworld"));
        assert!(valid_map_id("a.b-c_9"));
        assert!(!valid_map_id(""));
        assert!(!valid_map_id("../x"));
        assert!(!valid_map_id("a/b"));
        assert!(!valid_map_id(&"a".repeat(65)));
        assert!(!valid_map_id("日本"));
    }

    #[test]
    fn origins() {
        for ok in [
            "https://3d-map.torosaba.net",
            "https://torosaba.net",
            "https://map.torosaba.net",
            "http://localhost:3000",
            "http://127.0.0.1:8080",
        ] {
            assert!(origin_allowed(ok), "{ok}");
        }
        for bad in [
            "https://evil.example",
            "https://torosaba.net.evil.example",
            "https://eviltorosaba.net",
            "null",
            "",
            "ftp://torosaba.net",
            "https://3d-map.torosaba.net/path",
        ] {
            assert!(!origin_allowed(bad), "{bad}");
        }
    }

    #[test]
    fn ranges() {
        assert!(matches!(parse_range(None), Ok(None)));
        let r = parse_range(Some("bytes=0-99")).unwrap().unwrap();
        assert_eq!((r.start, r.end), (0, Some(99)));
        let r = parse_range(Some("bytes=5-")).unwrap().unwrap();
        assert_eq!((r.start, r.end), (5, None));
        assert!(parse_range(Some("bytes=5-2")).is_err());
        // 対応しない形は全体
        assert!(matches!(parse_range(Some("bytes=-5")), Ok(None)));
        assert!(matches!(parse_range(Some("bytes=0-1,3-4")), Ok(None)));
        assert!(matches!(parse_range(Some("items=0-1")), Ok(None)));
        let c = parse_content_range("bytes 0-99/20584").unwrap();
        assert_eq!((c.start, c.end, c.total), (0, 99, Some(20584)));
        assert_eq!(parse_content_range("bytes 1-2/*").unwrap().total, None);
        assert!(parse_content_range("bytes x").is_none());
    }
}

#[cfg(test)]
mod service_tests;
