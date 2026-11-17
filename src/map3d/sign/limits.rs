//! 数と量の上限 (1 つの IP からの数・量、同時に読む数) と、読んだものの置き場 (古いものから消す)。
//!
//! 正本 (移管前) は TORO-WebMap の `server/lib/marumasign/limits.ts`。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// IP アドレスの文字列を読む (`[` `]` と、`%` 以降のゾーンは除く)
pub fn parse_ip(text: &str) -> Option<IpAddr> {
    let text = text.trim().trim_start_matches('[').trim_end_matches(']');
    let text = text.split('%').next().unwrap_or(text);
    text.parse().ok()
}

/// 上限を数える単位。IPv6 は /64 にまとめる (1 つの回線に /64 が丸ごと割り当てられるので、/128 で数えても意味がない)。
/// IPv4 を写した IPv6 は IPv4 と同じ。IP アドレスでないものはそのまま (64 文字まで)
pub fn limit_key(ip: &str) -> String {
    match parse_ip(ip) {
        None => ip.trim().chars().take(64).collect(),
        Some(IpAddr::V4(v4)) => v4.to_string(),
        Some(IpAddr::V6(v6)) => {
            let o = v6.octets();
            if o[..10].iter().all(|b| *b == 0) && o[10] == 0xFF && o[11] == 0xFF {
                return format!("{}.{}.{}.{}", o[12], o[13], o[14], o[15]);
            }
            let head: String = o[..8].iter().map(|b| format!("{b:02x}")).collect();
            format!("{head}/64")
        }
    }
}

/// 数える鍵の数の上限 (ばらばらの鍵でメモリを増やさせない)
const MAX_KEYS: usize = 50_000;

/// 決まった時間ごとの数の上限 (時間ごとに数え直す)
pub struct WindowLimit {
    counts: Mutex<HashMap<String, (Instant, u64)>>,
    window: Duration,
    limit: u64,
    max_keys: usize,
}

impl WindowLimit {
    pub fn new(window: Duration, limit: u64) -> Self {
        Self::with_max_keys(window, limit, MAX_KEYS)
    }

    pub fn with_max_keys(window: Duration, limit: u64, max_keys: usize) -> Self {
        Self {
            counts: Mutex::new(HashMap::new()),
            window,
            limit,
            max_keys,
        }
    }

    /// n 足して、上限を超えたら true
    pub fn hit(&self, key: &str, n: u64) -> bool {
        let mut counts = lock(&self.counts);
        let entry = self.entry(&mut counts, key);
        entry.1 = entry.1.saturating_add(n);
        entry.1 > self.limit
    }

    /// 足さずに、もう上限を超えているか
    pub fn over(&self, key: &str) -> bool {
        let mut counts = lock(&self.counts);
        self.entry(&mut counts, key).1 > self.limit
    }

    pub fn len(&self) -> usize {
        lock(&self.counts).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn entry<'a>(
        &self,
        counts: &'a mut HashMap<String, (Instant, u64)>,
        key: &str,
    ) -> &'a mut (Instant, u64) {
        let now = Instant::now();
        let stale = counts
            .get(key)
            .is_none_or(|(start, _)| now.duration_since(*start) > self.window);
        if stale {
            if counts.len() >= self.max_keys {
                // 時間の過ぎたものをまとめて消し、それでも多ければ、ほかの鍵を消す
                counts.retain(|_, (start, _)| now.duration_since(*start) <= self.window);
                while counts.len() >= self.max_keys {
                    let Some(victim) = counts.keys().next().cloned() else {
                        break;
                    };
                    counts.remove(&victim);
                }
            }
            counts.insert(key.to_string(), (now, 0));
        }
        counts.get_mut(key).expect("inserted above")
    }
}

/// 同時に行う数の上限 (空くのを待たずに断る)
pub struct Gate {
    state: Mutex<(usize, HashMap<String, usize>)>,
    max: usize,
    max_per_key: usize,
}

/// 入っている間だけ持つ。手放す (drop) と枠が空く
pub struct GatePermit {
    gate: Arc<Gate>,
    key: String,
}

impl Gate {
    pub fn new(max: usize, max_per_key: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new((0, HashMap::new())),
            max,
            max_per_key,
        })
    }

    /// いっぱいなら None
    pub fn enter(self: &Arc<Self>, key: &str) -> Option<GatePermit> {
        let mut state = lock(&self.state);
        let n = state.1.get(key).copied().unwrap_or(0);
        if state.0 >= self.max || n >= self.max_per_key {
            return None;
        }
        state.0 += 1;
        state.1.insert(key.to_string(), n + 1);
        Some(GatePermit {
            gate: self.clone(),
            key: key.to_string(),
        })
    }

    pub fn size(&self) -> usize {
        lock(&self.state).0
    }
}

impl Drop for GatePermit {
    fn drop(&mut self) {
        let mut state = lock(&self.gate.state);
        state.0 = state.0.saturating_sub(1);
        let left = state
            .1
            .get(&self.key)
            .copied()
            .unwrap_or(1)
            .saturating_sub(1);
        if left == 0 {
            state.1.remove(&self.key);
        } else {
            state.1.insert(self.key.clone(), left);
        }
    }
}

struct Entry<T> {
    value: T,
    bytes: usize,
    until: Instant,
    used: u64,
}

struct CacheState<T> {
    map: HashMap<String, Entry<T>>,
    total: usize,
    tick: u64,
}

/// 大きさの合計に上限のある、時間で切れる置き場 (最近使ったものを残す)
pub struct ByteCache<T> {
    state: Mutex<CacheState<T>>,
    max_bytes: usize,
    max_entries: usize,
}

impl<T: Clone> ByteCache<T> {
    pub fn new(max_bytes: usize, max_entries: usize) -> Self {
        Self {
            state: Mutex::new(CacheState {
                map: HashMap::new(),
                total: 0,
                tick: 0,
            }),
            max_bytes,
            max_entries,
        }
    }

    pub fn get(&self, key: &str) -> Option<T> {
        let mut state = lock(&self.state);
        state.tick += 1;
        let tick = state.tick;
        let expired = state.map.get(key)?.until < Instant::now();
        if expired {
            if let Some(old) = state.map.remove(key) {
                state.total -= old.bytes;
            }
            return None;
        }
        let entry = state.map.get_mut(key)?;
        entry.used = tick;
        Some(entry.value.clone())
    }

    /// `bytes` は数える大きさ。1 つが上限より大きければ入れない
    pub fn set(&self, key: &str, value: T, bytes: usize, ttl: Duration) {
        if bytes > self.max_bytes {
            return;
        }
        let mut state = lock(&self.state);
        if let Some(old) = state.map.remove(key) {
            state.total -= old.bytes;
        }
        state.tick += 1;
        let used = state.tick;
        state.total += bytes;
        state.map.insert(
            key.to_string(),
            Entry {
                value,
                bytes,
                until: Instant::now() + ttl,
                used,
            },
        );
        while state.total > self.max_bytes || state.map.len() > self.max_entries {
            let Some(victim) = state
                .map
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(old) = state.map.remove(&victim) {
                state.total -= old.bytes;
            }
        }
    }

    pub fn total_bytes(&self) -> usize {
        lock(&self.state).total
    }

    pub fn len(&self) -> usize {
        lock(&self.state).map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(limit_key("203.0.113.9"), "203.0.113.9");
        assert_eq!(limit_key("::ffff:203.0.113.9"), "203.0.113.9");
        assert_eq!(limit_key("2001:db8:1:2:3:4:5:6"), "20010db800010002/64");
        assert_eq!(limit_key("2001:db8:1:2::9"), limit_key("2001:db8:1:2::1"));
        assert_eq!(limit_key("[2001:db8::1]"), limit_key("2001:db8::2"));
        assert_eq!(limit_key("unknown"), "unknown");
        assert_eq!(limit_key(&"x".repeat(100)).len(), 64);
    }

    #[test]
    fn window_limit_counts_and_resets() {
        let limit = WindowLimit::new(Duration::from_millis(50), 3);
        assert!(!limit.hit("a", 1));
        assert!(!limit.hit("a", 2));
        assert!(limit.hit("a", 1));
        assert!(limit.over("a"));
        assert!(!limit.over("b"));
        std::thread::sleep(Duration::from_millis(70));
        assert!(!limit.over("a"));
        assert!(!limit.hit("a", 3));
    }

    #[test]
    fn window_limit_bounds_keys() {
        let limit = WindowLimit::with_max_keys(Duration::from_secs(60), 10, 100);
        for i in 0..1000 {
            limit.hit(&format!("k{i}"), 1);
        }
        assert!(limit.len() <= 100);
    }

    #[test]
    fn gate_limits_total_and_per_key() {
        let gate = Gate::new(3, 2);
        let a1 = gate.enter("a").unwrap();
        let _a2 = gate.enter("a").unwrap();
        assert!(gate.enter("a").is_none());
        let _b = gate.enter("b").unwrap();
        assert!(gate.enter("c").is_none());
        assert_eq!(gate.size(), 3);
        drop(a1);
        assert_eq!(gate.size(), 2);
        assert!(gate.enter("a").is_some());
    }

    #[test]
    fn byte_cache_evicts_least_recently_used() {
        let cache: ByteCache<u8> = ByteCache::new(10, 100);
        cache.set("a", 1, 4, Duration::from_secs(60));
        cache.set("b", 2, 4, Duration::from_secs(60));
        assert_eq!(cache.get("a"), Some(1));
        cache.set("c", 3, 4, Duration::from_secs(60));
        assert_eq!(cache.get("b"), None);
        assert_eq!(cache.get("a"), Some(1));
        assert_eq!(cache.get("c"), Some(3));
        assert!(cache.total_bytes() <= 10);
        // 1 つが大きすぎれば入れない
        cache.set("big", 9, 11, Duration::from_secs(60));
        assert_eq!(cache.get("big"), None);
    }

    #[test]
    fn byte_cache_expires() {
        let cache: ByteCache<u8> = ByteCache::new(10, 100);
        cache.set("a", 1, 1, Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(cache.get("a"), None);
        assert_eq!(cache.total_bytes(), 0);
    }
}
