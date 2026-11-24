//! BlueMap 内蔵の Web サーバー からの取得。プレイヤーの位置とマーカー。
//! 内部ネットワークだけで届く。ここへは、DB に無いライブ情報だけを聞きに行く。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::OnceCell;

use reqwest::Url;

/// 本文の大きさの上限 (想定より大きいものは読まない)
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
const LIVE_CACHE_TTL: Duration = Duration::from_secs(1);

type LiveCacheMap = HashMap<(String, String), (Instant, Option<Vec<u8>>)>;
type LiveInflightMap = HashMap<(String, String), Arc<OnceCell<Option<Vec<u8>>>>>;

struct LiveInflightGuard<'a> {
    map: &'a Mutex<LiveInflightMap>,
    key: (String, String),
    cell: Arc<OnceCell<Option<Vec<u8>>>>,
}

impl Drop for LiveInflightGuard<'_> {
    fn drop(&mut self) {
        let _ = self.cell.set(None);
        self.map.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.key);
    }
}

pub struct Live {
    http: reqwest::Client,
    base: String,
    cache: Mutex<LiveCacheMap>,
    inflight: Mutex<LiveInflightMap>,
}

/// `{base}/maps/{地図}/live/{file}` の URL。地図の id はパスの 1 区間として符号化する
pub fn live_url(base: &str, map: &str, file: &str) -> Option<Url> {
    let mut url = Url::parse(base).ok()?;
    url.path_segments_mut()
        .ok()?
        .pop_if_empty()
        .extend(["maps", map, "live", file]);
    Some(url)
}

/// `{base}/{path}` (`marumasign/<地図>.json` など、BlueMap の web ルート)
pub fn web_url(base: &str, segments: &[&str]) -> Option<Url> {
    let mut url = Url::parse(base).ok()?;
    url.path_segments_mut()
        .ok()?
        .pop_if_empty()
        .extend(segments);
    Some(url)
}

impl Live {
    pub fn new(http: reqwest::Client, base: String) -> Self {
        Self {
            http,
            base,
            cache: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
        }
    }

    /// 200 の本文だけを返す。つながらない・遅い・200 でないときは None
    pub async fn get(&self, map: &str, file: &str, timeout: Duration) -> Option<Vec<u8>> {
        let key = (map.to_string(), file.to_string());
        if let Ok(cache) = self.cache.lock()
            && let Some((at, body)) = cache.get(&key)
            && at.elapsed() < LIVE_CACHE_TTL
        {
            return body.clone();
        }
        let (cell, leader) = {
            let mut guard = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(existing) = guard.get(&key) {
                (existing.clone(), false)
            } else {
                let created = Arc::new(OnceCell::new());
                guard.insert(key.clone(), created.clone());
                (created, true)
            }
        };
        if !leader {
            return cell.get_or_init(|| async { None }).await.clone();
        }
        let _guard = LiveInflightGuard {
            map: &self.inflight,
            key: key.clone(),
            cell: cell.clone(),
        };
        let body = self.fetch_uncached(map, file, timeout).await;
        if let Ok(mut cache) = self.cache.lock() {
            if cache.len() >= 128 && !cache.contains_key(&key) {
                cache.retain(|_, (at, _)| at.elapsed() < LIVE_CACHE_TTL);
                if cache.len() >= 128
                    && let Some(oldest) = cache.keys().next().cloned()
                {
                    cache.remove(&oldest);
                }
            }
            cache.insert(key, (Instant::now(), body.clone()));
        }
        let _ = cell.set(body.clone());
        drop(_guard);
        body
    }

    async fn fetch_uncached(&self, map: &str, file: &str, timeout: Duration) -> Option<Vec<u8>> {
        let url = live_url(&self.base, map, file)?;
        let mut res = self.http.get(url).timeout(timeout).send().await.ok()?;
        if !res.status().is_success() {
            return None;
        }
        if res
            .content_length()
            .is_some_and(|n| n > MAX_BODY_BYTES as u64)
        {
            return None;
        }
        let mut body = Vec::new();
        while let Some(chunk) = res.chunk().await.ok()? {
            if body.len() + chunk.len() > MAX_BODY_BYTES {
                return None;
            }
            body.extend_from_slice(&chunk);
        }
        Some(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_encode_the_map_id_as_one_segment() {
        let url = live_url(
            "http://bluemap.internal.example:8100",
            "overworld",
            "players.json",
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "http://bluemap.internal.example:8100/maps/overworld/live/players.json"
        );
        let url = live_url("http://bluemap.internal.example:8100", "a/b?c#d", "sse").unwrap();
        assert_eq!(
            url.as_str(),
            "http://bluemap.internal.example:8100/maps/a%2Fb%3Fc%23d/live/sse"
        );
        let url = web_url(
            "http://bluemap.internal.example:8100",
            &["marumasign", "x.json"],
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "http://bluemap.internal.example:8100/marumasign/x.json"
        );
    }
}
