//! Dynmap の地図をブラウザへすぐ届けるための中継 (WebSocket の配信ハブ)。
//! 移管前の `server/lib/live-hub.ts` と同じ仕様:
//!
//! - ワールドの更新 (描き直したタイル、マーカー、チャットなど): Dynmap が DB に 2 秒ごとに書き出すものを、
//!   ここで 1 秒ごとに 1 度だけ読み、`timestamp` が変わったときに、そのワールドを見ているブラウザ全員へ送る。
//!   読むのは `dynmap_config.json` にあるワールドだけ。何人が見ていても、ワールドごとに 1 秒に 1 度
//! - プレイヤーの位置: TORO-Core の dynmap-live が 0.25 秒ごとに送ってくる全員の本当の位置を、Dynmap が地図に出している
//!   人の分だけ、すぐ送る。Dynmap は隠す人 (透明化、スペクテイター、スニーク、影や地下の条件、protected-player-info、
//!   位置を送らないワールドなど) も `players` に残し、`world` を実在しない名前に、座標をダミーにして送る。
//!   だから `world` が実在のワールドで、dynmap-live の位置と同じワールドの人だけを通す
//! - 体力と防具: Dynmap は送らない設定 (sendhealth) や隠す人では 0 にする。0 のときは dynmap-live の値も送らない

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared, join_all};
use serde_json::Value;
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;

use super::js;
use super::store::MapStore;

const CONFIG_FILE: &str = "dynmap_config.json";
/// ワールドの一覧を読み直す間隔
const WORLDS_MAX_AGE: Duration = Duration::from_secs(30);
/// 知らない名前が来たときや、読めなかったときに、読み直すまでの間 (でたらめな名前で毎回 DB を読ませない)
const WORLDS_RETRY: Duration = Duration::from_secs(5);
/// ワールドの更新を DB から読む間隔
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// 1 人の送信待ちの数。詰まった人は外す (全員を待たせない)
const PEER_QUEUE: usize = 64;

/// 見ている 1 人 (WebSocket の 1 本)
#[derive(Clone)]
pub struct Peer {
    pub id: u64,
    tx: mpsc::Sender<String>,
    /// ハブが外したときに、接続を閉じさせる
    pub kicked: Arc<Notify>,
}

impl Peer {
    /// 送る文字列を受け取る側と一緒に作る
    pub fn new(id: u64) -> (Self, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel(PEER_QUEUE);
        (
            Self {
                id,
                tx,
                kicked: Arc::new(Notify::new()),
            },
            rx,
        )
    }
}

/// Dynmap が地図に出している人
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Visible {
    pub world: String,
    /// Dynmap が体力と防具を送っているか
    pub health: bool,
}

/// 地図に出している人 (出てきた順)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VisibleSet(Vec<(String, Visible)>);

impl VisibleSet {
    fn insert(&mut self, account: String, visible: Visible) {
        match self.0.iter_mut().find(|(a, _)| *a == account) {
            Some(entry) => entry.1 = visible,
            None => self.0.push((account, visible)),
        }
    }

    pub fn accounts(&self) -> Vec<&str> {
        self.0.iter().map(|(a, _)| a.as_str()).collect()
    }

    pub fn get(&self, account: &str) -> Option<&Visible> {
        self.0.iter().find(|(a, _)| a == account).map(|(_, v)| v)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// `dynmap_config.json` のワールドの名前。形が違えば None
pub fn world_names_of(raw: &str) -> Option<HashSet<String>> {
    let config: Value = serde_json::from_str(raw).ok()?;
    let worlds = config.as_object()?.get("worlds")?.as_array()?;
    Some(
        worlds
            .iter()
            .filter_map(|w| w.as_object()?.get("name")?.as_str().map(str::to_string))
            .collect(),
    )
}

/// Dynmap の更新の `players` から、地図に出している人 (`world` が実在のワールドの人) を取り出す
pub fn visible_players(players: Option<&Value>, worlds: &HashSet<String>) -> VisibleSet {
    let mut visible = VisibleSet::default();
    let Some(Value::Array(players)) = players else {
        return visible;
    };
    for p in players {
        let Some(p) = p.as_object() else { continue };
        let (Some(account), Some(world)) = (
            p.get("account").and_then(Value::as_str),
            p.get("world").and_then(Value::as_str),
        ) else {
            continue;
        };
        if !worlds.contains(world) {
            continue;
        }
        let health =
            js::value_number(p.get("health")) > 0.0 || js::value_number(p.get("armor")) > 0.0;
        visible.insert(
            account.to_string(),
            Visible {
                world: world.to_string(),
                health,
            },
        );
    }
    visible
}

/// dynmap-live の全員の位置のうち、Dynmap が地図に出している人の分
pub fn live_players_for(visible: &VisibleSet, positions: &HashMap<String, Value>) -> Vec<Value> {
    let mut list = Vec::new();
    for (account, v) in &visible.0 {
        let Some(p) = positions.get(account) else {
            continue;
        };
        if p.get("world").and_then(Value::as_str) != Some(v.world.as_str()) {
            continue;
        }
        if v.health {
            list.push(p.clone());
        } else {
            let mut hidden = p.clone();
            if let Some(map) = hidden.as_object_mut() {
                map.insert("health".into(), Value::from(0));
                map.insert("armor".into(), Value::from(0));
            }
            list.push(hidden);
        }
    }
    list
}

struct WorldState {
    /// 最後に読んだ更新の `timestamp` (読んだことがなければ 0)
    timestamp: Value,
    visible: VisibleSet,
    last_raw: Option<Arc<str>>,
    last_fetched_at: Option<Instant>,
}

struct Viewer {
    peer: Peer,
    world: String,
}

type Names = Arc<HashSet<String>>;
type ReadResult = Result<Option<Arc<str>>, ()>;
type SharedRead = Shared<BoxFuture<'static, ReadResult>>;

#[derive(Default)]
struct Inner {
    viewers: HashMap<u64, Viewer>,
    worlds: HashMap<String, WorldState>,
    reading: HashMap<String, SharedRead>,
    positions: HashMap<String, Value>,
    last_players: HashMap<u64, String>,
    known: Option<Names>,
    known_at: Option<Instant>,
    tried_at: Option<Instant>,
    loading_known: Option<Shared<BoxFuture<'static, ()>>>,
}

pub struct Hub {
    store: Arc<dyn MapStore>,
    inner: Mutex<Inner>,
    /// 1 秒ごとの読み取りを自動で回すか (試験では止めて、`tick` を直接呼ぶ)
    auto_poll: bool,
    polling: AtomicBool,
}

impl Hub {
    pub fn new(store: Arc<dyn MapStore>) -> Arc<Self> {
        Arc::new(Self {
            store,
            inner: Mutex::new(Inner::default()),
            auto_poll: true,
            polling: AtomicBool::new(false),
        })
    }

    /// 1 秒ごとの読み取りを自動で回さない (`tick` を呼んだときだけ読む)
    pub fn manual(store: Arc<dyn MapStore>) -> Arc<Self> {
        Arc::new(Self {
            store,
            inner: Mutex::new(Inner::default()),
            auto_poll: false,
            polling: AtomicBool::new(false),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 送れなかった人 (詰まった、閉じた) は外す
    fn send(inner: &mut Inner, id: u64, text: String) {
        let failed = match inner.viewers.get(&id) {
            None => return,
            Some(viewer) => {
                let failed = viewer.peer.tx.try_send(text).is_err();
                if failed {
                    viewer.peer.kicked.notify_one();
                }
                failed
            }
        };
        if failed {
            inner.viewers.remove(&id);
            inner.last_players.remove(&id);
        }
    }

    /// `dynmap_config.json` のワールドの名前。読めないときは前に読めたもの (1 度も読めていなければ None)
    /// `max_age` より古ければ読み直す
    pub async fn world_names(self: &Arc<Self>, max_age: Duration) -> Option<Names> {
        let loading = {
            let mut g = self.lock();
            let now = Instant::now();
            if let (Some(known), Some(at)) = (&g.known, g.known_at)
                && now.duration_since(at) < max_age
            {
                return Some(known.clone());
            }
            let may_try = g
                .tried_at
                .is_none_or(|t| now.duration_since(t) >= WORLDS_RETRY);
            if g.loading_known.is_none() && may_try {
                g.tried_at = Some(now);
                let hub = self.clone();
                let load = async move {
                    let raw = hub.store.standalone_file(CONFIG_FILE).await;
                    let mut g = hub.lock();
                    if let Ok(Some(raw)) = raw
                        && let Some(names) = world_names_of(&raw)
                    {
                        g.known = Some(Arc::new(names));
                        g.known_at = Some(Instant::now());
                    }
                    g.loading_known = None;
                }
                .boxed()
                .shared();
                g.loading_known = Some(load);
            }
            g.loading_known.clone()
        };
        if let Some(loading) = loading {
            loading.await;
        }
        self.lock().known.clone()
    }

    /// world を確かめるための一覧。一覧にない名前なら、増えたばかりかもしれないので読み直す
    /// (読み直すのは少し間を置いてから)
    pub async fn names_for(self: &Arc<Self>, world: &str) -> Option<Names> {
        let names = self.world_names(WORLDS_MAX_AGE).await?;
        if names.contains(world) {
            Some(names)
        } else {
            self.world_names(WORLDS_RETRY).await
        }
    }

    /// 同じワールドを同時に読まない
    fn read(self: &Arc<Self>, world: &str) -> SharedRead {
        let mut g = self.lock();
        if let Some(reading) = g.reading.get(world) {
            return reading.clone();
        }
        let hub = self.clone();
        let key = world.to_string();
        let reading = async move {
            let raw = hub
                .store
                .standalone_file(&format!("dynmap_{key}.json"))
                .await;
            hub.lock().reading.remove(&key);
            match raw {
                Ok(raw) => Ok(raw.map(Arc::from)),
                Err(_) => Err(()),
            }
        }
        .boxed()
        .shared();
        g.reading.insert(world.to_string(), reading.clone());
        reading
    }

    /// DB の更新を読み、変わっていれば見ている人へ送る (`only` があれば、その人だけに必ず送る)
    async fn poll(self: &Arc<Self>, world: &str, names: &Names, only: Option<u64>) {
        let reusable = {
            let g = self.lock();
            g.worlds
                .get(world)
                .and_then(|w| match (&w.last_raw, w.last_fetched_at) {
                    (Some(raw), Some(at)) if only.is_some() && at.elapsed() < POLL_INTERVAL => {
                        Some(raw.clone())
                    }
                    _ => None,
                })
        };
        let raw = match reusable {
            Some(raw) => raw,
            None => match self.read(world).await {
                Ok(Some(raw)) => raw,
                _ => return,
            },
        };
        let Ok(Value::Object(parsed)) = serde_json::from_str::<Value>(&raw) else {
            return;
        };
        let timestamp = parsed.get("timestamp").cloned().unwrap_or(Value::Null);
        let visible = visible_players(parsed.get("players"), names);

        let mut g = self.lock();
        let state = g
            .worlds
            .entry(world.to_string())
            .or_insert_with(|| WorldState {
                timestamp: Value::from(0),
                visible: VisibleSet::default(),
                last_raw: None,
                last_fetched_at: None,
            });
        let changed = !js::same_value(&state.timestamp, &timestamp);
        state.timestamp = timestamp;
        state.visible = visible;
        state.last_raw = Some(raw.clone());
        state.last_fetched_at = Some(Instant::now());
        if !changed && only.is_none() {
            return;
        }
        let text = format!(
            r#"{{"type":"update","world":{},"update":{}}}"#,
            serde_json::to_string(world).unwrap_or_default(),
            raw
        );
        let targets: Vec<u64> = g
            .viewers
            .iter()
            .filter(|(id, v)| v.world == world && only.is_none_or(|o| o == **id))
            .map(|(id, _)| *id)
            .collect();
        for id in targets {
            Self::send(&mut g, id, text.clone());
        }
    }

    /// 1 秒ごと: 見ている人のいるワールドを読む
    pub async fn tick(self: &Arc<Self>) {
        let watched: HashSet<String> = self
            .lock()
            .viewers
            .values()
            .map(|v| v.world.clone())
            .collect();
        if watched.is_empty() {
            return;
        }
        let Some(names) = self.world_names(WORLDS_MAX_AGE).await else {
            return;
        };
        let mut polls = Vec::new();
        for world in &watched {
            if names.contains(world) {
                let hub = self.clone();
                let world = world.clone();
                let names = names.clone();
                polls.push(async move { hub.poll(&world, &names, None).await });
            } else {
                self.drop_viewers(world);
            }
        }
        join_all(polls).await;
    }

    fn drop_viewers(&self, world: &str) {
        let mut g = self.lock();
        let ids: Vec<u64> = g
            .viewers
            .iter()
            .filter(|(_, v)| v.world == world)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            g.viewers.remove(&id);
            g.last_players.remove(&id);
        }
    }

    /// 見ている人がいるあいだ、1 秒ごとに読む。いなくなったら止まる
    fn start_polling(self: &Arc<Self>) {
        if !self.auto_poll || self.polling.swap(true, Ordering::SeqCst) {
            return;
        }
        let hub = self.clone();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval_at(Instant::now() + POLL_INTERVAL, POLL_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                {
                    // 空かどうかの確認と止める印を、同じロックの中で行う (見始めた人を取りこぼさない)
                    let g = hub.lock();
                    if g.viewers.is_empty() {
                        hub.polling.store(false, Ordering::SeqCst);
                        return;
                    }
                }
                // 遅い DB で、次の 1 秒を待たせない (読み取りは同じワールドなら 1 本に束ねられる)
                let ticking = hub.clone();
                tokio::spawn(async move { ticking.tick().await });
            }
        });
    }

    /// 見るワールドを知らせてきた。`dynmap_config.json` にない名前は断る (DB を読まない)
    pub async fn subscribe(self: &Arc<Self>, peer: &Peer, world: &str) {
        {
            let mut g = self.lock();
            g.viewers.insert(
                peer.id,
                Viewer {
                    peer: peer.clone(),
                    world: world.to_string(),
                },
            );
            g.last_players.remove(&peer.id);
        }
        self.start_polling();
        let names = self.names_for(world).await;
        // 調べているあいだに、閉じた・別のワールドに変えたときは何もしない
        let still = self
            .lock()
            .viewers
            .get(&peer.id)
            .is_some_and(|v| v.world == world);
        if !still {
            return;
        }
        let Some(names) = names else {
            return; // 一覧が読めるまで、1 秒ごとに読み直す (読めたら tick が送る)
        };
        if !names.contains(world) {
            self.leave(peer.id);
            return;
        }
        // 見始めたときは、今の状態をすぐ送る
        self.poll(world, &names, Some(peer.id)).await;
    }

    pub fn leave(&self, id: u64) {
        let mut g = self.lock();
        g.viewers.remove(&id);
        g.last_players.remove(&id);
    }

    /// dynmap-live から届いた全員の位置
    pub fn set_positions(&self, players: &[Value]) {
        let mut positions = HashMap::new();
        for p in players {
            if let (Some(account), Some(_)) = (
                p.get("account").and_then(Value::as_str),
                p.get("world").and_then(Value::as_str),
            ) {
                positions.insert(account.to_string(), p.clone());
            }
        }
        let mut g = self.lock();
        g.positions = positions;
        // JSON はワールドごとに 1 度だけ作る
        let mut texts: HashMap<String, String> = HashMap::new();
        let ids: Vec<(u64, String)> = g
            .viewers
            .iter()
            .map(|(id, v)| (*id, v.world.clone()))
            .collect();
        for (id, world) in ids {
            let text = match texts.get(&world) {
                Some(text) => text.clone(),
                None => {
                    let Some(state) = g.worlds.get(&world) else {
                        continue;
                    };
                    let list = live_players_for(&state.visible, &g.positions);
                    let text = format!(
                        r#"{{"type":"players","players":{}}}"#,
                        js::stringify(&Value::Array(list))
                    );
                    texts.insert(world.clone(), text.clone());
                    text
                }
            };
            // 前に送ったものと同じなら送らない
            if g.last_players.get(&id) == Some(&text) {
                continue;
            }
            g.last_players.insert(id, text.clone());
            Self::send(&mut g, id, text);
        }
    }

    /// 見ている人の数 (試験と運用の確認用)
    pub fn viewer_count(&self) -> usize {
        self.lock().viewers.len()
    }
}
