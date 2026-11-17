//! BlueMap のライブ情報 (プレイヤーの位置、描き直したタイル、マーカーの変更) の中継。
//!
//! BlueMap 内蔵の Web サーバー の `live/sse` を、見ている地図ごとに **1 本だけ**つなぎ、
//! 届いたイベントを、その地図を見ている WebSocket (`/map3d/v1/ws`) と SSE (`/map3d/v1/maps/{地図}/live/sse`) の
//! 閲覧者全員へ配る。見る人がいなくなったら、BlueMap とのつなぎをやめる。切れたら、間を置いてつなぎ直す。
//!
//! つながっているかは `open` (`watch`) で知らせる。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Notify, broadcast, watch};

use super::live::live_url;

/// 配るイベントの種類 (これ以外は捨てる)
const EVENTS: [&str; 3] = ["player", "tile", "marker"];
const MAX_RETRY: Duration = Duration::from_secs(15);
/// 閲覧者ごとの、読み残しの数の上限 (超えた分は読み飛ばす。詰まった人が全員を待たせない)
const QUEUE: usize = 256;

/// BlueMap が送ってきた 1 つのイベント
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpEvent {
    /// `player`、`tile`、`marker`
    pub event: String,
    /// `data:` の行を改行でつないだもの (BlueMap が送った JSON の文字列のまま)
    pub data: String,
}

/// Server-Sent Events (text/event-stream) の読み取り
#[derive(Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    event: String,
    data: Vec<String>,
}

impl SseParser {
    /// 届いたバイトを渡す。そろったイベントを返す。途中で切れた行は次の `push` まで持つ
    pub fn push(&mut self, chunk: &[u8]) -> Vec<UpEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut start = 0;
        loop {
            let mut found = None;
            for i in start..self.buffer.len() {
                match self.buffer[i] {
                    b'\n' => {
                        found = Some((i, i + 1));
                        break;
                    }
                    b'\r' => {
                        // \r\n の \n がまだ届いていないかもしれないので、次の文字を待つ
                        if i + 1 >= self.buffer.len() {
                            break;
                        }
                        let next = if self.buffer[i + 1] == b'\n' {
                            i + 2
                        } else {
                            i + 1
                        };
                        found = Some((i, next));
                        break;
                    }
                    _ => {}
                }
            }
            let Some((end, next)) = found else { break };
            let line = String::from_utf8_lossy(&self.buffer[start..end]).into_owned();
            if let Some(event) = self.line(&line) {
                out.push(event);
            }
            start = next;
        }
        self.buffer.drain(..start);
        out
    }

    fn line(&mut self, line: &str) -> Option<UpEvent> {
        if line.is_empty() {
            let data = std::mem::take(&mut self.data);
            let event = std::mem::take(&mut self.event);
            return (!data.is_empty()).then(|| UpEvent {
                event: if event.is_empty() {
                    "message".to_string()
                } else {
                    event
                },
                data: data.join("\n"),
            });
        }
        // コメント (: で始まる行。つなぎっぱなしの合図に使われる)
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = value.to_string(),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
        None
    }
}

struct Upstream {
    tx: broadcast::Sender<Arc<UpEvent>>,
    open: watch::Sender<bool>,
    /// 見ている人の数
    subs: usize,
    stop: Arc<Notify>,
}

pub struct BlueHub {
    base: String,
    /// 全体の時間切れのないクライアント (SSE は何時間もつなぎっぱなしになる)
    client: reqwest::Client,
    upstreams: Mutex<HashMap<String, Upstream>>,
}

/// 1 つの地図の閲覧。手放すと (Drop)、見る人が 0 になったときに BlueMap とのつなぎをやめる
pub struct Subscription {
    hub: Arc<BlueHub>,
    pub map: String,
    pub events: broadcast::Receiver<Arc<UpEvent>>,
    pub open: watch::Receiver<bool>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.hub.release(&self.map);
    }
}

impl BlueHub {
    pub fn new(base: String) -> Arc<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .tcp_keepalive(Duration::from_secs(30))
            .user_agent("TORO-API")
            .build()
            .unwrap_or_default();
        Arc::new(Self {
            base,
            client,
            upstreams: Mutex::new(HashMap::new()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Upstream>> {
        self.upstreams.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// その地図を見始める。最初の 1 人のときに、BlueMap へつなぎに行く
    pub fn subscribe(self: &Arc<Self>, map: &str) -> Subscription {
        let mut upstreams = self.lock();
        if !upstreams.contains_key(map) {
            let (tx, _) = broadcast::channel(QUEUE);
            let (open, _) = watch::channel(false);
            let stop = Arc::new(Notify::new());
            upstreams.insert(
                map.to_string(),
                Upstream {
                    tx: tx.clone(),
                    open: open.clone(),
                    subs: 0,
                    stop: stop.clone(),
                },
            );
            let url = live_url(&self.base, map, "sse");
            tokio::spawn(run(self.client.clone(), url, tx, open, stop));
        }
        let up = upstreams.get_mut(map).expect("inserted above");
        up.subs += 1;
        Subscription {
            hub: self.clone(),
            map: map.to_string(),
            events: up.tx.subscribe(),
            open: up.open.subscribe(),
        }
    }

    fn release(&self, map: &str) {
        let mut upstreams = self.lock();
        let Some(up) = upstreams.get_mut(map) else {
            return;
        };
        up.subs = up.subs.saturating_sub(1);
        if up.subs == 0 {
            up.stop.notify_one();
            upstreams.remove(map);
        }
    }

    /// BlueMap につないでいる地図の数 (試験と運用の確認用)
    pub fn upstream_count(&self) -> usize {
        self.lock().len()
    }
}

/// BlueMap の `live/sse` を読み続ける。切れたら間を置いてつなぎ直す。`stop` が来るまで
async fn run(
    client: reqwest::Client,
    url: Option<reqwest::Url>,
    tx: broadcast::Sender<Arc<UpEvent>>,
    open: watch::Sender<bool>,
    stop: Arc<Notify>,
) {
    let Some(url) = url else { return };
    let mut retry = Duration::from_secs(1);
    loop {
        let request = client
            .get(url.clone())
            .header("accept", "text/event-stream")
            .send();
        let sent = tokio::select! {
            () = stop.notified() => return,
            sent = request => sent,
        };
        if let Ok(mut res) = sent
            && res.status().is_success()
        {
            open.send_replace(true);
            retry = Duration::from_secs(1);
            let mut parser = SseParser::default();
            loop {
                let chunk = tokio::select! {
                    () = stop.notified() => return,
                    chunk = res.chunk() => chunk,
                };
                match chunk {
                    Ok(Some(bytes)) => {
                        for event in parser.push(&bytes) {
                            if EVENTS.contains(&event.event.as_str()) {
                                // 見ている人がいなくても (すれ違いで) 失敗するだけ
                                let _ = tx.send(Arc::new(event));
                            }
                        }
                    }
                    _ => break,
                }
            }
        }
        // Minecraft サーバーの再起動中など。つなぎ直す
        open.send_replace(false);
        tokio::select! {
            () = stop.notified() => return,
            () = tokio::time::sleep(retry) => {}
        }
        retry = (retry * 2).min(MAX_RETRY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(parts: &[&[u8]]) -> Vec<UpEvent> {
        let mut parser = SseParser::default();
        parts.iter().flat_map(|p| parser.push(p)).collect()
    }

    fn ev(event: &str, data: &str) -> UpEvent {
        UpEvent {
            event: event.into(),
            data: data.into(),
        }
    }

    #[test]
    fn parses_events() {
        assert_eq!(
            events(&[b"event: tile\ndata: {\"a\":1}\n\n"]),
            vec![ev("tile", "{\"a\":1}")]
        );
        // 名前がなければ message、複数の data は改行でつなぐ
        assert_eq!(
            events(&[b"data: a\ndata: b\n\n"]),
            vec![ev("message", "a\nb")]
        );
        // コメントと、data のないイベントは捨てる
        assert_eq!(events(&[b": keep-alive\n\nevent: x\n\n"]), vec![]);
        // \r\n と \r
        assert_eq!(
            events(&[b"event: p\r\ndata: 1\r\n\r\n"]),
            vec![ev("p", "1")]
        );
        assert_eq!(events(&[b"event: p\rdata: 1\r\r: c\n"]), vec![ev("p", "1")]);
    }

    #[test]
    fn handles_split_chunks_and_multibyte() {
        let whole = "event: marker\ndata: 日本語\n\n".as_bytes();
        for cut in 1..whole.len() {
            let got = events(&[&whole[..cut], &whole[cut..]]);
            assert_eq!(got, vec![ev("marker", "日本語")], "cut at {cut}");
        }
        // \r の直後で切れても、\r\n を 1 つの改行として扱う
        assert_eq!(
            events(&[b"data: 1\r", b"\n\r", b"\n"]),
            vec![ev("message", "1")]
        );
    }

    #[test]
    fn field_without_colon_and_leading_space() {
        assert_eq!(events(&[b"data\n\n"]), vec![ev("message", "")]);
        assert_eq!(events(&[b"data:  x\n\n"]), vec![ev("message", " x")]);
        assert_eq!(events(&[b"data:x\n\n"]), vec![ev("message", "x")]);
    }
}
