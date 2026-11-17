//! 配信ハブ: 隠れている人の位置を送らないこと、知らないワールドで DB を読まないこと、
//! ポーリングの間隔、重複送信の抑止。移管前の地図サーバーの `tests/live-hub.test.ts` を移したもの

mod common;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use common::fake_map::*;
use serde_json::{Value, json};
use tokio::sync::mpsc::Receiver;
use toro_api::map::hub::{Hub, Peer, live_players_for, visible_players, world_names_of};

const CONFIG: &str = r#"{"worlds":[{"name":"main"},{"name":"nether"}]}"#;
/// Dynmap が隠す人に使う名前 (ClientUpdateComponent)
const HIDDEN_WORLD: &str = "-some-other-bogus-world-";
const HIDDEN_PLAYER_WORLD: &str = "-hidden-player-";

fn live(account: &str, world: &str) -> Value {
    json!({ "account": account, "world": world, "x": 1.5, "y": 64, "z": -2.5, "health": 18, "armor": 7 })
}

fn names(list: &[&str]) -> HashSet<String> {
    list.iter().map(|s| s.to_string()).collect()
}

struct Viewer {
    peer: Peer,
    rx: Receiver<String>,
}

impl Viewer {
    fn new(id: u64) -> Self {
        let (peer, rx) = Peer::new(id);
        Self { peer, rx }
    }

    /// 届いているものを全部取り出す
    fn drain(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(text) = self.rx.try_recv() {
            out.push(text);
        }
        out
    }

    fn types(&mut self) -> Vec<String> {
        self.drain()
            .iter()
            .map(|t| {
                serde_json::from_str::<Value>(t).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }
}

fn last_players(messages: &[String]) -> Option<Vec<String>> {
    let text = messages
        .iter()
        .rev()
        .find(|m| m.starts_with(r#"{"type":"players""#))?;
    let v: Value = serde_json::from_str(text).unwrap();
    Some(
        v["players"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["account"].as_str().unwrap().to_string())
            .collect(),
    )
}

fn setup(files: &[(&str, String)]) -> (Arc<Hub>, Arc<FakeStore>) {
    let store = Arc::new(FakeStore::default());
    for (name, content) in files {
        store.set_file(name, content);
    }
    (Hub::manual(store.clone()), store)
}

#[test]
fn world_names_come_from_the_config() {
    let mut list: Vec<String> = world_names_of(CONFIG).unwrap().into_iter().collect();
    list.sort();
    assert_eq!(list, ["main", "nether"]);
    assert!(world_names_of("not json").is_none());
    assert!(world_names_of(r#"{"worlds":1}"#).is_none());
    assert!(world_names_of("null").is_none());
    let odd = world_names_of(r#"{"worlds":[{"name":1},null,{"name":"a"}]}"#).unwrap();
    assert_eq!(odd, names(&["a"]));
}

#[test]
fn players_hidden_by_dynmap_are_not_visible() {
    let worlds = names(&["main", "nether"]);
    let players: Value = json!([
        { "account": "shown", "world": "main", "health": 20, "armor": 0 },
        { "account": "elsewhere", "world": "nether", "health": 0, "armor": 0 },
        { "account": "hidden", "world": HIDDEN_WORLD, "health": 0, "armor": 0 },
        { "account": "hiddenplayer", "world": HIDDEN_PLAYER_WORLD, "health": 0, "armor": 0 },
        { "account": "unknownworld", "world": "private", "health": 20, "armor": 20 },
        { "world": "main" }, null, "x"
    ]);
    let visible = visible_players(Some(&players), &worlds);
    assert_eq!(visible.accounts(), ["shown", "elsewhere"]);
    assert!(visible.get("shown").unwrap().health);
    assert!(!visible.get("elsewhere").unwrap().health);
    assert!(visible_players(None, &worlds).is_empty());
    assert!(visible_players(Some(&json!("x")), &worlds).is_empty());
}

#[test]
fn health_flag_follows_javascript_number_conversion() {
    let worlds = names(&["main"]);
    let players = json!([
        { "account": "a", "world": "main", "health": "5", "armor": 0 },
        { "account": "b", "world": "main", "health": null, "armor": null },
        { "account": "c", "world": "main", "armor": 1 },
        { "account": "d", "world": "main", "health": "abc" },
    ]);
    let visible = visible_players(Some(&players), &worlds);
    assert!(visible.get("a").unwrap().health);
    assert!(!visible.get("b").unwrap().health);
    assert!(visible.get("c").unwrap().health);
    assert!(!visible.get("d").unwrap().health);
}

#[test]
fn positions_only_for_the_same_world_and_health_only_when_dynmap_sends_it() {
    let worlds = names(&["main"]);
    let players = json!([
        { "account": "a", "world": "main", "health": 20, "armor": 0 },
        { "account": "b", "world": "main", "health": 0, "armor": 0 },
        { "account": "moved", "world": "main", "health": 20, "armor": 0 },
        { "account": "gone", "world": "main", "health": 20, "armor": 0 },
    ]);
    let visible = visible_players(Some(&players), &worlds);
    let positions = [
        live("a", "main"),
        live("b", "main"),
        live("moved", "private"),
        live("off", "main"),
    ]
    .into_iter()
    .map(|p| (p["account"].as_str().unwrap().to_string(), p))
    .collect();
    let list = live_players_for(&visible, &positions);
    let accounts: Vec<&str> = list
        .iter()
        .map(|p| p["account"].as_str().unwrap())
        .collect();
    assert_eq!(accounts, ["a", "b"]);
    assert_eq!(list[0]["health"], 18);
    assert_eq!(list[0]["armor"], 7);
    assert_eq!(list[1]["health"], 0);
    assert_eq!(list[1]["armor"], 0);
    assert_eq!(list[1]["x"], 1.5);
}

#[tokio::test(start_paused = true)]
async fn positions_of_hidden_players_never_reach_viewers() {
    let (hub, _) = setup(&[
        ("dynmap_config.json", CONFIG.to_string()),
        (
            "dynmap_main.json",
            update_json(
                1,
                &json!([
                    { "account": "shown", "world": "main", "health": 20, "armor": 5 },
                    { "account": "nohealth", "world": "nether", "health": 0, "armor": 0 },
                    { "account": "hidden", "world": HIDDEN_WORLD, "x": 0, "y": 64, "z": 0, "health": 0, "armor": 0 },
                ])
                .to_string(),
            ),
        ),
    ]);
    let mut viewer = Viewer::new(1);
    hub.subscribe(&viewer.peer, "main").await;
    let first = viewer.drain();
    assert!(first[0].starts_with(r#"{"type":"update","world":"main","update":"#));
    // dynmap-live は全員の本当の位置を送ってくる
    hub.set_positions(&[
        live("shown", "main"),
        live("nohealth", "nether"),
        live("hidden", "main"),
        live("notonmap", "main"),
    ]);
    let messages = viewer.drain();
    assert_eq!(last_players(&messages).unwrap(), ["shown", "nohealth"]);
    for text in &messages {
        assert!(
            !text.contains("hidden") && !text.contains("notonmap"),
            "{text}"
        );
    }
    let v: Value = serde_json::from_str(&messages[0]).unwrap();
    assert_eq!(v["players"][0]["health"], 18);
    assert_eq!(v["players"][1]["health"], 0);
}

#[tokio::test(start_paused = true)]
async fn no_positions_while_dynmap_world_and_live_world_differ() {
    let (hub, _) = setup(&[
        ("dynmap_config.json", CONFIG.to_string()),
        (
            "dynmap_main.json",
            update_json(
                1,
                r#"[{"account":"a","world":"main","health":20,"armor":0}]"#,
            ),
        ),
    ]);
    let mut viewer = Viewer::new(1);
    hub.subscribe(&viewer.peer, "main").await;
    viewer.drain();
    hub.set_positions(&[live("a", "main")]);
    assert_eq!(last_players(&viewer.drain()).unwrap(), ["a"]);
    // 位置を送らないワールドへ移った。Dynmap が次の更新を書くまでも、移った先の位置は送らない
    hub.set_positions(&[live("a", "private")]);
    assert_eq!(last_players(&viewer.drain()).unwrap(), Vec::<String>::new());
}

#[tokio::test(start_paused = true)]
async fn unknown_world_names_do_not_read_the_db() {
    let (hub, store) = setup(&[
        ("dynmap_config.json", CONFIG.to_string()),
        ("dynmap_main.json", update_json(1, "[]")),
    ]);
    let mut viewers: Vec<Viewer> = (0..50).map(Viewer::new).collect();
    let subs: Vec<_> = viewers
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let hub = hub.clone();
            let peer = v.peer.clone();
            async move { hub.subscribe(&peer, &format!("nothing{i}")).await }
        })
        .collect();
    futures_util::future::join_all(subs).await;
    // でたらめな名前でも、一覧を 1 度読むだけ
    assert_eq!(*store.reads.lock().unwrap(), ["dynmap_config.json"]);
    assert!(viewers[0].drain().is_empty());
    hub.tick().await;
    assert_eq!(
        *store.reads.lock().unwrap(),
        ["dynmap_config.json"],
        "断った名前は読みに行かない"
    );
    assert_eq!(hub.viewer_count(), 0);

    // 少し間を置けば読み直す (増えたばかりのワールド)
    tokio::time::advance(Duration::from_secs(6)).await;
    hub.subscribe(&viewers[0].peer, "nothing").await;
    assert_eq!(store.read_count("dynmap_config.json"), 2);
    hub.subscribe(&viewers[0].peer, "nothing").await;
    assert_eq!(
        store.read_count("dynmap_config.json"),
        2,
        "すぐには読み直さない"
    );
}

#[tokio::test(start_paused = true)]
async fn a_newly_added_world_can_be_watched_after_a_reread() {
    let (hub, store) = setup(&[
        ("dynmap_config.json", CONFIG.to_string()),
        ("dynmap_new.json", update_json(1, "[]")),
    ]);
    hub.world_names(Duration::from_secs(30)).await;
    store.set_file(
        "dynmap_config.json",
        r#"{"worlds":[{"name":"main"},{"name":"new"}]}"#,
    );
    tokio::time::advance(Duration::from_secs(6)).await;
    let mut viewer = Viewer::new(1);
    hub.subscribe(&viewer.peer, "new").await;
    assert_eq!(viewer.types(), ["update"]);
}

#[tokio::test(start_paused = true)]
async fn waits_while_the_world_list_is_unreadable_then_sends() {
    let (hub, store) = setup(&[("dynmap_main.json", update_json(1, "[]"))]);
    let mut viewer = Viewer::new(1);
    let mut stranger = Viewer::new(2);
    hub.subscribe(&viewer.peer, "main").await;
    hub.subscribe(&stranger.peer, "nothing").await;
    assert!(viewer.drain().is_empty());
    assert_eq!(
        store.read_count("dynmap_main.json"),
        0,
        "一覧が読めるまでワールドを読まない"
    );
    hub.tick().await;
    assert_eq!(store.read_count("dynmap_main.json"), 0);
    store.set_file("dynmap_config.json", CONFIG);
    tokio::time::advance(Duration::from_secs(6)).await;
    hub.tick().await;
    assert_eq!(viewer.types(), ["update"], "読めたら送る");
    assert!(
        stranger.drain().is_empty(),
        "知らない名前は、読めたときに外す"
    );
    let before = store.reads.lock().unwrap().len();
    tokio::time::advance(Duration::from_secs(1)).await;
    hub.tick().await;
    assert_eq!(store.read_count("dynmap_nothing.json"), 0);
    assert_eq!(
        store.reads.lock().unwrap().len() - before,
        1,
        "残っているワールドだけを読む"
    );
}

#[tokio::test(start_paused = true)]
async fn the_previous_world_list_is_used_when_it_becomes_unreadable() {
    let (hub, store) = setup(&[
        ("dynmap_config.json", CONFIG.to_string()),
        ("dynmap_main.json", update_json(1, "[]")),
    ]);
    assert!(
        hub.world_names(Duration::from_secs(30))
            .await
            .unwrap()
            .contains("main")
    );
    store.set_failing(true);
    tokio::time::advance(Duration::from_secs(60)).await;
    assert!(
        hub.world_names(Duration::from_secs(30))
            .await
            .unwrap()
            .contains("main")
    );
    let count = store.reads.lock().unwrap().len();
    assert!(
        hub.world_names(Duration::from_secs(30))
            .await
            .unwrap()
            .contains("main")
    );
    assert_eq!(
        store.reads.lock().unwrap().len(),
        count,
        "失敗のあと、すぐには読み直さない"
    );
}

#[tokio::test(start_paused = true)]
async fn many_viewers_starting_at_once_read_the_world_once() {
    let (hub, store) = setup(&[
        ("dynmap_config.json", CONFIG.to_string()),
        ("dynmap_main.json", update_json(1, "[]")),
    ]);
    let mut viewers: Vec<Viewer> = (0..100).map(Viewer::new).collect();
    let subs: Vec<_> = viewers
        .iter()
        .map(|v| {
            let hub = hub.clone();
            let peer = v.peer.clone();
            async move { hub.subscribe(&peer, "main").await }
        })
        .collect();
    futures_util::future::join_all(subs).await;
    assert_eq!(store.read_count("dynmap_main.json"), 1);
    for v in &mut viewers {
        assert_eq!(v.types(), ["update"], "全員に今の状態を送る");
    }
    // 読んだばかりなら、新しく見始めた人にも読まずに送る
    let mut late = Viewer::new(1000);
    hub.subscribe(&late.peer, "main").await;
    assert_eq!(store.read_count("dynmap_main.json"), 1);
    assert_eq!(late.types(), ["update"]);
}

#[tokio::test(start_paused = true)]
async fn players_json_is_not_resent_when_unchanged() {
    let (hub, _) = setup(&[
        ("dynmap_config.json", CONFIG.to_string()),
        (
            "dynmap_main.json",
            update_json(
                1,
                r#"[{"account":"a","world":"main","health":20,"armor":0}]"#,
            ),
        ),
        (
            "dynmap_nether.json",
            update_json(
                1,
                r#"[{"account":"a","world":"main","health":0,"armor":0}]"#,
            ),
        ),
    ]);
    let (mut a, mut b, mut c) = (Viewer::new(1), Viewer::new(2), Viewer::new(3));
    hub.subscribe(&a.peer, "main").await;
    hub.subscribe(&b.peer, "main").await;
    hub.subscribe(&c.peer, "nether").await;
    a.drain();
    b.drain();
    c.drain();
    hub.set_positions(&[live("a", "main")]);
    let (ma, mb, mc) = (a.drain(), b.drain(), c.drain());
    assert_eq!(ma, mb, "同じワールドの人には同じ文字列");
    let va: Value = serde_json::from_str(&ma[0]).unwrap();
    assert_eq!(va["players"][0]["health"], 18);
    let vc: Value = serde_json::from_str(&mc[0]).unwrap();
    assert_eq!(vc["players"][0]["health"], 0, "ワールドごとの更新で決まる");
    // 同じ内容なら送らない
    hub.set_positions(&[live("a", "main")]);
    assert!(a.drain().is_empty() && b.drain().is_empty() && c.drain().is_empty());
    // 変われば送る
    let mut moved = live("a", "main");
    moved["x"] = json!(9);
    hub.set_positions(&[moved]);
    assert_eq!(a.types(), ["players"]);
}

#[tokio::test(start_paused = true)]
async fn broken_positions_do_not_stop_the_hub_and_left_peers_get_nothing() {
    let (hub, _) = setup(&[
        ("dynmap_config.json", CONFIG.to_string()),
        (
            "dynmap_main.json",
            update_json(
                1,
                r#"[{"account":"a","world":"main","health":20,"armor":0}]"#,
            ),
        ),
    ]);
    let mut viewer = Viewer::new(1);
    hub.subscribe(&viewer.peer, "main").await;
    viewer.drain();
    hub.set_positions(&[
        Value::Null,
        json!(1),
        json!({ "account": 1 }),
        json!({ "account": "x" }),
        live("a", "main"),
    ]);
    assert_eq!(last_players(&viewer.drain()).unwrap(), ["a"]);
    hub.leave(1);
    let mut moved = live("a", "main");
    moved["x"] = json!(9);
    hub.set_positions(&[moved]);
    assert!(viewer.drain().is_empty());
    // 調べているあいだに閉じた人を、あとから足さない
    let store = Arc::new(FakeStore::default());
    store.set_file("dynmap_config.json", CONFIG);
    store.set_file(
        "dynmap_main.json",
        &update_json(
            1,
            r#"[{"account":"a","world":"main","health":20,"armor":0}]"#,
        ),
    );
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    *store.gate.lock().unwrap() = Some(gate.clone());
    let hub = Hub::manual(store.clone());
    let mut quick = Viewer::new(2);
    let pending = {
        let hub = hub.clone();
        let peer = quick.peer.clone();
        tokio::spawn(async move { hub.subscribe(&peer, "main").await })
    };
    tokio::task::yield_now().await; // 一覧を読む問い合わせで止まっている
    assert_eq!(hub.viewer_count(), 1);
    hub.leave(2);
    gate.add_permits(10);
    pending.await.unwrap();
    assert_eq!(hub.viewer_count(), 0);
    let mut moved = live("a", "main");
    moved["x"] = json!(10);
    hub.set_positions(&[moved]);
    assert!(quick.drain().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_stuck_viewer_is_dropped_and_told_to_close() {
    let (hub, _) = setup(&[
        ("dynmap_config.json", CONFIG.to_string()),
        (
            "dynmap_main.json",
            update_json(
                1,
                r#"[{"account":"a","world":"main","health":20,"armor":0}]"#,
            ),
        ),
    ]);
    let stuck = Viewer::new(1); // 受け取らない
    hub.subscribe(&stuck.peer, "main").await;
    for i in 0..200 {
        let mut moved = live("a", "main");
        moved["x"] = json!(i);
        hub.set_positions(&[moved]);
    }
    assert_eq!(hub.viewer_count(), 0, "詰まった人は外す");
    tokio::time::timeout(Duration::from_millis(10), stuck.peer.kicked.notified())
        .await
        .expect("閉じるよう知らされる");
}

// ---- ポーリングの間隔と、重複送信の抑止

#[tokio::test(start_paused = true)]
async fn reads_each_world_once_per_second_however_many_watch() {
    let store = Arc::new(FakeStore::default());
    store.set_file("dynmap_config.json", CONFIG);
    store.set_file("dynmap_main.json", &update_json(1, "[]"));
    let hub = Hub::new(store.clone());
    let mut viewers: Vec<Viewer> = (0..30).map(Viewer::new).collect();
    for v in &viewers {
        hub.subscribe(&v.peer, "main").await;
    }
    for v in &mut viewers {
        v.drain();
    }
    let after_subscribe = store.read_count("dynmap_main.json");
    assert_eq!(after_subscribe, 1);
    // 3.5 秒: 1 秒ごとに 1 度 (30 人でも 3 回)
    for _ in 0..7 {
        tokio::time::advance(Duration::from_millis(500)).await;
        tokio::task::yield_now().await;
    }
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(store.read_count("dynmap_main.json") - after_subscribe, 3);
    // timestamp が変わっていないので、何も送らない
    for v in &mut viewers {
        assert!(v.drain().is_empty());
    }
    // timestamp が変わった次の 1 秒で、全員に 1 度だけ送る
    store.set_file("dynmap_main.json", &update_json(2, "[]"));
    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    for v in &mut viewers {
        let got = v.drain();
        assert_eq!(got.len(), 1);
        assert!(got[0].contains(r#""timestamp": 2"#));
    }
    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    for v in &mut viewers {
        assert!(v.drain().is_empty(), "同じ timestamp は送り直さない");
    }
}

#[tokio::test(start_paused = true)]
async fn polling_stops_when_nobody_watches() {
    let store = Arc::new(FakeStore::default());
    store.set_file("dynmap_config.json", CONFIG);
    store.set_file("dynmap_main.json", &update_json(1, "[]"));
    let hub = Hub::new(store.clone());
    let viewer = Viewer::new(1);
    hub.subscribe(&viewer.peer, "main").await;
    hub.leave(1);
    tokio::time::advance(Duration::from_secs(3)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    let before = store.reads.lock().unwrap().len();
    tokio::time::advance(Duration::from_secs(10)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        store.reads.lock().unwrap().len(),
        before,
        "見る人がいなければ読まない"
    );
    // また見始めれば、読み始める
    let again = Viewer::new(2);
    hub.subscribe(&again.peer, "main").await;
    tokio::task::yield_now().await; // 読み取りの仕事が動き出すまで
    let count = store.read_count("dynmap_main.json");
    tokio::time::advance(Duration::from_secs(2)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(store.read_count("dynmap_main.json") > count);
}

#[tokio::test(start_paused = true)]
async fn a_failing_db_does_not_stop_other_polls() {
    let (hub, store) = setup(&[
        ("dynmap_config.json", CONFIG.to_string()),
        ("dynmap_main.json", update_json(1, "[]")),
    ]);
    let mut viewer = Viewer::new(1);
    hub.subscribe(&viewer.peer, "main").await;
    viewer.drain();
    store.set_failing(true);
    hub.tick().await;
    assert!(viewer.drain().is_empty());
    store.set_failing(false);
    store.set_file("dynmap_main.json", &update_json(2, "[]"));
    tokio::time::advance(Duration::from_secs(1)).await;
    hub.tick().await;
    assert_eq!(viewer.types(), ["update"]);
}
