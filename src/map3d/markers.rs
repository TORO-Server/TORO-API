//! Dynmap のマーカー (DB の `MarkerFiles`) を、BlueMap の `markers.json` の形にする。
//!
//! 対象は、Dynmap で最初から出している組 (`hide` が偽) の点のマーカーだけ (MarumaSign のリンク切れなど、
//! 直す人向けの組は入れない)。BlueMap の html マーカーの中身を空にして、3D の画面には何も描かず、
//! マーカーの一覧にだけ名前と座標を出す (3D の画面では建物そのものが見えるため。一覧で選ぶと、カメラがそこへ動く)。

use serde_json::{Map, Value, json};

/// 英語のままの組の名前を、一覧で日本語にする
const SET_LABELS: [(&str, &str); 1] = [("markers", "地点")];
/// 読み直す間隔
pub const MAX_AGE_SECS: u64 = 30;

/// JavaScript の真偽 (`if (set.hide)`)
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `markup` が真の名前は HTML なので、文字だけにする
pub fn plain_label(label: &str, markup: bool) -> String {
    let text = if markup {
        strip_tags(label)
            .replace("&nbsp;", " ")
            .replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
    } else {
        label.to_string()
    };
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `/<[^>]*>/g` を空白に置き換える (閉じていない `<` はそのまま)
fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        match rest[start..].find('>') {
            Some(len) => {
                out.push_str(&rest[..start]);
                out.push(' ');
                rest = &rest[start + len + 1..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

/// `MarkerFiles` の内容から、BlueMap のマーカーの組を作る。読めなければ空
pub fn dynmap_sets(content: &str) -> Map<String, Value> {
    let mut sets = Map::new();
    let Ok(Value::Object(file)) = serde_json::from_str::<Value>(content) else {
        return sets;
    };
    let Some(Value::Object(file_sets)) = file.get("sets") else {
        return sets;
    };
    for (set_id, set) in file_sets {
        let Value::Object(set) = set else { continue };
        if truthy(set.get("hide")) {
            continue;
        }
        let mut markers = Map::new();
        if let Some(Value::Object(points)) = set.get("markers") {
            for (id, point) in points {
                let Value::Object(point) = point else {
                    continue;
                };
                let label = plain_label(
                    point.get("label").and_then(Value::as_str).unwrap_or(""),
                    truthy(point.get("markup")),
                );
                if label.is_empty() {
                    continue;
                }
                let mut position = Map::new();
                for axis in ["x", "y", "z"] {
                    if let Some(v) = point.get(axis) {
                        position.insert(axis.to_string(), v.clone());
                    }
                }
                markers.insert(
                    format!("dynmap-{id}"),
                    json!({
                        "type": "html",
                        "position": position,
                        "label": label,
                        "html": "",
                        "classes": [],
                        "anchor": { "x": 0, "y": 0 },
                        "listed": true,
                        "sorting": 0,
                    }),
                );
            }
        }
        if markers.is_empty() {
            continue;
        }
        let label = SET_LABELS
            .iter()
            .find(|(id, _)| id == set_id)
            .map(|(_, label)| Value::from(*label))
            .or_else(|| set.get("label").filter(|v| !v.is_null()).cloned())
            .unwrap_or_else(|| Value::from(set_id.as_str()));
        sets.insert(
            format!("dynmap-{set_id}"),
            json!({
                "label": label,
                // 3D の画面に何も描かないので、表示の切り替えはいらない
                "toggleable": false,
                "defaultHidden": false,
                "sorting": set.get("layerprio").filter(|v| !v.is_null()).cloned().unwrap_or(Value::from(0)),
                "markers": markers,
                "markerSets": {},
            }),
        );
    }
    sets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(plain_label("  a   b \n c ", false), "a b c");
        assert_eq!(
            plain_label("<b>駅</b>&nbsp;&amp;&lt;A&gt; &quot;x&quot;", true),
            "駅 &<A> \"x\""
        );
        assert_eq!(plain_label("1 < 2", true), "1 < 2");
        assert_eq!(plain_label("<i></i>", true), "");
        // markup でなければ、HTML のままの文字として残す
        assert_eq!(plain_label("<b>x</b>", false), "<b>x</b>");
    }

    #[test]
    fn converts_visible_sets_only() {
        let content = r#"{"sets":{
            "markers":{"label":"Markers","hide":false,"layerprio":3,"markers":{
                "ic1":{"label":"IC 1","x":1,"y":64,"z":-3,"markup":false},
                "empty":{"label":"  ","x":0,"y":0,"z":0},
                "html":{"label":"<b>駅</b>","markup":true,"x":5,"y":6,"z":7}}},
            "broken":{"label":"リンク切れ","hide":true,"markers":{"a":{"label":"a","x":0,"y":0,"z":0}}},
            "nothing":{"label":"空","markers":{}},
            "other":{"label":"その他","markers":{"p":{"label":"P","x":1,"y":2,"z":3}}}
        }}"#;
        let sets = dynmap_sets(content);
        let keys: Vec<&str> = sets.keys().map(String::as_str).collect();
        assert_eq!(keys, ["dynmap-markers", "dynmap-other"]);
        let markers = &sets["dynmap-markers"];
        assert_eq!(markers["label"], "地点");
        assert_eq!(markers["sorting"], 3);
        assert_eq!(markers["toggleable"], false);
        let points: Vec<&str> = markers["markers"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(points, ["dynmap-ic1", "dynmap-html"]);
        assert_eq!(markers["markers"]["dynmap-ic1"]["position"]["z"], -3);
        assert_eq!(markers["markers"]["dynmap-html"]["label"], "駅");
        assert_eq!(markers["markers"]["dynmap-ic1"]["html"], "");
        assert_eq!(sets["dynmap-other"]["label"], "その他");
        assert_eq!(sets["dynmap-other"]["sorting"], 0);
    }

    #[test]
    fn unreadable_files_are_empty() {
        assert!(dynmap_sets("not json").is_empty());
        assert!(dynmap_sets("[]").is_empty());
        assert!(dynmap_sets("{}").is_empty());
    }
}
