//! 構造物の中のプレイヤーの頭で、名前か UUID しか書かれていないもののスキンの URL を、Mojang のサーバーに問い合わせて返す
//! (`/map3d/v1/mcassets/skin`)。移管前の地図サーバーの `server/utils/skin-lookup.ts` と `lib/marumasign/limits.ts` の移植。
//!
//! 問い合わせる先は決まったもの (名前 → UUID は api.mojang.com、だめなら api.minecraftservices.com。UUID → textures は
//! sessionserver.mojang.com。どれも設定で差し替えられる) だけで、ブラウザーから受け取るのは、形を確かめた名前か UUID だけ
//! (ホストやパスには、ほかのものを入れない)。返すのは textures.minecraft.net のスキンの URL と、細い腕のモデルかどうかだけ。
//! スキンの画像はブラウザーが textures.minecraft.net から読む。転送 (リダイレクト) は追わない。

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use serde_json::Value;
use tokio::sync::Semaphore;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkinLookup {
    /// `https://textures.minecraft.net/texture/<hash>`。分からなければ None (ブラウザーは Steve にする)
    pub url: Option<String>,
    pub slim: bool,
}

impl SkinLookup {
    fn none() -> Self {
        Self {
            url: None,
            slim: false,
        }
    }
}

/// 分かったものを覚えておく時間と、分からなかったもの (失敗も) を覚えておく時間
pub const POSITIVE_SECS: u64 = 60 * 60;
pub const NEGATIVE_SECS: u64 = 10 * 60;
const CACHE_SIZE: usize = 4096;
/// Mojang のサーバーに同時に問い合わせる数と、待たせる数
const MAX_UPSTREAM: usize = 4;
const MAX_WAITING: usize = 64;
const TIMEOUT: Duration = Duration::from_secs(5);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
/// 1 つの IP から、10 分あたりに受け付ける数 (覚えているものを返すだけのもの) と、そのうち Mojang に問い合わせる数
const WINDOW: Duration = Duration::from_secs(10 * 60);
const IP_REQUESTS: u32 = 600;
const IP_LOOKUPS: u32 = 60;
/// 数える鍵の数の上限 (ばらばらの鍵でメモリを増やさせない。超えたら古いものから忘れる)
const MAX_KEYS: usize = 50_000;
/// 時間の過ぎたものをまとめて消す間隔 (鍵が増え続けても、毎回全部を見ない)
const SWEEP: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Query {
    Name(String),
    /// 32 桁の小文字
    Uuid(String),
}

pub enum Resolved {
    Found(SkinLookup),
    /// その IP からの数が多すぎる
    Rate,
    /// 問い合わせが混んでいる
    Busy,
}

/// Mojang の問い合わせ先 (末尾の / なし)
#[derive(Clone, Debug)]
pub struct Upstreams {
    pub api: String,
    pub services: String,
    pub session: String,
}

/// 名前の形 (`^[A-Za-z0-9_]{1,16}$`)
pub fn valid_name(name: &str) -> bool {
    (1..=16).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn is_hex32(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// UUID (区切りありでもなしでも) を 32 桁の小文字にする。形が違えば None
pub fn normalize_uuid(value: &str) -> Option<String> {
    let v = value.trim().to_lowercase();
    let dashed = v.len() == 36
        && v.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                matches!(b, b'0'..=b'9' | b'a'..=b'f')
            }
        });
    let plain = if dashed { v.replace('-', "") } else { v };
    is_hex32(&plain).then_some(plain)
}

/// textures の値 (base64 の JSON) から、スキンの URL とモデル
pub fn textures_to_skin(textures: &str) -> SkinLookup {
    let decoded = STANDARD_NO_PAD.decode(textures.trim_end_matches('='));
    let Ok(bytes) = decoded else {
        return SkinLookup::none();
    };
    let Ok(json) = serde_json::from_slice::<Value>(&bytes) else {
        return SkinLookup::none();
    };
    let skin = json.get("textures").and_then(|t| t.get("SKIN"));
    let hash = skin
        .and_then(|s| s.get("url"))
        .and_then(Value::as_str)
        .and_then(|url| {
            let rest = url
                .strip_prefix("https://")
                .or_else(|| url.strip_prefix("http://"))?;
            let hash = rest.strip_prefix("textures.minecraft.net/texture/")?;
            ((1..=128).contains(&hash.len())
                && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
            .then_some(hash)
        });
    match hash {
        Some(hash) => SkinLookup {
            url: Some(format!("https://textures.minecraft.net/texture/{hash}")),
            slim: skin
                .and_then(|s| s.get("metadata"))
                .and_then(|m| m.get("model"))
                .and_then(Value::as_str)
                == Some("slim"),
        },
        None => SkinLookup::none(),
    }
}

/// 上限を数える単位。IPv6 は /64 にまとめる (1 つの回線に /64 が丸ごと割り当てられるので、/128 で数えても意味がない)。
/// IPv4 を写した IPv6 は IPv4 と同じ。IP アドレスでないものはそのまま (64 文字まで)
pub fn limit_key(ip: &str) -> String {
    let t = ip.trim();
    let parsed = t
        .parse::<IpAddr>()
        .ok()
        .or_else(|| t.parse::<SocketAddr>().ok().map(|a| a.ip()))
        .or_else(|| {
            t.strip_prefix('[')
                .and_then(|r| r.strip_suffix(']'))
                .and_then(|r| r.parse().ok())
        });
    match parsed {
        Some(IpAddr::V4(v4)) => v4.to_string(),
        Some(IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let o = v6.octets();
                let mut s: String = o[..8].iter().map(|b| format!("{b:02x}")).collect();
                s.push_str("/64");
                s
            }
        },
        None => t.chars().take(64).collect(),
    }
}

/// 決まった時間ごとの数の上限 (時間ごとに数え直す)
struct WindowLimit {
    window: Duration,
    limit: u32,
    max_keys: usize,
    counts: HashMap<String, (Instant, u32)>,
    /// 鍵を入れた順 (古いものから忘れる。数え直して作り直した古い印は、見たときに捨てる)
    order: VecDeque<(String, Instant)>,
    swept_at: Option<Instant>,
}

impl WindowLimit {
    fn new(window: Duration, limit: u32, max_keys: usize) -> Self {
        Self {
            window,
            limit,
            max_keys,
            counts: HashMap::new(),
            order: VecDeque::new(),
            swept_at: None,
        }
    }

    /// 1 足して、上限を超えたら true
    fn hit(&mut self, key: &str, now: Instant) -> bool {
        let fresh = match self.counts.get(key) {
            Some((start, _)) => now.duration_since(*start) > self.window,
            None => true,
        };
        if fresh {
            self.counts.insert(key.to_string(), (now, 0));
            self.order.push_back((key.to_string(), now));
            if self.counts.len() > self.max_keys {
                if self.swept_at.is_none_or(|t| now.duration_since(t) > SWEEP) {
                    self.swept_at = Some(now);
                    let window = self.window;
                    self.counts
                        .retain(|_, (start, _)| now.duration_since(*start) <= window);
                }
                while self.counts.len() > self.max_keys {
                    let Some((k, start)) = self.order.pop_front() else {
                        break;
                    };
                    if self.counts.get(&k).is_some_and(|(s, _)| *s == start) {
                        self.counts.remove(&k);
                    }
                }
            }
            // 古い印がたまりすぎたら、生きているものだけにする
            if self.order.len() > self.max_keys.saturating_mul(2) + 16 {
                let counts = &self.counts;
                self.order
                    .retain(|(k, start)| counts.get(k).is_some_and(|(s, _)| s == start));
            }
        }
        let entry = self.counts.get_mut(key).expect("just inserted");
        entry.1 += 1;
        entry.1 > self.limit
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.counts.len()
    }
}

/// 最近使ったものを残す、数に上限のある置き場
struct Cache {
    map: HashMap<String, (Instant, SkinLookup, u64)>,
    order: VecDeque<(String, u64)>,
    seq: u64,
}

impl Cache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            seq: 0,
        }
    }

    fn touch(&mut self, key: &str) -> u64 {
        self.seq += 1;
        self.order.push_back((key.to_string(), self.seq));
        if self.order.len() > CACHE_SIZE * 4 {
            let map = &self.map;
            self.order
                .retain(|(k, s)| map.get(k).is_some_and(|(_, _, seq)| seq == s));
        }
        self.seq
    }

    fn get(&mut self, key: &str, now: Instant) -> Option<SkinLookup> {
        let (until, value) = {
            let (until, value, _) = self.map.get(key)?;
            (*until, value.clone())
        };
        if until <= now {
            return None;
        }
        let seq = self.touch(key);
        if let Some(entry) = self.map.get_mut(key) {
            entry.2 = seq;
        }
        Some(value)
    }

    fn remember(&mut self, key: &str, value: SkinLookup, now: Instant) {
        let ttl = if value.url.is_some() {
            POSITIVE_SECS
        } else {
            NEGATIVE_SECS
        };
        let seq = self.touch(key);
        self.map.insert(
            key.to_string(),
            (now + Duration::from_secs(ttl), value, seq),
        );
        while self.map.len() > CACHE_SIZE {
            let Some((k, s)) = self.order.pop_front() else {
                break;
            };
            if self.map.get(&k).is_some_and(|(_, _, seq)| *seq == s) {
                self.map.remove(&k);
            }
        }
    }
}

type Inflight = Shared<BoxFuture<'static, SkinLookup>>;

struct State {
    cache: Cache,
    inflight: HashMap<String, Inflight>,
    requests: WindowLimit,
    lookups: WindowLimit,
}

struct Inner {
    http: reqwest::Client,
    upstreams: Upstreams,
    state: Mutex<State>,
    permits: Semaphore,
    waiting: AtomicUsize,
    calls: AtomicU64,
}

pub struct SkinResolver {
    inner: Arc<Inner>,
}

impl SkinResolver {
    /// `http` は、転送を追わず、時間切れのあるもの
    pub fn new(http: reqwest::Client, upstreams: Upstreams) -> Self {
        Self {
            inner: Arc::new(Inner {
                http,
                upstreams,
                state: Mutex::new(State {
                    cache: Cache::new(),
                    inflight: HashMap::new(),
                    requests: WindowLimit::new(WINDOW, IP_REQUESTS, MAX_KEYS),
                    lookups: WindowLimit::new(WINDOW, IP_LOOKUPS, MAX_KEYS),
                }),
                permits: Semaphore::new(MAX_UPSTREAM),
                waiting: AtomicUsize::new(0),
                calls: AtomicU64::new(0),
            }),
        }
    }

    /// Mojang のサーバーに問い合わせた数 (試験用)
    pub fn upstream_calls(&self) -> u64 {
        self.inner.calls.load(Ordering::SeqCst)
    }

    /// 名前か UUID のスキン (呼ぶ側で形を確かめたもの)。`ip` は `limit_key` を通したもの
    pub async fn resolve(&self, query: Query, ip: &str) -> Resolved {
        let key = match &query {
            Query::Name(n) => format!("n:{}", n.to_lowercase()),
            Query::Uuid(u) => format!("u:{u}"),
        };
        let now = Instant::now();
        let inflight = {
            let mut st = self.inner.state.lock().expect("skin state");
            if st.requests.hit(ip, now) {
                return Resolved::Rate;
            }
            if let Some(hit) = st.cache.get(&key, now) {
                return Resolved::Found(hit);
            }
            if let Some(f) = st.inflight.get(&key) {
                f.clone()
            } else {
                if st.lookups.hit(ip, now) {
                    return Resolved::Rate;
                }
                if self.inner.permits.available_permits() == 0
                    && self.inner.waiting.load(Ordering::SeqCst) >= MAX_WAITING
                {
                    return Resolved::Busy;
                }
                // 問い合わせは別の仕事で行う (呼んだ人が切れても、結果は覚える)。ここの錠を持っているあいだは
                // その仕事が終わりの処理 (inflight を消す) に入れないので、先に入れた印が残ることはない
                let inner = self.inner.clone();
                let task_key = key.clone();
                let handle = tokio::spawn(async move {
                    let value = inner.lookup(&query).await.unwrap_or_else(|_| {
                        // 失敗も 10 分は覚えておく (Mojang のサーバーに何度も問い合わせない)
                        SkinLookup::none()
                    });
                    let mut st = inner.state.lock().expect("skin state");
                    st.cache.remember(&task_key, value.clone(), Instant::now());
                    st.inflight.remove(&task_key);
                    value
                });
                let shared: Inflight =
                    async move { handle.await.unwrap_or_else(|_| SkinLookup::none()) }
                        .boxed()
                        .shared();
                st.inflight.insert(key, shared.clone());
                shared
            }
        };
        Resolved::Found(inflight.await)
    }
}

impl Inner {
    async fn lookup(&self, query: &Query) -> Result<SkinLookup, ()> {
        let uuid = match query {
            Query::Uuid(u) => u.clone(),
            Query::Name(name) => {
                if !valid_name(name) {
                    return Ok(SkinLookup::none());
                }
                let up = &self.upstreams;
                let profile = match self
                    .get(&format!("{}/users/profiles/minecraft/{name}", up.api))
                    .await
                {
                    Ok(p) => p,
                    Err(()) => {
                        self.get(&format!(
                            "{}/minecraft/profile/lookup/name/{name}",
                            up.services
                        ))
                        .await?
                    }
                };
                let id = profile
                    .as_ref()
                    .and_then(|p| p.get("id"))
                    .and_then(Value::as_str)
                    .and_then(normalize_uuid);
                match id {
                    Some(id) => id,
                    None => return Ok(SkinLookup::none()),
                }
            }
        };
        if !is_hex32(&uuid) {
            return Ok(SkinLookup::none());
        }
        let session = self
            .get(&format!(
                "{}/session/minecraft/profile/{uuid}",
                self.upstreams.session
            ))
            .await?;
        let properties = session
            .as_ref()
            .and_then(|s| s.get("properties"))
            .and_then(Value::as_array);
        for p in properties.into_iter().flatten() {
            if p.get("name").and_then(Value::as_str) == Some("textures")
                && let Some(value) = p.get("value").and_then(Value::as_str)
            {
                return Ok(textures_to_skin(value));
            }
        }
        Ok(SkinLookup::none())
    }

    /// 決まった URL を GET する (Cookie なし、転送は追わない、時間切れあり)。見つからなければ Ok(None)
    async fn get(&self, url: &str) -> Result<Option<Value>, ()> {
        let _permit = match self.permits.try_acquire() {
            Ok(p) => p,
            Err(_) => {
                if self.waiting.load(Ordering::SeqCst) >= MAX_WAITING {
                    return Err(());
                }
                self.waiting.fetch_add(1, Ordering::SeqCst);
                let p = self.permits.acquire().await;
                self.waiting.fetch_sub(1, Ordering::SeqCst);
                p.map_err(|_| ())?
            }
        };
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut res = self
            .http
            .get(url)
            .header("accept", "application/json")
            .timeout(TIMEOUT)
            .send()
            .await
            .map_err(|_| ())?;
        let status = res.status();
        if status.as_u16() == 204 || status.as_u16() == 404 {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(());
        }
        if res
            .content_length()
            .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
        {
            return Err(());
        }
        let mut body = Vec::new();
        while let Some(chunk) = res.chunk().await.map_err(|_| ())? {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_RESPONSE_BYTES {
                return Err(());
            }
        }
        if body.is_empty() {
            return Ok(None);
        }
        serde_json::from_slice(&body).map(Some).map_err(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_and_name_shapes() {
        assert_eq!(
            normalize_uuid("069A79F4-44E9-4726-A5BE-FCA90E38AAF5").as_deref(),
            Some("069a79f444e94726a5befca90e38aaf5")
        );
        assert_eq!(normalize_uuid("../x"), None);
        assert_eq!(normalize_uuid("zzz"), None);
        assert!(valid_name("Notch_1"));
        assert!(!valid_name("a b"));
        assert!(!valid_name(""));
        assert!(!valid_name("12345678901234567"));
    }

    #[test]
    fn limit_keys() {
        assert_eq!(limit_key("1.2.3.4"), "1.2.3.4");
        assert_eq!(limit_key("::ffff:1.2.3.4"), "1.2.3.4");
        assert_eq!(
            limit_key("2001:db8:1:2:aaaa:bbbb:cccc:dddd"),
            "20010db800010002/64"
        );
        assert_eq!(
            limit_key("2001:db8:1:2:1::1"),
            limit_key("2001:db8:1:2:2::2")
        );
        assert_eq!(limit_key("unknown"), "unknown");
    }

    #[test]
    fn window_limit_counts_and_bounds_keys() {
        let mut w = WindowLimit::new(Duration::from_secs(60), 2, 3);
        let t = Instant::now();
        assert!(!w.hit("a", t));
        assert!(!w.hit("a", t));
        assert!(w.hit("a", t));
        // 時間が過ぎたら数え直す
        assert!(!w.hit("a", t + Duration::from_secs(61)));
        for i in 0..20 {
            w.hit(&format!("k{i}"), t);
            assert!(w.len() <= 3);
        }
    }

    #[test]
    fn textures_only_from_textures_host() {
        let b64 = |v: Value| STANDARD_NO_PAD.encode(v.to_string());
        let ok = textures_to_skin(&b64(serde_json::json!({"textures":{"SKIN":{
            "url":"http://textures.minecraft.net/texture/ab12","metadata":{"model":"slim"}}}})));
        assert_eq!(
            ok,
            SkinLookup {
                url: Some("https://textures.minecraft.net/texture/ab12".into()),
                slim: true
            }
        );
        let evil = textures_to_skin(&b64(serde_json::json!({"textures":{"SKIN":{
            "url":"https://evil.example/texture/ab12"}}})));
        assert_eq!(evil, SkinLookup::none());
    }
}
