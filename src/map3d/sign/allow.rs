//! サーバーが看板のために読んでよい URL の一覧 (`resolve` と `proxy` がただの中継にならないように)。
//!
//! - 看板に書かれた URL (`marumasign/<地図>.json` にいまあるもの)
//! - その転送の行き先 (サーバーが自分で追って確かめたもの)
//! - それらの再生リスト (HLS, MPEG-DASH) と .gltf に書かれた URL (サーバーが自分で読んで取り出したもの)。
//!   再生リストと別のサイトのものは、中継できる中身をさらに絞る (`kinds::classify`)
//!
//! 正本 (移管前) は TORO-WebMap の `server/lib/marumasign/allow.ts`。

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use super::core::{safe_url, without_fragment};
use super::manifest::{ManifestRefs, UrlPattern, match_pattern, pattern_edges_match};

/// 取り出した URL を覚えておく時間
pub const CHILD_TTL: Duration = Duration::from_secs(10 * 60);
pub const ALIAS_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_CHILDREN: usize = 200_000;
const MAX_ALIASES: usize = 50_000;
/// 覚えておく URL の形の数 (全体と、1 つのオリジンごと)
const MAX_PATTERNS: usize = 1000;
const MAX_PATTERNS_PER_ORIGIN: usize = 256;
/// 1 つの URL について、前と後ろが合った形をいくつまで詳しく確かめるか
const MAX_PATTERN_TRIES: usize = 16;

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
}

fn trim(map: &mut HashMap<String, Instant>, max: usize) {
    if map.len() <= max {
        return;
    }
    let now = Instant::now();
    map.retain(|_, until| *until > now);
    while map.len() > max {
        let Some(victim) = map.keys().next().cloned() else {
            break;
        };
        map.remove(&victim);
    }
}

impl AllowList {
    /// 看板に書かれた URL を入れ替える (看板データを読み直したとき)
    pub fn set_roots(&mut self, urls: HashSet<String>) {
        self.roots = urls;
    }

    pub fn root_count(&self) -> usize {
        self.roots.len()
    }

    pub fn is_root(&self, url: &str) -> bool {
        self.roots.contains(url)
    }

    /// 看板の URL (か、その行き先) から転送された先
    pub fn add_alias(&mut self, url: &str) {
        self.aliases
            .insert(url.to_string(), Instant::now() + ALIAS_TTL);
        trim(&mut self.aliases, MAX_ALIASES);
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
        for u in &refs.urls {
            let same = is_same_site(u);
            let list = if same {
                &mut self.children
            } else {
                &mut self.foreign
            };
            list.insert(u.clone(), until);
        }
        trim(&mut self.children, MAX_CHILDREN);
        trim(&mut self.foreign, MAX_CHILDREN);
        for p in refs.patterns {
            let Some(first) = p.parts.first() else {
                continue;
            };
            let origin = origin_of(first).to_string();
            let foreign = !is_same_site(first);
            let list = self.patterns.entry(origin).or_default();
            let key = pattern_key(&p);
            if let Some(old) = list.iter_mut().find(|e| e.key == key) {
                old.until = until;
                old.foreign = old.foreign && foreign;
                continue;
            }
            list.push(PatternEntry {
                key,
                pattern: p,
                until,
                foreign,
            });
            self.pattern_count += 1;
            if list.len() > MAX_PATTERNS_PER_ORIGIN {
                list.remove(0);
                self.pattern_count -= 1;
            }
        }
        if self.pattern_count > MAX_PATTERNS {
            self.trim_patterns();
        }
    }

    fn trim_patterns(&mut self) {
        let now = Instant::now();
        for list in self.patterns.values_mut() {
            let before = list.len();
            list.retain(|e| e.until >= now);
            self.pattern_count -= before - list.len();
        }
        self.patterns.retain(|_, list| !list.is_empty());
        // 古くからあるものから消す (消したものは、再生リストを読み直せばまた入る)
        while self.pattern_count > MAX_PATTERNS {
            let Some(origin) = self.patterns.keys().next().cloned() else {
                break;
            };
            if let Some(list) = self.patterns.get_mut(&origin) {
                if list.is_empty() {
                    self.patterns.remove(&origin);
                } else {
                    list.remove(0);
                    self.pattern_count -= 1;
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
}
