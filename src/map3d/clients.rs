//! いまつながっているプレイヤーの IP。入口のプロキシ が、人の出入りがあったとき (イベント駆動) に
//! `/live/clients` へ全員分を送ってくる。入口のプロキシが見ている接続元なので、Paper の再起動中に待っている人も含む。
//!
//! どう確かめるか (ログアウト後の猶予、知らせが途絶えたときの扱い) はここで決める。
//! **IP はメモリにだけ置き、ログにも DB にも残さない。** `/_assets/` を読んでよいプレイヤーかの判定にだけ使う。

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 知らせが途絶えたら (入口のプロキシが止まった、など)、最後の一覧をこの時間まで信じる
const SNAPSHOT_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// 1 度に受ける IP の数の上限
pub const MAX_IPS: usize = 2000;
/// IP 1 つの文字数の上限
pub const MAX_IP_LEN: usize = 64;

#[derive(Default)]
struct State {
    online: HashSet<Vec<u8>>,
    snapshot_at: Option<Instant>,
    left: HashMap<Vec<u8>, Instant>,
}

pub struct Clients {
    grace: Duration,
    state: Mutex<State>,
}

/// IP アドレスの生のバイト (IPv4 は 4、IPv6 は 16。IPv4 を写した IPv6 は IPv4 の 4)。同じ人の書き方の違いをそろえる
pub fn ip_bytes(ip: &str) -> Option<Vec<u8>> {
    let trimmed = ip.trim();
    // 先頭の [ と末尾の ] を 1 つずつ取る
    let unbracketed = trimmed.strip_prefix('[').unwrap_or(trimmed);
    let unbracketed = unbracketed.strip_suffix(']').unwrap_or(unbracketed);
    // スコープ (%eth0) は取る
    let addr = unbracketed.split('%').next().unwrap_or("");
    match addr.parse::<IpAddr>().ok()? {
        IpAddr::V4(v4) => Some(v4.octets().to_vec()),
        IpAddr::V6(v6) => Some(match mapped(&v6) {
            Some(v4) => v4.to_vec(),
            None => v6.octets().to_vec(),
        }),
    }
}

fn mapped(v6: &Ipv6Addr) -> Option<[u8; 4]> {
    let o = v6.octets();
    (o[..10].iter().all(|b| *b == 0) && o[10] == 0xff && o[11] == 0xff)
        .then(|| [o[12], o[13], o[14], o[15]])
}

/// `is_player()` 照合用のキー (IPv4 と `::ffff:` は 4 バイト、IPv6 は `/64` の先頭 8 バイト)。
/// SLAAC 一時アドレスで下位 64 ビットが変わっても同じ人とみなす (`assets::limit_key` の `/64` 集約と揃える)
pub fn ip_key(ip: &str) -> Option<Vec<u8>> {
    let mut b = ip_bytes(ip)?;
    if b.len() == 16 {
        b.truncate(8);
    }
    Some(b)
}

impl Clients {
    pub fn new(grace_minutes: u64) -> Arc<Self> {
        Arc::new(Self {
            grace: Duration::from_secs(grace_minutes * 60),
            state: Mutex::new(State::default()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 入口のプロキシからの全員分の一覧で入れ替える。いなくなった IP は、いなくなった時刻を覚えておく
    pub fn update(&self, ips: &[String]) {
        self.update_at(ips, Instant::now());
    }

    pub fn update_at(&self, ips: &[String], now: Instant) {
        let next: HashSet<Vec<u8>> = ips.iter().filter_map(|ip| ip_key(ip)).collect();
        let mut s = self.lock();
        let gone: Vec<Vec<u8>> = s.online.difference(&next).cloned().collect();
        for key in gone {
            s.left.insert(key, now);
        }
        for key in &next {
            s.left.remove(key);
        }
        let grace = self.grace;
        s.left.retain(|_, at| now.duration_since(*at) <= grace);
        s.online = next;
        s.snapshot_at = Some(now);
    }

    /// いまつながっている (またはログアウトしてから猶予の時間以内の) プレイヤーの IP か
    pub fn is_player(&self, ip: &str) -> bool {
        self.is_player_at(ip, Instant::now())
    }

    pub fn is_player_at(&self, ip: &str, now: Instant) -> bool {
        let Some(key) = ip_key(ip) else {
            return false;
        };
        let s = self.lock();
        if s.online.contains(&key) {
            return s
                .snapshot_at
                .is_some_and(|at| now.duration_since(at) < SNAPSHOT_MAX_AGE + self.grace);
        }
        s.left
            .get(&key)
            .is_some_and(|at| now.duration_since(*at) < self.grace)
    }

    /// 覚えている IP の数 (試験用。IP そのものは返さない)
    pub fn online_count(&self) -> usize {
        self.lock().online.len()
    }
}

/// `{"ips": [...]}` の本文から IP の一覧を取り出す。形が違えば None
pub fn parse_ips(body: &[u8]) -> Option<Vec<String>> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let list = value.get("ips")?.as_array()?;
    if list.len() > MAX_IPS {
        return None;
    }
    list.iter()
        .map(|ip| {
            ip.as_str()
                .filter(|s| s.chars().count() <= MAX_IP_LEN)
                .map(str::to_string)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ips(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn same_person_written_differently() {
        assert_eq!(ip_bytes("203.0.113.5"), ip_bytes("::ffff:203.0.113.5"));
        assert_eq!(ip_bytes("203.0.113.5"), ip_bytes(" [::ffff:203.0.113.5] "));
        assert_eq!(ip_bytes("fe80::1%eth0"), ip_bytes("fe80:0:0:0:0:0:0:1"));
        assert_eq!(ip_bytes("203.0.113.5").map(|b| b.len()), Some(4));
        assert_eq!(ip_bytes("2001:db8::1").map(|b| b.len()), Some(16));
        assert_eq!(ip_bytes("not an ip"), None);
        assert_eq!(ip_bytes(""), None);
        assert_eq!(ip_bytes("999.1.1.1"), None);
    }

    #[test]
    fn online_then_grace_then_forgotten() {
        let clients = Clients::new(10);
        let t0 = Instant::now();
        assert!(!clients.is_player_at("203.0.113.5", t0));
        clients.update_at(&ips(&["203.0.113.5", "2001:db8::1"]), t0);
        assert!(clients.is_player_at("203.0.113.5", t0));
        assert!(clients.is_player_at("::ffff:203.0.113.5", t0));
        assert!(clients.is_player_at("2001:db8::1", t0));
        // いなくなった人は、猶予の間だけ読める
        let t1 = t0 + Duration::from_secs(60);
        clients.update_at(&ips(&["2001:db8::1"]), t1);
        assert!(clients.is_player_at("203.0.113.5", t1 + Duration::from_secs(9 * 60)));
        assert!(!clients.is_player_at("203.0.113.5", t1 + Duration::from_secs(10 * 60)));
        // 戻ってきた人は、また読める
        clients.update_at(&ips(&["203.0.113.5"]), t1 + Duration::from_secs(5));
        assert!(clients.is_player_at("203.0.113.5", t1 + Duration::from_secs(3600)));
    }

    #[test]
    fn stale_snapshot_is_not_trusted_after_a_day() {
        let clients = Clients::new(10);
        let t0 = Instant::now();
        clients.update_at(&ips(&["203.0.113.5"]), t0);
        assert!(clients.is_player_at("203.0.113.5", t0 + Duration::from_secs(24 * 3600)));
        assert!(!clients.is_player_at("203.0.113.5", t0 + Duration::from_secs(24 * 3600 + 600)));
    }

    #[test]
    fn ipv6_matches_by_64_prefix_while_ipv4_stays_exact() {
        let clients = Clients::new(10);
        let t0 = Instant::now();
        clients.update_at(&ips(&["203.0.113.5", "2001:db8:1:2:aaaa::1"]), t0);

        // 同じ /64 内で下位 64 ビット (一時アドレス等) が変わっても一致する
        assert!(clients.is_player_at("2001:db8:1:2:bbbb::2", t0));
        assert!(clients.is_player_at("[2001:db8:1:2:cccc::3]", t0));
        assert!(clients.is_player_at("2001:db8:1:2:cccc::3%eth0", t0));
        // 別の /64 は一致しない
        assert!(!clients.is_player_at("2001:db8:1:3::1", t0));

        // IPv4 (::ffff: 含む) は従来どおり /32 完全一致
        assert!(clients.is_player_at("203.0.113.5", t0));
        assert!(clients.is_player_at("::ffff:203.0.113.5", t0));
        assert!(!clients.is_player_at("203.0.113.6", t0));
        assert!(!clients.is_player_at("::ffff:203.0.113.6", t0));

        // 猶予期間中も /64 集約で一致し、猶予経過後は失効する
        let t1 = t0 + Duration::from_secs(60);
        clients.update_at(&[], t1);
        assert!(clients.is_player_at("2001:db8:1:2:dddd::4", t1 + Duration::from_secs(9 * 60)));
        assert!(!clients.is_player_at("2001:db8:1:2:dddd::4", t1 + Duration::from_secs(10 * 60)));
    }

    #[test]
    fn body_limits() {
        assert_eq!(
            parse_ips(br#"{"ips":["1.2.3.4","::1"]}"#),
            Some(ips(&["1.2.3.4", "::1"]))
        );
        assert_eq!(parse_ips(br#"{"ips":[]}"#), Some(vec![]));
        assert_eq!(parse_ips(br#"{"ips":"1.2.3.4"}"#), None);
        assert_eq!(parse_ips(br#"{"ips":[1]}"#), None);
        assert_eq!(parse_ips(b"not json"), None);
        assert_eq!(parse_ips(b"{}"), None);
        let long = format!(r#"{{"ips":["{}"]}}"#, "1".repeat(65));
        assert_eq!(parse_ips(long.as_bytes()), None);
        let many = format!(r#"{{"ips":[{}]}}"#, vec![r#""1.2.3.4""#; 2001].join(","));
        assert_eq!(parse_ips(many.as_bytes()), None);
        let ok = format!(r#"{{"ips":[{}]}}"#, vec![r#""1.2.3.4""#; 2000].join(","));
        assert!(parse_ips(ok.as_bytes()).is_some());
    }
}
