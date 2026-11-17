//! 構造物の看板を描くための Minecraft のリソースのまとめを、client の jar から作る。
//! 移管前の地図サーバーの `server/utils/mcassets-build.ts` の移植 (出力は同じバイト列になるようにしてある。`jsjson` を見ること)。
//!
//! ブロックの blockstates と models、アイテムの定義、防具の層、テクスチャ (と .mcmeta のアニメーション)、バイオームの色、
//! 絵画の大きさ、フォントを集めて、ブラウザーが 1 度だけ読めばよい形 (JSON 1 つとテクスチャを並べたもの 1 つ) にする。
//! Mojang のファイルは git にも Docker のイメージにも入れない (起動したあとに、マウントした jar から作る)。
//!
//! 移植しなかったもの: BlueMap の packs フォルダ (リソースパック) の重ね合わせ (本番では使っていない)。

use std::cmp::Ordering;
use std::collections::HashMap;
use std::io::{self, Cursor};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::jsjson;
use super::zip::ZipFile;

/// まとめの形のバージョン (変えたら上げる。URL の hash に入る)
pub const FORMAT: u32 = 1;

/// 1 枚のテクスチャの大きさの上限 (巨大な画像を配らない)
const MAX_TEXTURE_BYTES: usize = 4 * 1024 * 1024;
const MAX_TOTAL_TEXTURE_BYTES: usize = 192 * 1024 * 1024;
const MAX_JSON_BYTES: usize = 4 * 1024 * 1024;
/// テクスチャのうち、まとめに入れるフォルダ (gui や particle などは使わない)
const TEXTURE_FOLDERS: [&str; 6] = [
    "block/",
    "item/",
    "entity/",
    "painting/",
    "colormap/",
    "font/",
];

/// Unifont (26.3 のアセットの一覧のもの。MarumaSign 本体の SignTexts と同じ)
pub const UNIFONT_SHA1: &str = "ccd5ac4767ce0a9c71d1dd62f2dc25449789b5dd";
pub const UNIFONT_SIZE: usize = 1_559_654;

/// Unifont のページ (codepoint >> 8) と `XXXX:hex\n` の並び。ページと行は、zip の中で最初に出た順 (hash に入る)
#[derive(Default, Debug)]
pub struct Unifont {
    pages: Vec<(u32, String)>,
    by_page: HashMap<u32, usize>,
}

impl Unifont {
    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    pub fn page(&self, page: u32) -> Option<&str> {
        self.by_page.get(&page).map(|&i| self.pages[i].1.as_str())
    }

    pub fn bytes(&self) -> usize {
        self.pages.iter().map(|(_, t)| t.len()).sum()
    }
}

/// 作ったもの
pub struct Built {
    pub hash: String,
    pub version: String,
    pub texture_count: usize,
    /// assets.json
    pub json: Vec<u8>,
    /// textures.bin
    pub textures: Vec<u8>,
    pub unifont: Arc<Unifont>,
    /// 作るのにかかった時間 (ミリ秒)
    pub millis: u128,
}

/// `assets/<ns>/<folder><rest><ext>` を、名前 (`ns:rest`) にする
fn id_of(path: &str, folder: &str, ext: &str) -> Option<String> {
    let (ns, rest) = path.strip_prefix("assets/")?.split_once('/')?;
    if ns.is_empty()
        || !ns
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-'))
    {
        return None;
    }
    let rest = rest.strip_prefix(folder)?.strip_suffix(ext)?;
    if rest.is_empty()
        || !rest
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'/' | b'-'))
        || rest.contains("..")
    {
        return None;
    }
    Some(format!("{ns}:{rest}"))
}

fn parse_json(bytes: Option<Vec<u8>>) -> Option<Value> {
    let bytes = bytes?;
    if bytes.len() > MAX_JSON_BYTES {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes);
    // UTF-8 の BOM を除く
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    serde_json::from_str(text).ok()
}

/// JavaScript の `parseInt(text, 16)` (頭の空白、符号、`0x`、16 進の数字だけ読み、残りは捨てる)。数字が無ければ None
fn parse_int_hex(text: &str) -> Option<f64> {
    let t = text.trim_start();
    let (negative, t) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let t = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    let mut value = 0f64;
    let mut any = false;
    for c in t.chars() {
        let Some(d) = c.to_digit(16) else { break };
        value = value * 16.0 + f64::from(d);
        any = true;
    }
    any.then_some(if negative { -value } else { value })
}

/// JavaScript の `n & 0xFFFFFF` (`n` は有限の数)
fn mask24(n: f64) -> u32 {
    (n.trunc().rem_euclid(4_294_967_296.0) as u64 & 0xFF_FFFF) as u32
}

/// 色の値 (数か、`#` が付いてもよい 16 進の文字列)。読めなければ None
fn color_value(v: Option<&Value>) -> Option<u32> {
    match v? {
        Value::Number(n) => {
            let f = n.as_f64()?;
            f.is_finite().then(|| mask24((f + 0.5).floor()))
        }
        Value::String(s) => {
            let n = parse_int_hex(s.strip_prefix('#').unwrap_or(s))?;
            n.is_finite().then(|| mask24(n))
        }
        _ => None,
    }
}

/// `typeof v === 'number' && Number.isFinite(v) ? v : default`
fn num(v: Option<&Value>, default: f64) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().filter(|f| f.is_finite()).unwrap_or(default),
        _ => default,
    }
}

/// JavaScript の `!value` の逆
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// 連想配列の 1 つぶんの `"key":value` を足す
struct Section {
    buf: Vec<u8>,
}

impl Section {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn key(&mut self, key: &str) {
        if !self.buf.is_empty() {
            self.buf.push(b',');
        }
        jsjson::write(&Value::String(key.to_string()), &mut self.buf);
        self.buf.push(b':');
    }

    fn put(&mut self, key: &str, value: &Value) {
        self.key(key);
        jsjson::write(value, &mut self.buf);
    }
}

/// JavaScript の配列の `sort()` (UTF-16 の順) と同じ並べ方
fn utf16_cmp(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// jar からまとめを作る (重い仕事。`spawn_blocking` で呼ぶこと)
pub fn build(jar: &Path, unifont: Option<Arc<Unifont>>) -> io::Result<Built> {
    let started = Instant::now();
    let mut zip = ZipFile::open(jar)?;
    let mut paths: Vec<String> = zip
        .names()
        .filter(|p| p.starts_with("assets/") || p.starts_with("data/"))
        .map(str::to_string)
        .collect();
    paths.sort_by(|a, b| utf16_cmp(a, b));

    let version = match parse_json(zip.get("version.json")?) {
        Some(Value::Object(o)) => match o.get("name") {
            Some(Value::String(name)) => name.chars().take(40).collect(),
            _ => String::new(),
        },
        _ => String::new(),
    };

    let mut mcmeta: HashMap<String, Value> = HashMap::new();
    for p in &paths {
        if !p.ends_with(".png.mcmeta") {
            continue;
        }
        if let Some(json) = parse_json(zip.get(p)?)
            && let Some(animation) = json.get("animation")
            && matches!(animation, Value::Object(_) | Value::Array(_))
        {
            mcmeta.insert(
                p[..p.len() - ".mcmeta".len()].to_string(),
                animation.clone(),
            );
        }
    }

    let mut blockstates = Section::new();
    let mut models = Section::new();
    let mut items = Section::new();
    let mut equipment = Section::new();
    let mut biomes = Section::new();
    let mut paintings = Section::new();
    let mut textures_index = Section::new();
    let mut texture_count = 0usize;
    let mut textures: Vec<u8> = Vec::new();

    for p in &paths {
        if let Some(id) = id_of(p, "blockstates/", ".json") {
            if let Some(j) = parse_json(zip.get(p)?)
                && matches!(j, Value::Object(_) | Value::Array(_))
            {
                blockstates.put(&id, &j);
            }
        } else if let Some(id) = id_of(p, "models/", ".json") {
            if let Some(j) = parse_json(zip.get(p)?)
                && matches!(j, Value::Object(_) | Value::Array(_))
            {
                models.put(&id, &j);
            }
        } else if let Some(id) = id_of(p, "items/", ".json") {
            if let Some(j) = parse_json(zip.get(p)?)
                && let Some(model) = j.get("model")
                && matches!(model, Value::Object(_) | Value::Array(_))
            {
                items.put(&id, model);
            }
        } else if let Some(id) = id_of(p, "equipment/", ".json") {
            if let Some(j) = parse_json(zip.get(p)?)
                && matches!(j, Value::Object(_) | Value::Array(_))
            {
                equipment.put(&id, &j);
            }
        } else if let Some(id) = id_of(p, "textures/", ".png") {
            let rest = &id[id.find(':').map_or(0, |i| i + 1)..];
            if !TEXTURE_FOLDERS.iter().any(|f| rest.starts_with(f)) {
                continue;
            }
            let Some(b) = zip.get(p)? else { continue };
            // PNG だけ (先頭の印で確かめる)
            if b.len() > MAX_TEXTURE_BYTES || !b.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
                continue;
            }
            if textures.len() + b.len() > MAX_TOTAL_TEXTURE_BYTES {
                continue;
            }
            textures_index.key(&id);
            let animation = mcmeta.get(p).map_or_else(
                || "0".to_string(),
                |a| String::from_utf8_lossy(&jsjson::to_vec(a)).into_owned(),
            );
            textures_index.buf.extend_from_slice(
                format!("[{},{},{animation}]", textures.len(), b.len()).as_bytes(),
            );
            texture_count += 1;
            textures.extend_from_slice(&b);
        } else if let Some(name) = p
            .strip_prefix("data/minecraft/worldgen/biome/")
            .and_then(|n| n.strip_suffix(".json"))
        {
            let Some(j) = parse_json(zip.get(p)?).filter(truthy) else {
                continue;
            };
            let effects = j.get("effects").filter(|e| e.is_object());
            let effect = |k: &str| effects.and_then(|e| e.get(k));
            let color = |v: Option<u32>| v.map_or_else(|| "null".to_string(), |c| c.to_string());
            let modifier = match effect("grass_color_modifier") {
                Some(Value::String(s)) => s.as_str(),
                _ => "none",
            };
            let mut row = format!(
                "[{},{},{},{},{},{},",
                jsjson::js_number(num(j.get("temperature"), 0.8)),
                jsjson::js_number(num(j.get("downfall"), 0.4)),
                color(color_value(effect("grass_color"))),
                color(color_value(effect("foliage_color"))),
                color(color_value(effect("dry_foliage_color"))),
                color(color_value(effect("water_color"))),
            )
            .into_bytes();
            jsjson::write(&Value::String(modifier.to_string()), &mut row);
            row.push(b']');
            biomes.key(&format!("minecraft:{name}"));
            biomes.buf.extend_from_slice(&row);
        } else if let Some(name) = p
            .strip_prefix("data/minecraft/painting_variant/")
            .and_then(|n| n.strip_suffix(".json"))
        {
            let Some(j) = parse_json(zip.get(p)?).filter(truthy) else {
                continue;
            };
            let dim = |k: &str| match j.get(k) {
                Some(Value::Number(n)) => jsjson::number(n),
                _ => "1".to_string(),
            };
            let mut row = format!("[{},{},", dim("width"), dim("height")).into_bytes();
            let asset = match j.get("asset_id") {
                Some(Value::String(s)) => s.as_str(),
                _ => "",
            };
            jsjson::write(&Value::String(asset.to_string()), &mut row);
            row.push(b']');
            paintings.key(name);
            paintings.buf.extend_from_slice(&row);
        }
    }

    let fonts: Vec<Value> = match parse_json(zip.get("assets/minecraft/font/include/default.json")?)
    {
        Some(j) => match j.get("providers") {
            Some(Value::Array(providers)) => providers
                .iter()
                .filter(|p| p.get("type").and_then(Value::as_str) == Some("bitmap"))
                .cloned()
                .collect(),
            _ => Vec::new(),
        },
        None => Vec::new(),
    };

    let unifont = unifont.unwrap_or_default();
    let mut json: Vec<u8> = Vec::with_capacity(
        blockstates.buf.len()
            + models.buf.len()
            + items.buf.len()
            + textures_index.buf.len()
            + 4096,
    );
    json.extend_from_slice(format!("{{\"format\":{FORMAT},\"version\":").as_bytes());
    jsjson::write(&Value::String(version.clone()), &mut json);
    for (name, section) in [
        ("blockstates", &blockstates),
        ("models", &models),
        ("items", &items),
        ("equipment", &equipment),
        ("biomes", &biomes),
        ("paintings", &paintings),
    ] {
        json.extend_from_slice(format!(",\"{name}\":{{").as_bytes());
        json.extend_from_slice(&section.buf);
        json.push(b'}');
    }
    json.extend_from_slice(b",\"fonts\":");
    jsjson::write(&Value::Array(fonts), &mut json);
    json.extend_from_slice(b",\"textures\":{");
    json.extend_from_slice(&textures_index.buf);
    json.extend_from_slice(format!("}},\"unifont\":{}}}", !unifont.is_empty()).as_bytes());

    let mut h = Sha256::new();
    h.update(FORMAT.to_string());
    h.update(&json);
    h.update(&textures);
    for (page, text) in &unifont.pages {
        h.update(page.to_string());
        h.update(text);
    }
    let hash: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();

    Ok(Built {
        hash: hash[..20].to_string(),
        version,
        texture_count,
        json,
        textures,
        unifont,
        millis: started.elapsed().as_millis(),
    })
}

/// Unifont の zip (`minecraft/font/unifont.zip`。jar にはなく、ランチャーが別に配る) の .hex を、256 文字ごとのページにする。
/// MarumaSign 本体の SignTexts と同じく、zip の中の .hex を順に読み、後のものが前のものを上書きする
pub fn parse_unifont(zip_bytes: &[u8]) -> io::Result<Unifont> {
    let mut zip = ZipFile::new(Cursor::new(zip_bytes))?;
    let names: Vec<String> = zip
        .names()
        .filter(|n| n.ends_with(".hex"))
        .map(str::to_string)
        .collect();
    let mut glyphs: Vec<(u32, String)> = Vec::new();
    let mut at: HashMap<u32, usize> = HashMap::new();
    for name in names {
        let Some(bytes) = zip.get(&name)? else {
            continue;
        };
        // latin1 として読む (中身は ASCII の 16 進だけ)
        let text: String = bytes.iter().map(|&b| char::from(b)).collect();
        for line in text.split('\n') {
            let Some(colon) = line.find(':').filter(|&c| c > 0) else {
                continue;
            };
            let Some(cp) = parse_int_hex(&line[..colon]) else {
                continue;
            };
            let hex = line[colon + 1..].trim();
            let valid =
                (32..=256).contains(&hex.len()) && hex.bytes().all(|b| b.is_ascii_hexdigit());
            if !(0.0..=1_114_111.0).contains(&cp) || !valid {
                continue;
            }
            let cp = cp as u32;
            match at.get(&cp) {
                Some(&i) => glyphs[i].1 = hex.to_string(),
                None => {
                    at.insert(cp, glyphs.len());
                    glyphs.push((cp, hex.to_string()));
                }
            }
        }
    }
    let mut out = Unifont::default();
    for (cp, hex) in glyphs {
        let page = cp >> 8;
        let i = *out.by_page.entry(page).or_insert_with(|| {
            out.pages.push((page, String::new()));
            out.pages.len() - 1
        });
        let text = &mut out.pages[i].1;
        text.push_str(&format!("{cp:04X}:{hex}\n"));
    }
    Ok(out)
}

/// バージョンの名前 (26.3、26.10、26.3.1 など) を、数の部分ごとに比べる
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    fn numbers(s: &str) -> Vec<i64> {
        let mut out = Vec::new();
        let mut cur = String::new();
        for c in s.chars().chain(std::iter::once(' ')) {
            if c.is_ascii_digit() {
                cur.push(c);
            } else if !cur.is_empty() {
                out.push(cur.parse().unwrap_or(i64::MAX));
                cur.clear();
            }
        }
        out
    }
    let (na, nb) = (numbers(a), numbers(b));
    for i in 0..na.len().max(nb.len()) {
        let d = na
            .get(i)
            .copied()
            .unwrap_or(-1)
            .cmp(&nb.get(i).copied().unwrap_or(-1));
        if d != Ordering::Equal {
            return d;
        }
    }
    a.cmp(b)
}

/// client の jar の場所。ファイルならそれ、フォルダなら、その中の `minecraft-client-<バージョン>.jar` のうちバージョンが最も新しいもの
/// (BlueMap がダウンロードする名前。Minecraft を更新しても設定を変えなくてよい)。見つからなければ None
pub fn resolve_client_jar(path: &Path) -> Option<PathBuf> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_dir() {
        return Some(path.to_path_buf());
    }
    let mut best: Option<(PathBuf, String)> = None;
    for entry in std::fs::read_dir(path).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(version) = name
            .strip_prefix("minecraft-client-")
            .and_then(|n| n.strip_suffix(".jar"))
            .filter(|v| !v.is_empty())
        else {
            continue;
        };
        if !entry.metadata().is_ok_and(|m| m.is_file()) {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|(_, v)| compare_versions(version, v) == Ordering::Greater)
        {
            best = Some((entry.path(), version.to_string()));
        }
    }
    best.map(|(p, _)| p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number() {
        assert_eq!(compare_versions("26.10", "26.3"), Ordering::Greater);
        assert_eq!(compare_versions("26.3.1", "26.3"), Ordering::Greater);
        assert_eq!(compare_versions("1.21.11", "1.21.9"), Ordering::Greater);
        assert_eq!(compare_versions("26.3", "26.3"), Ordering::Equal);
    }

    #[test]
    fn hex_prefix_like_parse_int() {
        assert_eq!(parse_int_hex("0041 x"), Some(65.0));
        assert_eq!(parse_int_hex("  0x1F"), Some(31.0));
        assert_eq!(parse_int_hex("zz"), None);
        assert_eq!(parse_int_hex("-1"), Some(-1.0));
        assert_eq!(mask24(-1.0), 0xFF_FFFF);
    }

    #[test]
    fn ids() {
        assert_eq!(
            id_of(
                "assets/minecraft/models/block/stone.json",
                "models/",
                ".json"
            )
            .as_deref(),
            Some("minecraft:block/stone")
        );
        assert_eq!(
            id_of("assets/minecraft/models/../x.json", "models/", ".json"),
            None
        );
        assert_eq!(
            id_of("assets/Minecraft/models/x.json", "models/", ".json"),
            None
        );
        assert_eq!(
            id_of("assets/minecraft/models/X.json", "models/", ".json"),
            None
        );
        assert_eq!(
            id_of("assets/minecraft/models/.json", "models/", ".json"),
            None
        );
    }
}
