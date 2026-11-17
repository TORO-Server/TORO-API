//! BlueMap 内蔵の Web サーバー からの取得。プレイヤーの位置とマーカー。
//! 内部ネットワークだけで届く。ここへは、DB に無いライブ情報だけを聞きに行く。

use std::time::Duration;

use reqwest::Url;

/// 本文の大きさの上限 (想定より大きいものは読まない)
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

pub struct Live {
    http: reqwest::Client,
    base: String,
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
        Self { http, base }
    }

    /// 200 の本文だけを返す。つながらない・遅い・200 でないときは None
    pub async fn get(&self, map: &str, file: &str, timeout: Duration) -> Option<Vec<u8>> {
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
