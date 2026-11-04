//! Wiki の全文検索。原稿をメモリに持って、自分で探す
//!
//! GitHub のコード検索は TORO-Wiki を索引していない (常に 0 件を返す) ので使わない。
//! 取り直すのは変わった原稿だけ。ツリーを見に行くのも一定時間に 1 回なので、
//! 検索が何度来ても GitHub の利用枠をほとんど使わない。
//!
//! 点数を付ける手間は「語の数 × 原稿の量」なので、語の長さと数に上限を設け、ロックの外で点数を付ける。

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

use futures_util::{StreamExt, stream};
use tokio::sync::{Mutex, Semaphore};

use super::github;
use crate::App;

/// 原稿を同時に取りに行く数
const FETCH_CONCURRENCY: usize = 8;
/// パスに語が含まれるときの重み (本文の出現回数より優先する)
const PATH_MATCH_WEIGHT: usize = 1000;
/// 検索の文字列の長さの上限 (バイト)
const MAX_QUERY_BYTES: usize = 512;
/// 語の数の上限 (同じ語は 1 つに数える)
const MAX_TERMS: usize = 16;
/// 同時に点数を付ける数。同じ機械で動く Minecraft サーバーの CPU を使い切らないように
const SCORING_CONCURRENCY: usize = 2;

struct Doc {
    sha: String,
    lowercase_text: String,
}

/// docs/ を除いたパス → 原稿
type Docs = HashMap<String, Arc<Doc>>;

pub struct Index {
    ttl: Duration,
    /// 読み直しを 1 つずつにする。中身は最後に見に行った時刻
    checked_at: Mutex<Option<Instant>>,
    /// いまの原稿。検索は写しを取り、ロックの外で読む
    docs: RwLock<Arc<Docs>>,
    scoring: Semaphore,
}

/// 小文字にして空白で区切り、同じ語を 1 つにする
fn distinct_terms(query: &str) -> Vec<String> {
    let lowercase_query = query.to_lowercase();
    let mut terms: Vec<String> = Vec::new();
    for term in lowercase_query.split_whitespace() {
        if !terms.iter().any(|known| known == term) {
            terms.push(term.to_string());
        }
    }
    terms
}

/// 検索の文字列が上限の中か
pub fn query_fits(query: &str) -> bool {
    query.len() <= MAX_QUERY_BYTES && distinct_terms(query).len() <= MAX_TERMS
}

/// 検索に使う語。上限を超えた分は捨てる
fn search_terms(query: &str) -> Vec<String> {
    let mut end = query.len().min(MAX_QUERY_BYTES);
    while !query.is_char_boundary(end) {
        end -= 1;
    }
    let mut terms = distinct_terms(&query[..end]);
    terms.truncate(MAX_TERMS);
    terms
}

impl Index {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            checked_at: Mutex::new(None),
            docs: RwLock::new(Arc::default()),
            scoring: Semaphore::new(SCORING_CONCURRENCY),
        }
    }

    /// 語をすべて含む原稿のパスを、よく一致する順に返す。一度も原稿を読めていないなら None
    pub async fn search(&self, app: &App, query: &str) -> Option<Vec<String>> {
        let docs = self.fresh_docs(app).await;
        if docs.is_empty() {
            return None;
        }

        let terms = search_terms(query);
        if terms.is_empty() {
            return Some(Vec::new());
        }
        let terms: Vec<&str> = terms.iter().map(String::as_str).collect();
        let _turn = self.scoring.acquire().await.ok()?;
        Some(rank(&docs, &terms))
    }

    fn current_docs(&self) -> Arc<Docs> {
        self.docs
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// 必要なら読み直して、いまの原稿を返す。
    /// 読み直しの最中は、読めた原稿があれば終わりを待たない (遅い GitHub に同時処理の枠を取られないように)
    async fn fresh_docs(&self, app: &App) -> Arc<Docs> {
        let turn = if self.current_docs().is_empty() {
            Some(self.checked_at.lock().await)
        } else {
            self.checked_at.try_lock().ok()
        };
        if let Some(mut checked_at) = turn {
            self.refresh(&mut checked_at, app).await;
        }
        self.current_docs()
    }

    async fn refresh(&self, checked_at: &mut Option<Instant>, app: &App) {
        if checked_at.is_some_and(|at| at.elapsed() < self.ttl) {
            return;
        }
        // 失敗しても一定時間は聞き直さない (GitHub の障害中に検索が来るたびに叩かない)
        *checked_at = Some(Instant::now());

        let Some(blobs) = github::doc_blobs(app).await else {
            eprintln!("wiki search: could not read the document tree");
            return;
        };

        let current: Vec<(String, String)> = blobs
            .into_iter()
            .filter_map(|blob| {
                let path = blob.path.strip_prefix("docs/")?.to_string();
                Some((path, blob.sha))
            })
            .collect();
        let mut next: Docs = self
            .current_docs()
            .iter()
            .filter(|(path, _)| current.iter().any(|(p, _)| p == *path))
            .map(|(path, doc)| (path.clone(), doc.clone()))
            .collect();

        let changed: Vec<(String, String)> = current
            .into_iter()
            .filter(|(path, sha)| next.get(path).is_none_or(|doc| doc.sha != *sha))
            .collect();
        let fetched: Vec<_> = stream::iter(changed)
            .map(|(path, sha)| async move {
                let text = github::blob_text(app, &sha).await;
                (path, sha, text)
            })
            .buffer_unordered(FETCH_CONCURRENCY)
            .collect()
            .await;

        for (path, sha, text) in fetched {
            match text {
                Some(text) => {
                    let lowercase_text = text.to_lowercase();
                    next.insert(
                        path,
                        Arc::new(Doc {
                            sha,
                            lowercase_text,
                        }),
                    );
                }
                // 古い内容を残し、次の更新でまた取りに行く
                None => eprintln!("wiki search: could not read {path}"),
            }
        }
        *self.docs.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(next);
    }
}

fn rank(docs: &Docs, terms: &[&str]) -> Vec<String> {
    let mut hits: Vec<(usize, &String)> = docs
        .iter()
        .filter_map(|(path, doc)| score(path, &doc.lowercase_text, terms).map(|s| (s, path)))
        .collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    hits.into_iter().map(|(_, path)| path.clone()).collect()
}

/// すべての語を含むときだけ点数を返す
fn score(path: &str, lowercase_text: &str, terms: &[&str]) -> Option<usize> {
    let lowercase_path = path.to_lowercase();
    let mut total = 0;
    for term in terms {
        let in_path = lowercase_path.matches(term).count();
        let in_text = lowercase_text.matches(term).count();
        if in_path + in_text == 0 {
            return None;
        }
        total += in_path * PATH_MATCH_WEIGHT + in_text;
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_terms_must_match_and_paths_rank_higher() {
        let in_text = score("guide.md", "traincarts を使う", &["traincarts"]).unwrap();
        let in_path = score("traincarts.md", "説明", &["traincarts"]).unwrap();
        assert!(in_path > in_text);
        assert_eq!(score("a.md", "traincarts", &["traincarts", "無い語"]), None);
        assert!(score("a.md", "電車 と 駅", &["電車", "駅"]).is_some());
    }

    #[test]
    fn terms_are_lowercased_and_deduplicated() {
        assert_eq!(
            distinct_terms("  TrainCarts 電車 traincarts\u{3000}電車 "),
            ["traincarts", "電車"]
        );
        assert!(distinct_terms("   ").is_empty());
    }

    #[test]
    fn queries_over_the_limits_do_not_fit() {
        assert!(query_fits(""));
        assert!(query_fits(&"あ".repeat(MAX_QUERY_BYTES / 3)));
        assert!(!query_fits(&"x".repeat(MAX_QUERY_BYTES + 1)));

        let words = |n: usize| {
            (0..n)
                .map(|i| format!("w{i}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert!(query_fits(&words(MAX_TERMS)));
        assert!(!query_fits(&words(MAX_TERMS + 1)));
        // 同じ語は 1 つに数える
        assert!(query_fits(&"w ".repeat(MAX_TERMS * 2)));
    }

    #[test]
    fn search_terms_never_exceed_the_limits() {
        let words: Vec<String> = (0..100).map(|i| format!("w{i}")).collect();
        assert_eq!(search_terms(&words.join(" ")).len(), MAX_TERMS);

        // 文字の途中で切らない
        let long = "あ".repeat(MAX_QUERY_BYTES);
        let terms = search_terms(&long);
        assert_eq!(terms.len(), 1);
        assert!(terms[0].len() <= MAX_QUERY_BYTES);
        assert!(terms[0].chars().all(|c| c == 'あ'));
    }

    #[test]
    fn hits_are_sorted_by_score_then_path() {
        let doc = |text: &str| {
            Arc::new(Doc {
                sha: String::new(),
                lowercase_text: text.to_string(),
            })
        };
        let docs: Docs = HashMap::from([
            ("b.md".to_string(), doc("電車")),
            ("a.md".to_string(), doc("電車")),
            ("c.md".to_string(), doc("電車 電車")),
            ("d.md".to_string(), doc("駅")),
        ]);
        assert_eq!(rank(&docs, &["電車"]), ["c.md", "a.md", "b.md"]);
    }
}
