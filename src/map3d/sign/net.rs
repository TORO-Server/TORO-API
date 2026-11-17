//! 看板の URL をサーバーから読む (転送の行き先を調べる resolve と、CORS のないサイトの中継 proxy)。
//!
//! 看板の URL は誰でも書けるので、サーバーの中や家の中のネットワークを読ませないようにする (SSRF を防ぐ):
//! - https だけ。ユーザー情報つき・IP アドレスそのもの・怪しいホスト名・怪しいポートは使わない (`core::safe_url` と同じ決まり)
//! - 名前を引いたアドレスのどれかが、手元・家の中・共有アドレス・リンクローカル・マルチキャスト・IPv6 の固有ローカルなら読まない。
//!   確かめたアドレスにそのままつなぐ (つなぐときの名前解決で確かめるので、確かめた後に行き先をすり替えられない)
//! - 転送は自分で追い、行き先ごとに同じ確かめをする (5 回まで)
//! - Cookie・Referer・ブラウザーの頭書きは送らない
//!
//! 正本 (移管前) は TORO-WebMap の `server/lib/marumasign/net.ts`。

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use super::core::{safe_url, without_fragment};

pub const MAX_REDIRECTS: usize = 5;
pub const USER_AGENT: &str = "Mozilla/5.0 (MarumaSign; +https://3d-map.torosaba.net)";
/// 転送の行き先を調べる全体の時間
pub const RESOLVE_MAX: Duration = Duration::from_secs(30);

fn blocked_v4(a: u8, b: u8, c: u8) -> bool {
    a == 0 // 0.0.0.0/8 (どこでもない)
        || a == 10 // 家の中 10.0.0.0/8
        || a == 127 // 手元
        || (a == 100 && (b & 0xC0) == 64) // 共有アドレス (CGNAT) 100.64.0.0/10
        || (a == 169 && b == 254) // リンクローカル (クラウドのメタデータも)
        || (a == 172 && (b & 0xF0) == 16) // 家の中 172.16.0.0/12 (Docker のネットワークも)
        || (a == 192 && b == 168) // 家の中
        || (a == 192 && b == 0 && c == 0) // IETF の予約 192.0.0.0/24
        || (a == 192 && b == 0 && c == 2) // 説明用 192.0.2.0/24
        || (a == 192 && b == 88 && c == 99) // 6to4 の中継
        || (a == 198 && (b & 0xFE) == 18) // ベンチマーク用 198.18.0.0/15
        || (a == 198 && b == 51 && c == 100) // 説明用
        || (a == 203 && b == 0 && c == 113) // 説明用
        || a >= 224 // マルチキャスト、予約、ブロードキャスト
}

/// つないではいけないアドレスか (移管前の `isBlockedAddress` と同じ)
pub fn is_blocked_address(ip: &IpAddr) -> bool {
    let b = match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            return blocked_v4(o[0], o[1], o[2]);
        }
        IpAddr::V6(v6) => v6.octets(),
    };
    let zero = |from: usize, to: usize| b[from..to].iter().all(|x| *x == 0);
    // :: と ::1
    if zero(0, 15) && (b[15] == 0 || b[15] == 1) {
        return true;
    }
    // IPv4 を写したもの ::ffff:a.b.c.d と、古い書き方の ::a.b.c.d
    if zero(0, 10) && ((b[10] == 0xFF && b[11] == 0xFF) || (b[10] == 0 && b[11] == 0)) {
        return blocked_v4(b[12], b[13], b[14]);
    }
    // IPv4 を訳すもの ::ffff:0:a.b.c.d
    if zero(0, 8) && b[8] == 0xFF && b[9] == 0xFF && b[10] == 0 && b[11] == 0 {
        return blocked_v4(b[12], b[13], b[14]);
    }
    // NAT64 64:ff9b::/96 は埋め込まれた IPv4 で。手元の NAT64 64:ff9b:1::/48 は使わない
    if b[0] == 0 && b[1] == 0x64 && b[2] == 0xFF && b[3] == 0x9B {
        if b[4] == 0 && b[5] == 1 {
            return true;
        }
        return if zero(4, 12) {
            blocked_v4(b[12], b[13], b[14])
        } else {
            true
        };
    }
    // 6to4 2002::/16 は埋め込まれた IPv4 で
    if b[0] == 0x20 && b[1] == 0x02 {
        return blocked_v4(b[2], b[3], b[4]);
    }
    // Teredo 2001:0::/32、説明用 2001:db8::/32、ORCHID など 2001:10::/28, 2001:20::/28
    if b[0] == 0x20 && b[1] == 0x01 {
        if b[2] == 0 && b[3] == 0 {
            return true;
        }
        if b[2] == 0x0D && b[3] == 0xB8 {
            return true;
        }
        if b[2] == 0 && (0x10..=0x2F).contains(&b[3]) {
            return true;
        }
    }
    // 捨てる 100::/64
    if b[0] == 1 && zero(1, 8) {
        return true;
    }
    // 固有ローカル fc00::/7、リンクローカル fe80::/10、古いサイトローカル fec0::/10、マルチキャスト ff00::/8
    if (b[0] & 0xFE) == 0xFC {
        return true;
    }
    if b[0] == 0xFE && ((b[1] & 0xC0) == 0x80 || (b[1] & 0xC0) == 0xC0) {
        return true;
    }
    if b[0] == 0xFF {
        return true;
    }
    // 割り当てられているのは 2000::/3 だけ
    (b[0] & 0xE0) != 0x20
}

/// 名前を引いて、どれかがつないではいけないアドレスなら失敗にする (つなぐときの名前解決)
pub struct SafeResolver;

impl Resolve for SafeResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let host = name.as_str().to_string();
            let found: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if found.is_empty() {
                return Err("no address".into());
            }
            if found.iter().any(|a| is_blocked_address(&a.ip())) {
                return Err(format!("blocked address for {host}").into());
            }
            Ok(Box::new(found.into_iter()) as Addrs)
        })
    }
}

/// 読んでよい URL の形か (https、`core::safe_url` と同じ決まり)。だめなら None、よければ「#」を除いた URL。
/// 名前の末尾の「.」(DNS の完全な名前の書き方) は使わない
pub fn check_url(value: &str) -> Option<String> {
    let url = safe_url(value, true)?;
    let parsed = url::Url::parse(&url).ok()?;
    let host = parsed.host_str()?;
    if host.ends_with('.') || host.contains("..") {
        return None;
    }
    Some(without_fragment(&url).to_string())
}

/// 外向きの取得のしかた。`test_hosts` は試験のときだけ (本番では空): 決まった名前を、決まったアドレスへ http でつなぐ
#[derive(Clone, Default)]
pub struct Outbound {
    pub test_hosts: HashMap<String, SocketAddr>,
}

/// 外向きの HTTP クライアント。転送は追わない、環境のプロキシは使わない、名前の解決は `SafeResolver`
pub fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .dns_resolver(std::sync::Arc::new(SafeResolver))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(10))
        .pool_max_idle_per_host(4)
        .build()
        .expect("outbound client")
}

#[derive(Debug)]
pub struct UpstreamError {
    pub status: u16,
    pub message: String,
}

impl UpstreamError {
    pub fn new(message: &str, status: u16) -> Self {
        Self {
            status,
            message: message.to_string(),
        }
    }
}

/// 1 回の要求 (転送は追わない)
#[derive(Clone, Default)]
pub struct UpstreamRequest {
    pub head: bool,
    pub headers: Vec<(&'static str, String)>,
    /// 応答の頭が来るまでの時間
    pub timeout: Option<Duration>,
}

fn is_blocked_error(e: &reqwest::Error) -> bool {
    let mut source: Option<&dyn std::error::Error> = Some(e);
    while let Some(err) = source {
        if err.to_string().starts_with("blocked address") {
            return true;
        }
        source = err.source();
    }
    false
}

impl Outbound {
    /// 1 回だけ要求する。URL はもう `check_url` で確かめたもの
    pub async fn request_once(
        &self,
        client: &reqwest::Client,
        url: &str,
        req: &UpstreamRequest,
    ) -> Result<reqwest::Response, UpstreamError> {
        let parsed = url::Url::parse(url).map_err(|_| UpstreamError::new("blocked url", 403))?;
        let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
        let mut headers = HeaderMap::new();
        headers.insert("accept", HeaderValue::from_static("*/*"));
        headers.insert("accept-encoding", HeaderValue::from_static("identity"));
        for (name, value) in &req.headers {
            if let Ok(value) = HeaderValue::from_str(value) {
                headers.insert(HeaderName::from_static(name), value);
            }
        }
        let target = match self.test_hosts.get(&host) {
            Some(addr) => {
                let mut t = format!("http://{addr}{}", parsed.path());
                if let Some(q) = parsed.query() {
                    t.push('?');
                    t.push_str(q);
                }
                if let Ok(v) = HeaderValue::from_str(&host) {
                    headers.insert("host", v);
                }
                t
            }
            None => url.to_string(),
        };
        let builder = if req.head {
            client.head(&target)
        } else {
            client.get(&target)
        };
        let send = builder.headers(headers).send();
        let timeout = req.timeout.unwrap_or(Duration::from_secs(15));
        match tokio::time::timeout(timeout, send).await {
            Err(_) => Err(UpstreamError::new("upstream timeout", 504)),
            Ok(Err(e)) if is_blocked_error(&e) => Err(UpstreamError::new("blocked address", 403)),
            Ok(Err(e)) if e.is_timeout() => Err(UpstreamError::new("upstream timeout", 504)),
            Ok(Err(_)) => Err(UpstreamError::new("upstream error", 502)),
            Ok(Ok(res)) => Ok(res),
        }
    }

    /// 転送を追って読む (5 回まで。行き先ごとに URL とアドレスを確かめ直す)
    pub async fn follow(
        &self,
        client: &reqwest::Client,
        url: &str,
        req: &UpstreamRequest,
    ) -> Result<Followed, UpstreamError> {
        let mut current = check_url(url).ok_or_else(|| UpstreamError::new("blocked url", 403))?;
        let mut chain = vec![current.clone()];
        let mut hop = 0;
        loop {
            let res = self.request_once(client, &current, req).await?;
            let status = res.status().as_u16();
            if [301, 302, 303, 307, 308].contains(&status) {
                if hop >= MAX_REDIRECTS {
                    return Err(UpstreamError::new("too many redirects", 508));
                }
                hop += 1;
                let location = res
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| UpstreamError::new("redirect without location", 502))?
                    .to_string();
                let next = url::Url::parse(&current)
                    .and_then(|base| base.join(&location))
                    .map_err(|_| UpstreamError::new("bad redirect", 502))?;
                current = check_url(next.as_str())
                    .ok_or_else(|| UpstreamError::new("blocked redirect", 403))?;
                chain.push(current.clone());
                continue;
            }
            return Ok(Followed {
                res,
                url: current,
                chain,
            });
        }
    }

    /// 転送 (URL の短縮サービスなど) の行き先を調べる。HEAD で聞き、HEAD に答えないサイトには GET の先頭 1 バイトだけを聞く
    pub async fn resolve_redirects(
        &self,
        client: &reqwest::Client,
        url: &str,
        origin: &str,
        max: Duration,
    ) -> Result<Resolved, UpstreamError> {
        let work = async {
            let mut req = UpstreamRequest {
                head: true,
                headers: vec![("origin", origin.to_string())],
                timeout: Some(Duration::from_secs(10)),
            };
            let mut f = self.follow(client, url, &req).await?;
            let mut status = f.res.status().as_u16();
            if [400, 403, 404, 405, 501].contains(&status) {
                req.head = false;
                req.headers.push(("range", "bytes=0-0".to_string()));
                f = self.follow(client, url, &req).await?;
                status = f.res.status().as_u16();
            }
            let header = |name: &str| {
                f.res
                    .headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string()
            };
            let acao = header("access-control-allow-origin").trim().to_string();
            let ty = header("content-type")
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            Ok(Resolved {
                url: f.url.clone(),
                chain: f.chain.clone(),
                status,
                cors: acao == "*" || acao == origin,
                ty,
            })
        };
        match tokio::time::timeout(max, work).await {
            Ok(result) => result,
            Err(_) => Err(UpstreamError::new("upstream timeout", 504)),
        }
    }
}

/// 転送を追った結果
pub struct Followed {
    pub res: reqwest::Response,
    /// 最後の URL
    pub url: String,
    /// 通った URL (最初を含む)
    pub chain: Vec<String>,
}

/// 転送の行き先を調べた結果
pub struct Resolved {
    pub url: String,
    pub chain: Vec<String>,
    pub status: u16,
    /// 最後の URL が、ページに CORS を返すか
    pub cors: bool,
    /// 最後の応答の Content-Type (主な部分、小文字)
    pub ty: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked(text: &str) -> bool {
        is_blocked_address(&text.parse().unwrap())
    }

    #[test]
    fn blocks_private_and_reserved_v4() {
        for ip in [
            "0.0.0.0",
            "10.1.2.3",
            "127.0.0.1",
            "100.64.0.1",
            "100.127.255.255",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "192.0.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "198.19.255.255",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "255.255.255.255",
        ] {
            assert!(blocked(ip), "{ip}");
        }
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            "100.63.255.255",
            "100.128.0.1",
            "172.15.0.1",
            "172.32.0.1",
            "198.20.0.1",
        ] {
            assert!(!blocked(ip), "{ip}");
        }
    }

    #[test]
    fn blocks_private_and_reserved_v6() {
        for ip in [
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::7f00:1",
            "64:ff9b::a00:1",
            "64:ff9b:1::1",
            "2002:7f00:1::",
            "2001:0::1",
            "2001:db8::1",
            "fc00::1",
            "fd12::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "100::1",
            "4000::1",
        ] {
            assert!(blocked(ip), "{ip}");
        }
        for ip in [
            "2606:4700:4700::1111",
            "2001:4860:4860::8888",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
        ] {
            assert!(!blocked(ip), "{ip}");
        }
    }

    #[test]
    fn check_url_rules() {
        assert_eq!(
            check_url("https://example.com/a.png#x").as_deref(),
            Some("https://example.com/a.png")
        );
        assert!(check_url("http://example.com/a.png").is_none());
        assert!(check_url("https://127.0.0.1/a.png").is_none());
        assert!(check_url("https://user:pass@example.com/").is_none());
        assert!(check_url("https://example.com./a.png").is_none());
        assert!(check_url("https://exa..mple.com/a.png").is_none());
        assert!(check_url("").is_none());
    }

    #[tokio::test]
    async fn resolver_refuses_loopback_names() {
        let client = build_client();
        let out = Outbound::default();
        // localhost は名前を引くとループバックになる。つなぐ前に断る
        let result = out
            .request_once(&client, "https://localhost/", &UpstreamRequest::default())
            .await;
        assert_eq!(result.err().map(|e| e.status), Some(403));
    }
}
