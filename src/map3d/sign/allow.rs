//! サーバーが看板のために読んでよい URL の一覧 (`resolve` と `proxy` がただの中継にならないように)。
//!
//! - 看板に書かれた URL (`marumasign/<地図>.json` にいまあるもの)
//! - その転送の行き先 (サーバーが自分で追って確かめたもの)
//! - それらの再生リスト (HLS, MPEG-DASH) と .gltf に書かれた URL (サーバーが自分で読んで取り出したもの)。
//!   再生リストと別のサイトのものは、中継できる中身をさらに絞る (`kinds::classify`)
//!
//! 正本 (移管前) は、移管前の地図サーバーの `server/lib/marumasign/allow.ts`。

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use super::super::budget::SIGN_ALLOW_LIST_BYTES;
use super::core::{safe_url, without_fragment};
use super::manifest::{ManifestRefs, UrlPattern, match_pattern, pattern_edges_match};

/// 取り出した URL を覚えておく時間
pub const CHILD_TTL: Duration = Duration::from_secs(10 * 60);
pub const ALIAS_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_CHILDREN: usize = 200_000;
const MAX_ALIASES: usize = 50_000;
/// 1 つのマニフェストから 1 度に取り込む参照数の上限
const MAX_REFS_PER_MANIFEST: usize = 2_000;
const MAX_PATTERNS_PER_MANIFEST: usize = 64;
/// 覚えておく URL の形の数 (全体と、1 つのオリジンごと)
const MAX_PATTERNS: usize = 1000;
const MAX_PATTERNS_PER_ORIGIN: usize = 256;
/// 1 つの URL について、前と後ろが合った形をいくつまで詳しく確かめるか
const MAX_PATTERN_TRIES: usize = 16;
const ENTRY_OVERHEAD: usize = 96;

/// 看板データ 1 つ (`marumasign/<地図>.json`) から、読むかもしれない URL (https のもの、「#」を除く)
pub fn sign_urls(data: &serde_json::Value, into: &mut HashSet<String>) {
    let Some(signs) = data
        .get("signs")
        .and_then(|s| s.as_array())
        .filter(|_| data.get("format").and_then(|f| f.as_i64()) == Some(1))
    else {
        return;
    };
    for raw in signs {
        let Some(sign) = raw.as_array().filter(|a| a.len() >= 7) else {
            continue;
        };
        for v in [&sign[5], &sign[6]] {
            if let Some(url) = v.as_str().and_then(|s| safe_url(s, true)) {
                into.insert(without_fragment(&url).to_string());
            }
        }
    }
}

/// 読んでよい理由。Child は看板やその再生リストと同じサイト、Foreign は再生リストと別のサイトに書かれていたもの
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Allowed {
    Root,
    Alias,
    Child,
    Foreign,
}

/// URL のスキームとホスト (確かめた URL だけを渡す)
fn origin_of(url: &str) -> &str {
    let start = url.find("://").map_or(0, |i| i + 3);
    match url[start..].find('/') {
        Some(i) => &url[..start + i],
        None => url,
    }
}

/// 共有ホスティングの公開接尾辞 (借り手ごとにホスト名が分かれるもの)。よく知られたものだけを挙げる
const SHARED_SUFFIXES: &[&str] = &[
    "github.io",
    "gitlab.io",
    "codeberg.page",
    "pages.dev",
    "workers.dev",
    "netlify.app",
    "vercel.app",
    "herokuapp.com",
    "firebaseapp.com",
    "web.app",
    "azurewebsites.net",
    "glitch.me",
    "surge.sh",
    "deno.dev",
    "fly.dev",
    "onrender.com",
    "hf.space",
    "pythonanywhere.com",
    "streamlit.app",
    "repl.co",
    "replit.app",
];

/// URL のサイト (登録できるドメイン)。公開接尾辞の全体の一覧は持たないので、co.jp や com.au のような 2 段のものだけを見分ける。
/// 間違えても、別のサイトと見たものは中継できる中身が絞られ、同じサイトと見たものは大きさの上限が変わるだけ
pub fn site_of(url: &str) -> String {
    let Some(host) = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
    else {
        return url.to_string();
    };
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() <= 2 {
        return host;
    }
    let last = labels[labels.len() - 1];
    let second = labels[labels.len() - 2];
    let short = last.len() == 2 && second.len() <= 3;
    let mut take = if short { 3 } else { 2 };
    if SHARED_SUFFIXES.contains(&labels[labels.len() - take..].join(".").as_str()) {
        take += 1;
    }
    labels[labels.len().saturating_sub(take)..].join(".")
}

struct PatternEntry {
    key: String,
    pattern: UrlPattern,
    until: Instant,
    foreign: bool,
}

fn pattern_key(p: &UrlPattern) -> String {
    format!("{:?}", (&p.parts, &p.slots, &p.ids.order))
}

#[derive(Default)]
pub struct AllowList {
    roots: HashSet<String>,
    aliases: HashMap<String, Instant>,
    children: HashMap<String, Instant>,
    foreign: HashMap<String, Instant>,
    patterns: HashMap<String, Vec<PatternEntry>>,
    pattern_count: usize,
    total_bytes: usize,
}

fn url_bytes(url: &str) -> usize {
    url.len().saturating_add(ENTRY_OVERHEAD)
}

fn allow_url(url: &str) -> Option<String> {
    safe_url(url, true).or_else(|| {
        if cfg!(test)
            && (url.starts_with("http://127.0.0.1:")
                || url.starts_with("http://127.0.0.1/")
                || url.starts_with("http://[::1]:"))
        {
            safe_url(url, false)
        } else {
            None
        }
    })
}

fn pattern_bytes(key: &str, p: &UrlPattern) -> usize {
    let parts: usize = p
        .parts
        .iter()
        .map(|s| s.len().saturating_add(24))
        .sum();
    key.len().saturating_add(parts).saturating_add(ENTRY_OVERHEAD)
}

fn trim(map: &mut HashMap<String, Instant>, max: usize, total_bytes: &mut usize) {
    if map.len() <= max {
        return;
    }
    let now = Instant::now();
    map.retain(|k, until| {
        if *until > now {
            true
        } else {
            *total_bytes = total_bytes.saturating_sub(url_bytes(k));
            false
        }
    });
    if map.len() <= max {
        return;
    }
    let excess = map.len() - max;
    let mut order: Vec<(Instant, String)> = map.iter().map(|(k, &t)| (t, k.clone())).collect();
    order.sort_by_key(|(t, _)| *t);
    for (_, victim) in order.into_iter().take(excess) {
        if map.remove(&victim).is_some() {
            *total_bytes = total_bytes.saturating_sub(url_bytes(&victim));
        }
    }
}

fn evict_oldest_from_map(
    map: &mut HashMap<String, Instant>,
    bytes_to_free: usize,
    total_bytes: &mut usize,
) -> usize {
    if map.is_empty() || bytes_to_free == 0 {
        return 0;
    }
    let mut order: Vec<(Instant, String)> = map.iter().map(|(k, &t)| (t, k.clone())).collect();
    order.sort_by_key(|(t, _)| *t);
    let mut freed = 0;
    for (_, victim) in order {
        if freed >= bytes_to_free {
            break;
        }
        if map.remove(&victim).is_some() {
            let b = url_bytes(&victim);
            *total_bytes = total_bytes.saturating_sub(b);
            freed += b;
        }
    }
    freed
}

impl AllowList {
    /// 看板に書かれた URL を入れ替える (看板データを読み直したとき)
    pub fn set_roots(&mut self, urls: HashSet<String>) {
        let old: usize = self.roots.iter().map(|u| url_bytes(u)).sum();
        self.total_bytes = self.total_bytes.saturating_sub(old);
        self.roots = urls;
        let new_bytes: usize = self.roots.iter().map(|u| url_bytes(u)).sum();
        self.total_bytes = self.total_bytes.saturating_add(new_bytes);
        self.enforce_byte_budget();
    }

    pub fn root_count(&self) -> usize {
        self.roots.len()
    }

    pub fn is_root(&self, url: &str) -> bool {
        self.roots.contains(url)
    }

    pub fn estimated_bytes(&self) -> usize {
        self.total_bytes
    }

    fn insert_url(
        map: &mut HashMap<String, Instant>,
        url: String,
        until: Instant,
        total_bytes: &mut usize,
    ) {
        if let Some(prev) = map.get_mut(&url) {
            *prev = until;
        } else {
            *total_bytes = total_bytes.saturating_add(url_bytes(&url));
            map.insert(url, until);
        }
    }

    fn enforce_byte_budget(&mut self) {
        if self.total_bytes <= SIGN_ALLOW_LIST_BYTES {
            return;
        }
        let now = Instant::now();
        for map in [&mut self.children, &mut self.foreign, &mut self.aliases] {
            let tb = &mut self.total_bytes;
            map.retain(|k, until| {
                if *until > now {
                    true
                } else {
                    *tb = tb.saturating_sub(url_bytes(k));
                    false
                }
            });
        }
        self.purge_expired_patterns(now);
        if self.total_bytes <= SIGN_ALLOW_LIST_BYTES {
            return;
        }
        // メモリ上限を超えたら 90% 程度まで古い項目から順に削る
        let target = SIGN_ALLOW_LIST_BYTES.saturating_mul(9) / 10;
        let mut need = self.total_bytes.saturating_sub(target);
        need = need.saturating_sub(evict_oldest_from_map(
            &mut self.foreign,
            need,
            &mut self.total_bytes,
        ));
        need = need.saturating_sub(evict_oldest_from_map(
            &mut self.children,
            need,
            &mut self.total_bytes,
        ));
        need = need.saturating_sub(evict_oldest_from_map(
            &mut self.aliases,
            need,
            &mut self.total_bytes,
        ));
        if need > 0 {
            self.evict_oldest_patterns(need);
        }
    }

    fn purge_expired_patterns(&mut self, now: Instant) {
        for list in self.patterns.values_mut() {
            let mut i = 0;
            while i < list.len() {
                if list[i].until >= now {
                    i += 1;
                } else {
                    let removed = list.remove(i);
                    self.pattern_count = self.pattern_count.saturating_sub(1);
                    self.total_bytes = self
                        .total_bytes
                        .saturating_sub(pattern_bytes(&removed.key, &removed.pattern));
                }
            }
        }
        self.patterns.retain(|_, list| !list.is_empty());
    }

    fn evict_oldest_patterns(&mut self, bytes_to_free: usize) {
        let mut freed = 0;
        while freed < bytes_to_free {
            let Some(origin) = self.patterns.keys().next().cloned() else {
                break;
            };
            if let Some(list) = self.patterns.get_mut(&origin) {
                if list.is_empty() {
                    self.patterns.remove(&origin);
                } else {
                    let removed = list.remove(0);
                    self.pattern_count = self.pattern_count.saturating_sub(1);
                    let b = pattern_bytes(&removed.key, &removed.pattern);
                    self.total_bytes = self.total_bytes.saturating_sub(b);
                    freed += b;
                }
            }
        }
    }

    /// 看板の URL (か、その行き先) から転送された先
    pub fn add_alias(&mut self, url: &str) {
        let Some(clean) = allow_url(url) else {
            return;
        };
        Self::insert_url(
            &mut self.aliases,
            clean,
            Instant::now() + ALIAS_TTL,
            &mut self.total_bytes,
        );
        trim(&mut self.aliases, MAX_ALIASES, &mut self.total_bytes);
        self.enforce_byte_budget();
    }

    /// 読んでよいと分かっている再生リストなどに書かれていた URL。`parent` はその再生リストなどの URL (転送されたなら、最後の URL)
    pub fn add_refs(&mut self, refs: ManifestRefs, parent: &str) {
        let until = Instant::now() + CHILD_TTL;
        let site = site_of(parent);
        // オリジンごとに 1 度だけ調べる
        let mut same_site: HashMap<String, bool> = HashMap::new();
        let mut is_same_site = |url: &str| -> bool {
            *same_site
                .entry(origin_of(url).to_string())
                .or_insert_with(|| site_of(url) == site)
        };
        for u in refs.urls.iter().take(MAX_REFS_PER_MANIFEST) {
            let Some(clean) = allow_url(u) else {
                continue;
            };
            let same = is_same_site(&clean);
            let list = if same {
                &mut self.children
            } else {
                &mut self.foreign
            };
            Self::insert_url(list, clean, until, &mut self.total_bytes);
        }
        trim(&mut self.children, MAX_CHILDREN, &mut self.total_bytes);
        trim(&mut self.foreign, MAX_CHILDREN, &mut self.total_bytes);
        for p in refs.patterns.into_iter().take(MAX_PATTERNS_PER_MANIFEST) {
            let Some(first) = p.parts.first() else {
                continue;
            };
            if first.len() > 2000 {
                continue;
            }
            let origin = origin_of(first).to_string();
            let foreign = !is_same_site(first);
            let list = self.patterns.entry(origin).or_default();
            let key = pattern_key(&p);
            if let Some(old) = list.iter_mut().find(|e| e.key == key) {
                old.until = until;
                old.foreign = old.foreign && foreign;
                continue;
            }
            self.total_bytes = self.total_bytes.saturating_add(pattern_bytes(&key, &p));
            list.push(PatternEntry {
                key,
                pattern: p,
                until,
                foreign,
            });
            self.pattern_count += 1;
            if list.len() > MAX_PATTERNS_PER_ORIGIN {
                let removed = list.remove(0);
                self.pattern_count -= 1;
                self.total_bytes = self
                    .total_bytes
                    .saturating_sub(pattern_bytes(&removed.key, &removed.pattern));
            }
        }
        if self.pattern_count > MAX_PATTERNS {
            self.trim_patterns();
        }
        self.enforce_byte_budget();
    }

    fn trim_patterns(&mut self) {
        let now = Instant::now();
        self.purge_expired_patterns(now);
        // 古くからあるものから消す (消したものは、再生リストを読み直せばまた入る)
        while self.pattern_count > MAX_PATTERNS {
            let Some(origin) = self.patterns.keys().next().cloned() else {
                break;
            };
            if let Some(list) = self.patterns.get_mut(&origin) {
                if list.is_empty() {
                    self.patterns.remove(&origin);
                } else {
                    let removed = list.remove(0);
                    self.pattern_count -= 1;
                    self.total_bytes = self
                        .total_bytes
                        .saturating_sub(pattern_bytes(&removed.key, &removed.pattern));
                }
            }
        }
    }

    /// 読んでよいか (どの理由で)
    pub fn allowed(&self, url: &str) -> Option<Allowed> {
        if self.roots.contains(url) {
            return Some(Allowed::Root);
        }
        let now = Instant::now();
        let live = |map: &HashMap<String, Instant>| map.get(url).is_some_and(|u| *u > now);
        if live(&self.aliases) {
            return Some(Allowed::Alias);
        }
        if live(&self.children) {
            return Some(Allowed::Child);
        }
        if live(&self.foreign) {
            return Some(Allowed::Foreign);
        }
        let list = self.patterns.get(origin_of(url))?;
        let mut tries = 0;
        for e in list {
            if e.until <= now || !pattern_edges_match(&e.pattern, url) {
                continue;
            }
            tries += 1;
            if tries > MAX_PATTERN_TRIES {
                return None;
            }
            if match_pattern(&e.pattern, url) {
                return Some(if e.foreign {
                    Allowed::Foreign
                } else {
                    Allowed::Child
                });
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sign_urls_reads_https_urls_without_fragments() {
        let data = json!({
            "format": 1,
            "signs": [
                [0, 0, 0, 0, 0, "https://example.com/a.png#frag", "http://example.com/b.png"],
                [0, 0, 0, 0, 0, "https://example.com/c.png", 5],
                [0, 0, 0],
                "bad"
            ]
        });
        let mut urls = HashSet::new();
        sign_urls(&data, &mut urls);
        let mut got: Vec<_> = urls.into_iter().collect();
        got.sort();
        assert_eq!(
            got,
            vec!["https://example.com/a.png", "https://example.com/c.png"]
        );
        // format が違う、形が違うデータは何も足さない
        let mut none = HashSet::new();
        sign_urls(
            &json!({"format": 2, "signs": [[0,0,0,0,0,"https://a.example/x","https://a.example/y"]]}),
            &mut none,
        );
        sign_urls(&json!([1, 2]), &mut none);
        assert!(none.is_empty());
    }

    #[test]
    fn sites() {
        assert_eq!(site_of("https://cdn.example.com/a"), "example.com");
        assert_eq!(site_of("https://example.com/a"), "example.com");
        assert_eq!(site_of("https://a.b.example.co.jp/x"), "example.co.jp");
        assert_eq!(site_of("https://alice.github.io/x"), "alice.github.io");
        assert_eq!(site_of("https://www.alice.github.io/x"), "alice.github.io");
        assert_ne!(
            site_of("https://alice.github.io/"),
            site_of("https://bob.github.io/")
        );
    }

    #[test]
    fn roots_aliases_and_refs() {
        let mut list = AllowList::default();
        assert_eq!(list.allowed("https://a.example/x"), None);
        list.set_roots(["https://a.example/x".to_string()].into());
        assert_eq!(list.allowed("https://a.example/x"), Some(Allowed::Root));
        list.add_alias("https://short.example/y");
        assert_eq!(
            list.allowed("https://short.example/y"),
            Some(Allowed::Alias)
        );
        let refs = ManifestRefs {
            urls: vec![
                "https://cdn.a.example/seg1.ts".to_string(),
                "https://other.test/seg2.ts".to_string(),
            ],
            ..Default::default()
        };
        list.add_refs(refs, "https://a.example/live.m3u8");
        assert_eq!(
            list.allowed("https://cdn.a.example/seg1.ts"),
            Some(Allowed::Child)
        );
        assert_eq!(
            list.allowed("https://other.test/seg2.ts"),
            Some(Allowed::Foreign)
        );
        assert_eq!(list.allowed("https://other.test/seg3.ts"), None);
    }

    #[test]
    fn enforces_byte_budget_and_url_limits() {
        let mut list = AllowList::default();
        let long_path = "a".repeat(2500);
        list.add_alias(&format!("https://short.example/{long_path}"));
        assert_eq!(list.aliases.len(), 0);
        list.add_alias("http://short.example/insecure");
        assert_eq!(list.aliases.len(), 0);

        let chunk = "b".repeat(1800);
        let urls: Vec<String> = (0..12_000)
            .map(|i| format!("https://cdn.example/{i:05}/{chunk}"))
            .collect();
        for batch in urls.chunks(MAX_REFS_PER_MANIFEST) {
            list.add_refs(
                ManifestRefs {
                    urls: batch.to_vec(),
                    ..Default::default()
                },
                "https://root.example/index.m3u8",
            );
            assert!(list.estimated_bytes() <= SIGN_ALLOW_LIST_BYTES);
        }
        assert!(list.estimated_bytes() <= SIGN_ALLOW_LIST_BYTES);
    }
}
