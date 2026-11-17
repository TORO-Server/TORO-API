//! `/map3d/v1/mcassets/...`: 構造物の看板を描くための Minecraft のリソース。移管前の toro-webmap の `/mcassets/*` の置き換え。
//!
//! - `index.json`: いまのバージョン (hash)。jar が無い、または作っている途中なら 503
//! - `<hash>/assets.json`: blockstates、models、アイテム、バイオームの色など (br、gzip、無圧縮)
//! - `<hash>/textures.bin`: テクスチャの PNG を並べたもの (位置は assets.json の textures)
//! - `<hash>/unifont/<4 桁の 16 進>.txt`: Unifont の 256 文字分 (jar のフォントにない文字。持たないページは 204)
//! - `skin?name=|uuid=`: 名前か UUID のスキンの URL (Mojang に問い合わせる)
//!
//! 作り方は `build` (jar から)、`skin` は問い合わせと上限。
//!
//! # 起動と作り直し
//! `McAssets::new` が背景の仕事 (`tokio::spawn`、重い処理は `spawn_blocking`) として、`MAP_MCASSETS_JAR` の jar からまとめを作り始める
//! (jar は 40 MB ほど、作るのは数秒。brotli の品質 11 が重い)。作り終わるまでは `index.json` は 503。
//! jar を作り直す必要があるかは、10 分に 1 度、リクエストのついでに確かめる (jar の場所・大きさ・更新時刻が変わったら作り直す)。
//! `MAP_MCASSETS_JAR` がフォルダなら、その中の `minecraft-client-<バージョン>.jar` のうち最も新しいバージョンを使う (BlueMap の `main/bluemap`)。
//! 作れなかったときは、前に作ったものを返し続け、前がなければ 30 秒おきにリクエストのついでにやり直す。
//!
//! # メモリ (`budget.rs` の外の、mcassets だけの常駐)
//! 作ったものはメモリに持つ。1 つのバージョン (hash) あたり、実際の jar (26.3) で次のとおり:
//!
//! | もの | 大きさ |
//! |---|---|
//! | assets.json (無圧縮) | 2.5 MB |
//! | assets.json (br、gzip) | 126 KB、200 KB |
//! | textures.bin | 1.18 MB |
//! | Unifont のページ | 数 MB (バージョンをまたいで 1 つを共有する) |
//!
//! 合わせて 10 MB 前後。バージョンは新旧の 2 つまで持つ (読み込み中のブラウザーのため) ので、最大でその 2 倍。
//! バージョン 1 つが `MAX_RESIDENT_BYTES` (32 MiB) を超えるまとめは、持たずに捨てる (壊れた jar や巨大なリソースパックで増やさない)。
//! 作っている最中は、jar の中身を 1 つずつ読んで、JSON の断片を足していく (jar 全体も、JSON の木もメモリに載せない)。
//! 返すときは本文を 1 度コピーする (2.5 MB。304 のときはコピーしない)。`budget.rs` の 2 つのキャッシュの合計 (256 MiB) とは別で、
//! 合わせても現行のコンテナの上限 (512 MiB) に収まる (下の定数の確認)。
//!
//! # 環境変数
//! - `MAP_MCASSETS_JAR`: client の jar か、`minecraft-client-<バージョン>.jar` があるフォルダ。無ければこの機能を動かさない (index は 503)
//! - `MAP_MCASSETS_CACHE`: Unifont の zip を保存する、書き込める場所。無ければメモリだけ (作り直すたびに取り直す)
//! - `MAP_MCASSETS_RESOURCES_URL`: Unifont の配布元 (既定 `https://resources.download.minecraft.net`)
//! - `MAP_MCASSETS_MOJANG_API_URL`、`MAP_MCASSETS_MOJANG_SERVICES_URL`、`MAP_MCASSETS_MOJANG_SESSION_URL`:
//!   スキンの問い合わせ先 (既定 `https://api.mojang.com`、`https://api.minecraftservices.com`、`https://sessionserver.mojang.com`)

mod build;
mod jsjson;
mod skin;
mod zip;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use axum::body::Body;
use axum::http::header::{self, HeaderName, HeaderValue};
use axum::response::Response;
use serde_json::json;
use sha1::{Digest, Sha1};

use super::budget;
use super::{Body3, error, respond};
use crate::config::{optional, or_default};
use crate::http::Ctx;
use build::Unifont;
use skin::{Query, Resolved, SkinResolver, Upstreams};

const MIB: usize = 1024 * 1024;
/// バージョン 1 つが常駐してよい大きさの上限 (assets.json の 3 つの形、textures.bin、Unifont の合計)
const MAX_RESIDENT_BYTES: usize = 32 * MIB;
/// 持つバージョンの数 (いまのものと、ひとつ前)
const MAX_BUILDS: usize = 2;
// このサービスのコンテナの上限 (512 MiB) に、`budget` のキャッシュの合計と、mcassets の常駐が収まる
const _: () = assert!(budget::TOTAL_CACHE_BYTES + MAX_BUILDS * MAX_RESIDENT_BYTES <= 384 * MIB);

/// jar を作り直す必要があるかを確かめる間隔
const CHECK_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// 作れなかったとき (前のものもない) のやり直しの間隔
const RETRY_INTERVAL: Duration = Duration::from_secs(30);
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const UNIFONT_TIMEOUT: Duration = Duration::from_secs(30);

/// mcassets の設定 (環境変数 `MAP_MCASSETS_*`)
#[derive(Clone, Debug)]
pub struct McAssetsConfig {
    /// client の jar か、`minecraft-client-<バージョン>.jar` があるフォルダ。None ならこの機能を動かさない
    pub jar: Option<PathBuf>,
    /// Unifont の zip の保存先。None ならメモリだけ
    pub cache: Option<PathBuf>,
    /// Unifont の配布元 (末尾の / なし)
    pub resources_url: String,
    pub mojang_api_url: String,
    pub mojang_services_url: String,
    pub mojang_session_url: String,
    /// Unifont の zip の SHA-1 と大きさ (配布元から取ったものの確認。試験で差し替える)
    pub unifont_sha1: String,
    pub unifont_size: usize,
}

impl Default for McAssetsConfig {
    fn default() -> Self {
        Self {
            jar: None,
            cache: None,
            resources_url: "https://resources.download.minecraft.net".to_string(),
            mojang_api_url: "https://api.mojang.com".to_string(),
            mojang_services_url: "https://api.minecraftservices.com".to_string(),
            mojang_session_url: "https://sessionserver.mojang.com".to_string(),
            unifont_sha1: build::UNIFONT_SHA1.to_string(),
            unifont_size: build::UNIFONT_SIZE,
        }
    }
}

fn url_setting(name: &str, default: &str) -> Result<String, String> {
    let value = or_default(name, default).trim_end_matches('/').to_string();
    match url::Url::parse(&value) {
        Ok(u) if matches!(u.scheme(), "http" | "https") && u.host().is_some() => Ok(value),
        _ => Err(format!("{name} must be an http(s) URL")),
    }
}

impl McAssetsConfig {
    pub fn from_env() -> Result<Self, String> {
        let d = Self::default();
        Ok(Self {
            jar: optional("MAP_MCASSETS_JAR").map(PathBuf::from),
            cache: optional("MAP_MCASSETS_CACHE").map(PathBuf::from),
            resources_url: url_setting("MAP_MCASSETS_RESOURCES_URL", &d.resources_url)?,
            mojang_api_url: url_setting("MAP_MCASSETS_MOJANG_API_URL", &d.mojang_api_url)?,
            mojang_services_url: url_setting(
                "MAP_MCASSETS_MOJANG_SERVICES_URL",
                &d.mojang_services_url,
            )?,
            mojang_session_url: url_setting(
                "MAP_MCASSETS_MOJANG_SESSION_URL",
                &d.mojang_session_url,
            )?,
            unifont_sha1: d.unifont_sha1,
            unifont_size: d.unifont_size,
        })
    }
}

/// 作ったバージョンの、ログに出すもの
struct Meta {
    version: String,
    texture_count: usize,
    millis: u128,
}

/// 返せる 1 つのバージョン
struct Served {
    hash: String,
    index_json: Vec<u8>,
    json: Vec<u8>,
    json_gzip: Vec<u8>,
    json_br: Vec<u8>,
    textures: Vec<u8>,
    unifont: Arc<Unifont>,
}

impl Served {
    /// 常駐するおおよその大きさ (Unifont はバージョンをまたいで共有するが、ここでは 1 つぶんを数える)
    fn resident_bytes(&self) -> usize {
        self.index_json.len()
            + self.json.len()
            + self.json_gzip.len()
            + self.json_br.len()
            + self.textures.len()
            + self.unifont.bytes()
    }
}

#[derive(Default)]
struct State {
    /// 古い順 (最後がいまのもの)
    builds: Vec<Arc<Served>>,
    building: bool,
    /// 最後に作ろうとした時刻 (作れなかったときのやり直し)
    last_attempt: Option<Instant>,
    /// 最後に jar を確かめた時刻
    last_check: Option<Instant>,
    /// いまのバージョンを作ったときの jar の場所・大きさ・更新時刻
    stamp: String,
    last_error: String,
    unifont: Option<Arc<Unifont>>,
}

pub struct McAssets {
    cfg: McAssetsConfig,
    state: Mutex<State>,
    skins: SkinResolver,
    /// Unifont の配布元用 (転送を追わない)
    http: reqwest::Client,
}

fn gzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(data)?;
    enc.finish()
}

fn brotli(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        // 品質 11、窓 22 (Node の `brotliCompress` で品質だけ 11 にしたものと同じ)
        let mut enc = ::brotli::CompressorWriter::new(&mut out, 64 * 1024, 11, 22);
        enc.write_all(data)?;
        enc.flush()?;
    }
    Ok(out)
}

/// jar の場所・大きさ・更新時刻 (変わったかを見る)
fn stamp_of(jar: &Path) -> String {
    match std::fs::metadata(jar) {
        Ok(m) => {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_millis());
            format!("{}:{}:{}", jar.display(), m.len(), mtime)
        }
        Err(_) => format!("{}:missing", jar.display()),
    }
}

impl McAssets {
    /// jar からの生成は、ここで背景の仕事として始める (tokio の中でなければ、最初のリクエストのときに始める)
    pub fn new(cfg: &McAssetsConfig, http: reqwest::Client) -> Arc<Self> {
        // Mojang への問い合わせは、転送を追わない (決まった先だけに行く)。ほかの機能と共有の `http` は転送を追うので、別に作る
        let strict = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .user_agent("TORO-API (mcassets)")
            .build()
            .unwrap_or(http);
        let skins = SkinResolver::new(
            strict.clone(),
            Upstreams {
                api: cfg.mojang_api_url.trim_end_matches('/').to_string(),
                services: cfg.mojang_services_url.trim_end_matches('/').to_string(),
                session: cfg.mojang_session_url.trim_end_matches('/').to_string(),
            },
        );
        let mc = Arc::new(Self {
            cfg: cfg.clone(),
            state: Mutex::new(State::default()),
            skins,
            http: strict,
        });
        mc.kick();
        mc
    }

    /// 作る必要があれば、背景の仕事を始める (作っている最中なら何もしない)
    fn kick(self: &Arc<Self>) {
        if self.cfg.jar.is_none() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let now = Instant::now();
        {
            let mut st = self.state.lock().expect("mcassets state");
            if st.building {
                return;
            }
            let due = if st.builds.is_empty() {
                st.last_attempt
                    .is_none_or(|t| now.duration_since(t) >= RETRY_INTERVAL)
            } else {
                st.last_check
                    .is_none_or(|t| now.duration_since(t) >= CHECK_INTERVAL)
            };
            if !due {
                return;
            }
            st.building = true;
            st.last_attempt = Some(now);
            st.last_check = Some(now);
        }
        let this = self.clone();
        runtime.spawn(async move {
            let result = this.run().await;
            let mut st = this.state.lock().expect("mcassets state");
            st.building = false;
            match result {
                Ok(()) => st.last_error.clear(),
                Err(e) => {
                    eprintln!("mcassets: cannot build: {e}");
                    st.last_error = e;
                }
            }
        });
    }

    /// 作り直す必要があれば作って、持つ
    async fn run(&self) -> Result<(), String> {
        let configured = self.cfg.jar.clone().ok_or("MAP_MCASSETS_JAR is not set")?;
        let (jar, stamp) = tokio::task::spawn_blocking(move || {
            build::resolve_client_jar(&configured).map(|jar| {
                let stamp = stamp_of(&jar);
                (jar, stamp)
            })
        })
        .await
        .map_err(|e| e.to_string())?
        .ok_or("no minecraft-client-*.jar at MAP_MCASSETS_JAR")?;
        {
            let st = self.state.lock().expect("mcassets state");
            if !st.builds.is_empty() && st.stamp == stamp {
                return Ok(());
            }
        }
        let unifont = self.unifont().await;
        let (served, meta) =
            tokio::task::spawn_blocking(move || -> Result<(Served, Meta), String> {
                let built = build::build(&jar, unifont).map_err(|e| e.to_string())?;
                let json_gzip = gzip(&built.json).map_err(|e| e.to_string())?;
                let json_br = brotli(&built.json).map_err(|e| e.to_string())?;
                let index_json = json!({
                    "format": build::FORMAT,
                    "hash": built.hash,
                    "version": built.version,
                    "unifont": !built.unifont.is_empty(),
                })
                .to_string()
                .into_bytes();
                let meta = Meta {
                    version: built.version,
                    texture_count: built.texture_count,
                    millis: built.millis,
                };
                let served = Served {
                    hash: built.hash,
                    index_json,
                    json: built.json,
                    json_gzip,
                    json_br,
                    textures: built.textures,
                    unifont: built.unifont,
                };
                Ok((served, meta))
            })
            .await
            .map_err(|e| e.to_string())??;
        let resident = served.resident_bytes();
        if resident > MAX_RESIDENT_BYTES {
            return Err(format!(
                "assets are too large to keep ({resident} bytes > {MAX_RESIDENT_BYTES})"
            ));
        }
        eprintln!(
            "mcassets: {} {} ({} textures, {} ms, {} bytes resident)",
            meta.version, served.hash, meta.texture_count, meta.millis, resident
        );
        let mut st = self.state.lock().expect("mcassets state");
        st.builds.retain(|b| b.hash != served.hash);
        st.builds.push(Arc::new(served));
        while st.builds.len() > MAX_BUILDS {
            st.builds.remove(0);
        }
        st.stamp = stamp;
        Ok(())
    }

    /// Unifont (jar にない文字)。1 度読めたら覚える。読めなければ None (その文字は描かない)
    async fn unifont(&self) -> Option<Arc<Unifont>> {
        if let Some(u) = self.state.lock().expect("mcassets state").unifont.clone() {
            return Some(u);
        }
        let bytes = self.unifont_zip().await?;
        match tokio::task::spawn_blocking(move || build::parse_unifont(&bytes)).await {
            Ok(Ok(parsed)) => {
                let parsed = Arc::new(parsed);
                self.state.lock().expect("mcassets state").unifont = Some(parsed.clone());
                Some(parsed)
            }
            Ok(Err(e)) => {
                eprintln!("mcassets: cannot read unifont: {e}");
                None
            }
            Err(e) => {
                eprintln!("mcassets: cannot read unifont: {e}");
                None
            }
        }
    }

    fn verify_unifont(&self, bytes: &[u8]) -> bool {
        let digest: String = Sha1::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        bytes.len() == self.cfg.unifont_size && digest == self.cfg.unifont_sha1
    }

    /// Unifont の zip。保存先にあればそれ、なければ配布元 (SHA-1 で確かめる) から 1 度だけ読んで、保存先に置く
    async fn unifont_zip(&self) -> Option<Vec<u8>> {
        let sha1 = &self.cfg.unifont_sha1;
        let file = self
            .cfg
            .cache
            .as_ref()
            .map(|dir| dir.join(format!("unifont-{sha1}.zip")));
        if let Some(file) = &file
            && let Ok(bytes) = tokio::fs::read(file).await
            && self.verify_unifont(&bytes)
        {
            return Some(bytes);
        }
        match self.download_unifont().await {
            Ok(bytes) => {
                if let Some(file) = &file
                    && let Err(e) = save_atomic(file, &bytes).await
                {
                    eprintln!("mcassets: cannot cache unifont: {e}");
                }
                Some(bytes)
            }
            Err(e) => {
                eprintln!(
                    "mcassets: cannot download unifont (text outside the bitmap fonts will not be drawn): {e}"
                );
                None
            }
        }
    }

    async fn download_unifont(&self) -> Result<Vec<u8>, String> {
        let sha1 = &self.cfg.unifont_sha1;
        let url = format!(
            "{}/{}/{sha1}",
            self.cfg.resources_url,
            sha1.get(..2).unwrap_or("")
        );
        let mut res = self
            .http
            .get(url)
            .timeout(UNIFONT_TIMEOUT)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !res.status().is_success() {
            return Err(format!("HTTP {}", res.status().as_u16()));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = res.chunk().await.map_err(|e| e.to_string())? {
            bytes.extend_from_slice(&chunk);
            if bytes.len() > self.cfg.unifont_size {
                return Err("too large".to_string());
            }
        }
        if !self.verify_unifont(&bytes) {
            return Err("checksum mismatch".to_string());
        }
        Ok(bytes)
    }

    /// Mojang のサーバーに問い合わせた数 (試験で、覚えているものには問い合わせないことを確かめる)
    pub fn skin_upstream_calls(&self) -> u64 {
        self.skins.upstream_calls()
    }

    fn by_hash(&self, hash: &str) -> Option<Arc<Served>> {
        let st = self.state.lock().expect("mcassets state");
        st.builds.iter().find(|b| b.hash == hash).cloned()
    }

    fn current(&self) -> Option<Arc<Served>> {
        self.state
            .lock()
            .expect("mcassets state")
            .builds
            .last()
            .cloned()
    }
}

/// 一時ファイルに書いてから置き換える (読む側が書きかけを見ない)
async fn save_atomic(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let mut tmp = file.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, file).await
}

/// `Accept-Encoding` に、その名前が 1 語として入っているか (正規表現の `\bbr\b` と同じ)
fn has_token(accept: &str, name: &str) -> bool {
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let bytes = accept.as_bytes();
    accept.match_indices(name).any(|(i, _)| {
        let before = i == 0 || !is_word(bytes[i - 1]);
        let after = bytes.get(i + name.len()).is_none_or(|b| !is_word(*b));
        before && after
    })
}

/// `rest` は `/map3d/v1/mcassets/` より後ろ (`index.json`、`skin`、`<hash>/...`)
pub async fn handle(mc: &Arc<McAssets>, ctx: &Ctx, rest: &str) -> Response {
    if rest == "skin" {
        return skin_route(mc, ctx).await;
    }
    mc.kick();
    if rest == "index.json" {
        let Some(a) = mc.current() else {
            let mut res = error(503, "Minecraft assets are not available");
            res.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            return res;
        };
        return respond(
            ctx,
            Body3 {
                data: a.index_json.clone(),
                content_type: "application/json",
                // jar を差し替えたときに、5 分ほどで新しいバージョンに移る
                cache: "public, max-age=300",
                etag: None,
                encoding: None,
            },
        );
    }
    let Some((hash, file)) = rest.split_once('/') else {
        return error(404, "Not Found");
    };
    if hash.len() != 20 || !hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return error(404, "Not Found");
    }
    let Some(a) = mc.by_hash(hash) else {
        return error(404, "Not Found");
    };
    // 304 のときは、本文をコピーしない
    let take = |etag: &str, data: &Vec<u8>| {
        if ctx.header(header::IF_NONE_MATCH) == etag {
            Vec::new()
        } else {
            data.clone()
        }
    };
    if file == "assets.json" {
        let accept = ctx.header(header::ACCEPT_ENCODING);
        let (data, encoding, label) = if has_token(accept, "br") {
            (&a.json_br, Some("br"), "br")
        } else if has_token(accept, "gzip") {
            (&a.json_gzip, Some("gzip"), "gzip")
        } else {
            (&a.json, None, "id")
        };
        let etag = format!("\"{hash}-{label}\"");
        return respond(
            ctx,
            Body3 {
                data: take(&etag, data),
                content_type: "application/json",
                cache: IMMUTABLE,
                etag: Some(etag),
                encoding,
            },
        );
    }
    if file == "textures.bin" {
        let etag = format!("\"{hash}-t\"");
        return respond(
            ctx,
            Body3 {
                data: take(&etag, &a.textures),
                content_type: "application/octet-stream",
                cache: IMMUTABLE,
                etag: Some(etag),
                encoding: None,
            },
        );
    }
    if let Some(page_name) = file
        .strip_prefix("unifont/")
        .and_then(|f| f.strip_suffix(".txt"))
        .filter(|p| p.len() == 4 && p.bytes().all(|b| matches!(b, b'0'..=b'9' | b'A'..=b'F')))
    {
        let page = u32::from_str_radix(page_name, 16).unwrap_or(u32::MAX);
        let Some(text) = a.unifont.page(page) else {
            let mut res = Response::new(Body::empty());
            *res.status_mut() = axum::http::StatusCode::NO_CONTENT;
            res.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static(IMMUTABLE));
            return res;
        };
        let etag = format!("\"{hash}-u{page_name}\"");
        let data = if ctx.header(header::IF_NONE_MATCH) == etag {
            Vec::new()
        } else {
            text.as_bytes().to_vec()
        };
        return respond(
            ctx,
            Body3 {
                data,
                content_type: "text/plain; charset=utf-8",
                cache: IMMUTABLE,
                etag: Some(etag),
                encoding: None,
            },
        );
    }
    error(404, "Not Found")
}

/// クエリの値。同じ名前が 2 つ以上あれば、文字列でないもの (None) として扱う (移管前と同じ)
fn single<'a>(ctx: &'a Ctx, name: &str) -> Option<&'a str> {
    let mut found = ctx.query.iter().filter(|(k, _)| k == name);
    let first = found.next()?;
    found.next().is_none().then_some(first.1.as_str())
}

/// 上限を数える相手の IP。toro-edge が付ける `X-Toro-Client-Ip`、Cloudflare の `cf-connecting-ip`、`X-Forwarded-For` の先頭の順。
/// どれも無ければ "unknown" (リクエストの `Ctx` には、つながってきた相手のアドレスが入っていない)
fn client_ip(ctx: &Ctx) -> String {
    for name in ["x-toro-client-ip", "cf-connecting-ip"] {
        let v = ctx.header(HeaderName::from_static(name)).trim();
        if !v.is_empty() {
            return skin::limit_key(v);
        }
    }
    let forwarded = ctx
        .header(HeaderName::from_static("x-forwarded-for"))
        .split(',')
        .next()
        .unwrap_or("")
        .trim();
    if forwarded.is_empty() {
        "unknown".to_string()
    } else {
        skin::limit_key(forwarded)
    }
}

async fn skin_route(mc: &Arc<McAssets>, ctx: &Ctx) -> Response {
    let name = single(ctx, "name");
    let uuid = single(ctx, "uuid");
    let query = match (name, uuid) {
        (None, None) | (Some(_), Some(_)) => return error(400, "name or uuid"),
        (Some(name), None) => {
            if !skin::valid_name(name) {
                return error(400, "bad name");
            }
            Query::Name(name.to_string())
        }
        (None, Some(uuid)) => match skin::normalize_uuid(uuid) {
            Some(uuid) => Query::Uuid(uuid),
            None => return error(400, "bad uuid"),
        },
    };
    match mc.skins.resolve(query, &client_ip(ctx)).await {
        Resolved::Found(found) => {
            let cache = if found.url.is_some() {
                "public, max-age=3600"
            } else {
                "public, max-age=600"
            };
            respond(
                ctx,
                Body3 {
                    data: json!({ "url": found.url, "slim": found.slim })
                        .to_string()
                        .into_bytes(),
                    content_type: "application/json",
                    cache,
                    etag: None,
                    encoding: None,
                },
            )
        }
        busy @ (Resolved::Rate | Resolved::Busy) => {
            let (status, message) = if matches!(busy, Resolved::Rate) {
                (429, "Too Many Requests")
            } else {
                (503, "Service Unavailable")
            };
            let mut res = error(status, message);
            let headers = res.headers_mut();
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            headers.insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
            res
        }
    }
}
