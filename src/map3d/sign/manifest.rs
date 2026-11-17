//! 再生リスト (HLS の .m3u8、MPEG-DASH の .mpd) と glTF (.gltf) に書かれた、ほかのファイルの URL を取り出す。
//!
//! 正本は TORO-3D-WebMap の `app/bluemap/marumasign/manifest.ts`。看板の中継は、看板に書かれた URL と、
//! そこから再生リストなどでたどれる URL だけを読む。そのために、たどった先の URL をここで決める (書かれていない URL は読まない)。
//!
//! 中身は外のサーバーが決めるので、正規表現を使わず、前から 1 度だけ読む (手間は本文の長さに比例する)。
//! `tests/vectors/marumasign-shared.json` の入出力と同じ結果になることを `tests/sign_vectors.rs` が確かめる。
//! 文字数の上限は、コードポイントではなく UTF-16 の単位で数える (JavaScript と同じ)。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use url::Url;

use super::core::utf16_len;
use super::xml_head::{js_trim, skip_space, starts_after_xml_head, tag_follows};

/// 1 つの再生リストなどから取り出す URL の数の上限
pub const MAX_CHILDREN: usize = 20000;
/// MPEG-DASH の SegmentTemplate (番号や時刻で URL が決まる) の数の上限
pub const MAX_PATTERNS: usize = 256;
/// SegmentTemplate 1 つの置き換える部分の数の上限
pub const MAX_SLOTS: usize = 5;
/// Representation の id の数と長さの上限
pub const MAX_IDS: usize = 256;
pub const MAX_ID_LENGTH: usize = 128;
const MAX_LINES: usize = 200_000;
const MAX_TOKENS: usize = 200_000;
const MAX_DEPTH: usize = 64;
const MAX_ATTRS: usize = 64;
const MAX_BASE_TEXT: usize = 8192;

/// 置き換える部分の種類: id は Representation の id のどれか、num は数字、any は id が分からないときの「/ ? # 以外」
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlSlot {
    Id,
    Num,
    Any,
}

/// Representation の id (見つけた順と、引くための集合)
#[derive(Debug, Default)]
pub struct IdSet {
    pub order: Vec<String>,
    set: HashSet<String>,
}

impl IdSet {
    fn insert(&mut self, id: &str) {
        if self.set.insert(id.to_string()) {
            self.order.push(id.to_string());
        }
    }
    pub fn len(&self) -> usize {
        self.order.len()
    }
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
    pub fn contains(&self, id: &str) -> bool {
        self.set.contains(id)
    }
}

/// 番号などで決まる URL の形 (MPEG-DASH の SegmentTemplate)。
/// parts[0], slots[0], parts[1], slots[1], ... parts[n] の順に並ぶ (parts は slots より 1 つ多い)
#[derive(Debug, Clone)]
pub struct UrlPattern {
    pub parts: Vec<String>,
    pub slots: Vec<UrlSlot>,
    pub ids: Arc<IdSet>,
    /// id の長さ (バイト。長い順)
    pub id_lengths: Arc<Vec<usize>>,
}

/// 取り出した URL
#[derive(Debug, Default)]
pub struct ManifestRefs {
    /// そのまま書かれた URL (絶対 URL、「#」から後ろは除く)
    pub urls: Vec<String>,
    /// 番号などで決まる URL の形
    pub patterns: Vec<UrlPattern>,
    /// 次も再生リストかもしれない URL (HLS)
    pub playlists: Vec<String>,
}

/// http(s) の絶対 URL にする (「#」から後ろは除く)。ほかの形式や壊れたものは None
pub fn resolve_ref(reference: &str, base: &str) -> Option<String> {
    let s = js_trim(reference);
    if s.is_empty() || utf16_len(s) > 4000 {
        return None;
    }
    let base = Url::parse(base).ok()?;
    let mut u = Url::options().base_url(Some(&base)).parse(s).ok()?;
    if u.scheme() != "https" && u.scheme() != "http" {
        return None;
    }
    u.set_fragment(None);
    Some(u.to_string())
}

fn push(list: &mut Vec<String>, seen: &mut HashSet<String>, url: Option<String>) {
    let Some(url) = url else { return };
    if seen.contains(&url) || list.len() >= MAX_CHILDREN {
        return;
    }
    seen.insert(url.clone());
    list.push(url);
}

fn starts_with_ci(text: &str, prefix: &str) -> bool {
    text.as_bytes()
        .get(..prefix.len())
        .is_some_and(|s| s.eq_ignore_ascii_case(prefix.as_bytes()))
}

/// `^(﻿)?\s*` のあと
fn after_bom_and_space(text: &str) -> &str {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    &text[skip_space(text, 0)..]
}

/// 先頭が HLS の再生リストか
pub fn is_hls(text: &str) -> bool {
    after_bom_and_space(text).starts_with("#EXTM3U")
}

/// 先頭が MPEG-DASH の MPD か
pub fn is_dash(text: &str) -> bool {
    starts_after_xml_head(text, |rest| tag_follows(rest, "MPD", false), true, true)
}

/// 行のなかの `URI="..."` と `SERVER-URI="..."` の値 (前が行頭か「,」「:」のもの。大文字小文字は区別しない)
fn uri_attributes(line: &str) -> Vec<&str> {
    let bytes = line.as_bytes();
    let keyword = |at: usize| -> Option<(usize, usize, usize)> {
        let rest = line.get(at..)?;
        let skip = if starts_with_ci(rest, "URI=\"") {
            5
        } else if starts_with_ci(rest, "SERVER-URI=\"") {
            12
        } else {
            return None;
        };
        let start = at + skip;
        let len = line[start..].find('"')?;
        Some((start, start + len, start + len + 1))
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i <= bytes.len() {
        let mut found = None;
        if i == 0 {
            found = keyword(0);
        }
        if found.is_none() && i < bytes.len() && (bytes[i] == b',' || bytes[i] == b':') {
            found = keyword(i + 1);
        }
        match found {
            Some((start, end, next)) => {
                out.push(&line[start..end]);
                i = next;
            }
            None => i += 1,
        }
    }
    out
}

/// HLS の再生リストに書かれた URL。タグでない行 (分割されたファイルや、各画質の再生リスト) と、タグの URI="..."
pub fn hls_refs(text: &str, base: &str) -> ManifestRefs {
    let mut urls = Vec::new();
    let mut playlists = Vec::new();
    let mut seen = HashSet::new();
    let mut next_is_playlist = false;
    let mut next = 0;
    let mut lines = 0;
    while next <= text.len() && lines < MAX_LINES && urls.len() < MAX_CHILDREN {
        lines += 1;
        let end = text[next..].find('\n').map_or(text.len(), |i| next + i);
        let line = js_trim(&text[next..end]);
        next = end + 1;
        if line.is_empty() {
            continue;
        }
        if line.starts_with('#') {
            if starts_with_ci(line, "#EXT-X-STREAM-INF:") {
                next_is_playlist = true;
            }
            let is_media = [
                "#EXT-X-MEDIA:",
                "#EXT-X-I-FRAME-STREAM-INF:",
                "#EXT-X-RENDITION-REPORT:",
            ]
            .iter()
            .any(|p| starts_with_ci(line, p));
            for value in uri_attributes(line) {
                let u = resolve_ref(value, base);
                push(&mut urls, &mut seen, u.clone());
                if is_media && playlists.len() < MAX_CHILDREN {
                    playlists.extend(u);
                }
            }
            continue;
        }
        let u = resolve_ref(line, base);
        push(&mut urls, &mut seen, u.clone());
        if next_is_playlist && playlists.len() < MAX_CHILDREN {
            playlists.extend(u);
        }
        next_is_playlist = false;
    }
    ManifestRefs {
        urls,
        patterns: Vec::new(),
        playlists,
    }
}

// ---------------------------------------------------------------- MPEG-DASH

/// XML の文字の参照 (`&lt;` `&#x41;` など) を解く
fn decode_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let body = &rest[1..];
        let named = [
            ("lt;", '<'),
            ("gt;", '>'),
            ("amp;", '&'),
            ("quot;", '"'),
            ("apos;", '\''),
        ]
        .into_iter()
        .find(|(name, _)| body.starts_with(name));
        if let Some((name, c)) = named {
            out.push(c);
            rest = &body[name.len()..];
            continue;
        }
        let numeric = if let Some(hex) = body.strip_prefix("#x") {
            let n = hex.bytes().take_while(u8::is_ascii_hexdigit).count();
            (n > 0 && hex[n..].starts_with(';'))
                .then(|| (u32::from_str_radix(&hex[..n], 16), 2 + n + 1))
        } else if let Some(dec) = body.strip_prefix('#') {
            let n = dec.bytes().take_while(u8::is_ascii_digit).count();
            (n > 0 && dec[n..].starts_with(';')).then(|| (dec[..n].parse::<u32>(), 1 + n + 1))
        } else {
            None
        };
        match numeric {
            Some((value, used)) => {
                // 大きすぎる値や 0 は空に。サロゲートは JavaScript なら 1 文字の壊れた文字になり、URL にすれば U+FFFD になる
                if let Ok(n) = value
                    && n > 0
                    && n < 0x11_0000
                {
                    out.push(char::from_u32(n).unwrap_or('\u{fffd}'));
                }
                rest = &body[used..];
            }
            None => {
                out.push('&');
                rest = body;
            }
        }
    }
    out.push_str(rest);
    out
}

struct Tag<'a> {
    closing: bool,
    name: &'a str,
    attrs: HashMap<String, String>,
    empty: bool,
}

enum XmlToken<'a> {
    Text(&'a str),
    Cdata(&'a str),
    Tag(Tag<'a>),
}

fn is_xml_space(c: u8) -> bool {
    matches!(c, 0x20 | 0x09 | 0x0A | 0x0D)
}

fn is_name_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_name_char(c: u8) -> bool {
    is_name_start(c) || c.is_ascii_digit() || c == b':' || c == b'.' || c == b'-'
}

/// 名前空間の接頭辞は外す (xlink:href など)
fn local_name(name: &str) -> &str {
    &name[name.rfind(':').map_or(0, |i| i + 1)..]
}

fn skip_xml_space(xml: &[u8], from: usize) -> usize {
    let mut at = from;
    while at < xml.len() && is_xml_space(xml[at]) {
        at += 1;
    }
    at
}

/// XML を前から 1 度だけ読んで、字句ごとに `f` を呼ぶ (`f` が false を返したら止める)。
/// 閉じていないコメントなどや引用符があれば、そこで読むのをやめる。タグの形が崩れていれば、そこまでを飛ばして続ける
fn xml_tokens<'a>(xml: &'a str, mut f: impl FnMut(XmlToken<'a>) -> bool) {
    let b = xml.as_bytes();
    let len = b.len();
    let mut at = 0;
    while at < len {
        let lt = xml[at..].find('<').map(|i| at + i);
        if lt != Some(at) {
            let end = lt.unwrap_or(len);
            if !f(XmlToken::Text(&xml[at..end])) {
                return;
            }
            at = end;
            continue;
        }
        if xml[at..].starts_with("<!--") {
            let Some(i) = xml[at + 4..].find("-->") else {
                return;
            };
            at = at + 4 + i + 3;
            continue;
        }
        if xml[at..].starts_with("<![CDATA[") {
            let Some(i) = xml[at + 9..].find("]]>") else {
                return;
            };
            if !f(XmlToken::Cdata(&xml[at + 9..at + 9 + i])) {
                return;
            }
            at = at + 9 + i + 3;
            continue;
        }
        if xml[at..].starts_with("<?") {
            let Some(i) = xml[at + 2..].find("?>") else {
                return;
            };
            at = at + 2 + i + 2;
            continue;
        }
        if xml[at..].starts_with("<!DOCTYPE") {
            let Some(i) = xml[at..].find('>') else {
                return;
            };
            at += i + 1;
            continue;
        }
        // タグ
        let mut p = at + 1;
        let closing = b.get(p) == Some(&b'/');
        if closing {
            p += 1;
        }
        let name_start = p;
        if p >= len || !is_name_start(b[p]) {
            at = p;
            continue;
        }
        while p < len && is_name_char(b[p]) {
            p += 1;
        }
        let name = local_name(&xml[name_start..p]);
        let mut attrs = HashMap::new();
        let mut done = false;
        let mut empty = false;
        loop {
            let before = p;
            p = skip_xml_space(b, p);
            if p >= len {
                return;
            }
            if b[p] == b'>' {
                p += 1;
                done = true;
                break;
            }
            if b[p] == b'/' && b.get(p + 1) == Some(&b'>') {
                p += 2;
                empty = true;
                done = true;
                break;
            }
            // 属性の前には空白が要る
            if p == before {
                break;
            }
            let attr_start = p;
            while p < len && !is_xml_space(b[p]) && !b"=>/<\"'".contains(&b[p]) {
                p += 1;
            }
            if p == attr_start {
                break;
            }
            let attr_name = &xml[attr_start..p];
            p = skip_xml_space(b, p);
            if b.get(p) != Some(&b'=') {
                break;
            }
            p = skip_xml_space(b, p + 1);
            let quote = b.get(p).copied();
            if quote != Some(b'"') && quote != Some(b'\'') {
                break;
            }
            let Some(close) = xml[p + 1..].find(quote.unwrap() as char) else {
                return;
            };
            let close = p + 1 + close;
            if attrs.len() < MAX_ATTRS && is_name_start(attr_name.as_bytes()[0]) {
                attrs.insert(
                    local_name(attr_name).to_string(),
                    decode_xml(&xml[p + 1..close]),
                );
            }
            p = close + 1;
        }
        // 崩れたタグは飛ばす (見たところには戻らない)
        if done
            && !f(XmlToken::Tag(Tag {
                closing,
                name,
                attrs,
                empty,
            }))
        {
            return;
        }
        at = p.max(at + 1);
    }
}

struct Element {
    name: String,
    base: String,
    /// この中の BaseURL の文字 (読み終えたら base を決める)
    text: String,
    /// `text` の UTF-16 の単位の数
    text_units: usize,
}

/// 先頭から UTF-16 の単位で `max` まで (文字の途中では切らない)
fn take_utf16(text: &str, max: usize) -> &str {
    let mut units = 0;
    for (i, c) in text.char_indices() {
        units += c.len_utf16();
        if units > max {
            return &text[..i];
        }
    }
    text
}

fn append_base_text(el: &mut Element, text: &str, decode: bool) {
    if el.name != "BaseURL" || el.text_units >= MAX_BASE_TEXT {
        return;
    }
    let text = take_utf16(text, MAX_BASE_TEXT);
    let piece = if decode {
        decode_xml(text)
    } else {
        text.to_string()
    };
    el.text_units += utf16_len(&piece);
    el.text.push_str(&piece);
}

enum Template {
    Url(String),
    Pattern(UrlPattern),
}

/// `$Name$`、`$Name%05d$` (Name は 5 種類) の長さ
fn variable_len(s: &str) -> Option<usize> {
    let body = s.strip_prefix('$')?;
    let name = [
        "RepresentationID",
        "Number",
        "Time",
        "Bandwidth",
        "SubNumber",
    ]
    .into_iter()
    .find(|n| body.starts_with(n))?;
    let mut at = 1 + name.len();
    let bytes = s.as_bytes();
    if bytes.get(at) == Some(&b'%') {
        let digits = bytes[at + 1..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count();
        let end = at + 1 + digits;
        if matches!(bytes.get(end), Some(b'd' | b'x' | b'X' | b'o')) {
            at = end + 1;
        }
    }
    (bytes.get(at) == Some(&b'$')).then_some(at + 1)
}

/// SegmentTemplate 1 つを URL の形にする。置き換える部分がなければ、その URL。使えないものは None
fn template_pattern(
    template: &str,
    base: &str,
    ids: &Arc<IdSet>,
    id_lengths: &Arc<Vec<usize>>,
) -> Option<Template> {
    // 置き換える部分を、URL として解決しても変わらない目印にしてから解決する
    let mut marks: Vec<&str> = Vec::new();
    let mut marked = String::with_capacity(template.len());
    let mut i = 0;
    while i < template.len() {
        let rest = &template[i..];
        if rest.starts_with('$') {
            if let Some(n) = variable_len(rest) {
                marks.push(&rest[..n]);
                marked.push_str(&format!("MSMARK{}X", marks.len() - 1));
                i += n;
                continue;
            }
            if rest.starts_with("$$") {
                marks.push("$$");
                marked.push_str(&format!("MSMARK{}X", marks.len() - 1));
                i += 2;
                continue;
            }
        }
        let c = rest.chars().next()?;
        marked.push(c);
        i += c.len_utf8();
    }
    let u = resolve_ref(&marked, base)?;
    let mut parts: Vec<String> = Vec::new();
    let mut slots: Vec<UrlSlot> = Vec::new();
    let mut text = String::new();
    let mut last = 0;
    let mut from = 0;
    while let Some(i) = u[from..].find("MSMARK") {
        let start = from + i;
        let digits = u[start + 6..]
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count();
        if digits == 0 || u.as_bytes().get(start + 6 + digits) != Some(&b'X') {
            from = start + 1;
            continue;
        }
        let end = start + 6 + digits + 1;
        let mark = *marks.get(u[start + 6..start + 6 + digits].parse::<usize>().ok()?)?;
        text.push_str(&u[last..start]);
        last = end;
        from = end;
        if mark == "$$" {
            text.push('$');
            continue;
        }
        // 置き換える部分が隣り合うものは、区切りが決まらないので使わない
        if !slots.is_empty() && text.is_empty() {
            return None;
        }
        parts.push(std::mem::take(&mut text));
        slots.push(if mark.starts_with("$RepresentationID") {
            if ids.is_empty() {
                UrlSlot::Any
            } else {
                UrlSlot::Id
            }
        } else {
            UrlSlot::Num
        });
        if slots.len() > MAX_SLOTS {
            return None;
        }
    }
    parts.push(text + &u[last..]);
    if slots.is_empty() {
        return Some(Template::Url(parts.swap_remove(0)));
    }
    // 置き換える部分はパスより後ろだけ
    let origin = Url::parse(&u).ok()?.origin().ascii_serialization();
    if !parts[0].starts_with(&format!("{origin}/")) {
        return None;
    }
    Some(Template::Pattern(UrlPattern {
        parts,
        slots,
        ids: ids.clone(),
        id_lengths: id_lengths.clone(),
    }))
}

/// MPEG-DASH の MPD に書かれた URL と、SegmentTemplate から決まる URL の形。BaseURL は入れ子ごとに解決する
pub fn dash_refs(xml: &str, base: &str) -> ManifestRefs {
    let mut urls = Vec::new();
    let mut seen = HashSet::new();
    let mut templates: Vec<(String, String)> = Vec::new();
    let mut ids = IdSet::default();
    let mut stack = vec![Element {
        name: "#root".to_string(),
        base: base.to_string(),
        text: String::new(),
        text_units: 0,
    }];
    let mut count = 0;
    xml_tokens(xml, |t| {
        count += 1;
        if count > MAX_TOKENS {
            return false;
        }
        let top = stack.len() - 1;
        let tag = match t {
            // 文字 (BaseURL の中身だけを使う)。文字の参照は、本文のものだけ解く (CDATA はそのまま)
            XmlToken::Text(text) => {
                append_base_text(&mut stack[top], text, true);
                return true;
            }
            XmlToken::Cdata(text) => {
                append_base_text(&mut stack[top], text, false);
                return true;
            }
            XmlToken::Tag(tag) => tag,
        };
        if tag.closing {
            let mut i = stack.len() - 1;
            while i > 0 && stack[i].name != tag.name {
                i -= 1;
            }
            if i == 0 {
                return true;
            }
            let closed = stack.swap_remove(i);
            stack.truncate(i);
            if closed.name == "BaseURL" {
                let parent = stack.last_mut().expect("root stays");
                if let Some(u) = resolve_ref(&closed.text, &parent.base) {
                    parent.base = u;
                }
            }
            return true;
        }
        let inherited = stack[top].base.clone();
        let name = tag.name;
        let a = &tag.attrs;
        if name == "Representation"
            && let Some(id) = a.get("id")
            && !id.is_empty()
            && utf16_len(id) <= MAX_ID_LENGTH
            && ids.len() < MAX_IDS
        {
            ids.insert(id);
        }
        let mut add = |value: Option<&String>| {
            let Some(value) = value.filter(|v| !v.is_empty()) else {
                return;
            };
            if value.contains('$') {
                if templates.len() < MAX_PATTERNS {
                    templates.push((value.clone(), inherited.clone()));
                }
            } else if urls.len() < MAX_CHILDREN {
                push(&mut urls, &mut seen, resolve_ref(value, &inherited));
            }
        };
        match name {
            "SegmentTemplate" => {
                add(a.get("media"));
                add(a.get("initialization"));
                add(a.get("index"));
            }
            "SegmentURL" => {
                add(a.get("media"));
                add(a.get("index"));
            }
            "Initialization" | "RepresentationIndex" => add(a.get("sourceURL")),
            // Location と PatchLocation は、読み直す先なので使わない
            _ => {}
        }
        if !tag.empty {
            if stack.len() >= MAX_DEPTH {
                return false;
            }
            stack.push(Element {
                name: name.to_string(),
                base: inherited,
                text: String::new(),
                text_units: 0,
            });
        }
        true
    });
    let ids = Arc::new(ids);
    let mut lengths: Vec<usize> = ids.order.iter().map(String::len).collect();
    lengths.sort_unstable_by(|a, b| b.cmp(a));
    lengths.dedup();
    let id_lengths = Arc::new(lengths);
    let mut patterns = Vec::new();
    for (template, base) in &templates {
        match template_pattern(template, base, &ids, &id_lengths) {
            Some(Template::Url(u)) => push(&mut urls, &mut seen, Some(u)),
            Some(Template::Pattern(p)) => patterns.push(p),
            None => {}
        }
    }
    ManifestRefs {
        urls,
        patterns,
        playlists: Vec::new(),
    }
}

/// url の前と後ろが、形の固定の部分に合うか (ここで落ちるものがほとんどなので先に見る)
pub fn pattern_edges_match(p: &UrlPattern, url: &str) -> bool {
    let first = &p.parts[0];
    let last = &p.parts[p.slots.len()];
    url.len() >= first.len() + last.len()
        && url.starts_with(first.as_str())
        && url.ends_with(last.as_str())
}

fn leading_digits(s: &str) -> usize {
    s.bytes().take_while(u8::is_ascii_digit).count()
}

/// 置き換える部分の終わりの位置。合わなければ None。`next` は次の固定の部分 (最後の置き換える部分なら None)
fn slot_end(
    p: &UrlPattern,
    slot: UrlSlot,
    url: &str,
    at: usize,
    stop: usize,
    next: Option<&str>,
) -> Option<usize> {
    let bytes = url.as_bytes();
    let fits = |end: usize| match next {
        None => end == stop,
        Some(n) => bytes
            .get(end..)
            .is_some_and(|rest| rest.starts_with(n.as_bytes())),
    };
    match slot {
        UrlSlot::Num => {
            let mut end = at;
            while end < stop && bytes[end].is_ascii_digit() {
                end += 1;
            }
            // 次の固定の部分が数字で始まるなら、その分を戻す
            if let Some(n) = next {
                end = end.checked_sub(leading_digits(n))?;
            }
            (end > at && end - at <= 20 && fits(end)).then_some(end)
        }
        UrlSlot::Id => {
            // 長い id から試す (ほかの id で始まる id があっても、次の固定の部分まで合うものを選ぶ)
            for &length in p.id_lengths.iter() {
                let end = at + length;
                if end <= stop && url.get(at..end).is_some_and(|s| p.ids.contains(s)) && fits(end) {
                    return Some(end);
                }
            }
            None
        }
        UrlSlot::Any => {
            let end = match next {
                None => stop,
                Some(n) => at + url.get(at..)?.find(n)?,
            };
            if end > stop {
                return None;
            }
            if bytes[at.min(end)..end]
                .iter()
                .any(|&c| c == b'/' || c == b'?' || c == b'#')
            {
                return None;
            }
            Some(end)
        }
    }
}

/// url が形に合うか。前から 1 度だけ見て戻らない
pub fn match_pattern(p: &UrlPattern, url: &str) -> bool {
    if !pattern_edges_match(p, url) {
        return false;
    }
    let count = p.slots.len();
    let stop = url.len() - p.parts[count].len();
    let mut at = p.parts[0].len();
    for i in 0..count {
        let next = (i + 1 < count).then(|| p.parts[i + 1].as_str());
        let Some(end) = slot_end(p, p.slots[i], url, at, stop, next) else {
            return false;
        };
        at = end + next.map_or(0, str::len);
    }
    at == stop
}

// ---------------------------------------------------------------- glTF

/// glTF の JSON から `buffers` と `images` の `uri` だけを読み取る。
/// 数の値は読み飛ばすだけにする (JavaScript の `JSON.parse` は `1e400` を Infinity として受け入れるが、
/// serde_json の `Value` は範囲外の数で失敗するため)。
/// 形の合わない値 (配列でない `buffers`、オブジェクトでない要素、文字列でない `uri`) は読み飛ばす。同じキーが重なったら後のものを使う
mod gltf_json {
    use std::fmt;

    use serde::de::{Deserialize, Deserializer, Error, IgnoredAny, MapAccess, SeqAccess, Visitor};

    /// 値の形に関わらず受け取る訪問者の、共通の部分 (数・真偽・null・文字列は `$other` の値にする)
    macro_rules! scalars {
        ($ty:ty, $other:expr, $str:ident => $on_str:expr) => {
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a JSON value")
            }
            fn visit_str<E: Error>(self, $str: &str) -> Result<$ty, E> {
                Ok($on_str)
            }
            fn visit_bool<E: Error>(self, _: bool) -> Result<$ty, E> {
                Ok($other)
            }
            fn visit_i64<E: Error>(self, _: i64) -> Result<$ty, E> {
                Ok($other)
            }
            fn visit_u64<E: Error>(self, _: u64) -> Result<$ty, E> {
                Ok($other)
            }
            fn visit_f64<E: Error>(self, _: f64) -> Result<$ty, E> {
                Ok($other)
            }
            fn visit_unit<E: Error>(self) -> Result<$ty, E> {
                Ok($other)
            }
        };
    }

    fn drain_seq<'de, A: SeqAccess<'de>>(a: &mut A) -> Result<(), A::Error> {
        while a.next_element::<IgnoredAny>()?.is_some() {}
        Ok(())
    }

    fn drain_map<'de, A: MapAccess<'de>>(a: &mut A) -> Result<(), A::Error> {
        while a.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(())
    }

    /// 文字列なら Some、ほかの値は読み飛ばして None
    struct MaybeString(Option<String>);

    impl<'de> Deserialize<'de> for MaybeString {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct V;
            impl<'de> Visitor<'de> for V {
                type Value = MaybeString;
                scalars!(MaybeString, MaybeString(None), v => MaybeString(Some(v.to_string())));
                fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                    drain_seq(&mut a)?;
                    Ok(MaybeString(None))
                }
                fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                    drain_map(&mut a)?;
                    Ok(MaybeString(None))
                }
            }
            d.deserialize_any(V)
        }
    }

    /// 1 つの要素の `uri` (オブジェクトでなければ None)
    struct Item(Option<String>);

    impl<'de> Deserialize<'de> for Item {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct V;
            impl<'de> Visitor<'de> for V {
                type Value = Item;
                scalars!(Item, Item(None), _v => Item(None));
                fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                    drain_seq(&mut a)?;
                    Ok(Item(None))
                }
                fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                    let mut uri = None;
                    while let Some(key) = a.next_key::<String>()? {
                        if key == "uri" {
                            uri = a.next_value::<MaybeString>()?.0;
                        } else {
                            a.next_value::<IgnoredAny>()?;
                        }
                    }
                    Ok(Item(uri))
                }
            }
            d.deserialize_any(V)
        }
    }

    /// `uri` の並び (配列でなければ空)
    #[derive(Default)]
    struct List(Vec<Option<String>>);

    impl<'de> Deserialize<'de> for List {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct V;
            impl<'de> Visitor<'de> for V {
                type Value = List;
                scalars!(List, List(Vec::new()), _v => List(Vec::new()));
                fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                    let mut out = Vec::new();
                    while let Some(Item(uri)) = a.next_element::<Item>()? {
                        out.push(uri);
                    }
                    Ok(List(out))
                }
                fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                    drain_map(&mut a)?;
                    Ok(List(Vec::new()))
                }
            }
            d.deserialize_any(V)
        }
    }

    struct Root(List, List);

    impl<'de> Deserialize<'de> for Root {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct V;
            impl<'de> Visitor<'de> for V {
                type Value = Root;
                fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                    f.write_str("a JSON object")
                }
                fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                    let (mut buffers, mut images) = (List::default(), List::default());
                    while let Some(key) = a.next_key::<String>()? {
                        match key.as_str() {
                            "buffers" => buffers = a.next_value()?,
                            "images" => images = a.next_value()?,
                            _ => {
                                a.next_value::<IgnoredAny>()?;
                            }
                        }
                    }
                    Ok(Root(buffers, images))
                }
            }
            d.deserialize_map(V)
        }
    }

    /// (buffers の uri、images の uri)。オブジェクトの JSON として読めなければ None
    #[allow(clippy::type_complexity)]
    pub fn uris(json: &str) -> Option<(Vec<Option<String>>, Vec<Option<String>>)> {
        let Root(buffers, images) = serde_json::from_str(json).ok()?;
        Some((buffers.0, images.0))
    }
}

/// .gltf (JSON) が参照するファイル (buffers と images の uri)。data: の URL は除く
pub fn gltf_refs(json: &str, base: &str) -> ManifestRefs {
    let mut urls = Vec::new();
    let mut seen = HashSet::new();
    let Some((buffers, images)) = gltf_json::uris(json.strip_prefix('\u{feff}').unwrap_or(json))
    else {
        return ManifestRefs::default();
    };
    for uri in buffers.into_iter().chain(images) {
        if urls.len() >= MAX_CHILDREN {
            break;
        }
        let Some(uri) = uri else {
            continue;
        };
        if starts_with_ci(&uri, "data:") {
            continue;
        }
        push(&mut urls, &mut seen, resolve_ref(&uri, base));
    }
    ManifestRefs {
        urls,
        patterns: Vec::new(),
        playlists: Vec::new(),
    }
}

/// `/"asset"\s*:/`
fn has_asset_key(text: &str) -> bool {
    let mut from = 0;
    while let Some(i) = text[from..].find("\"asset\"") {
        let after = &text[from + i + 7..];
        if after[skip_space(after, 0)..].starts_with(':') {
            return true;
        }
        from += i + 1;
    }
    false
}

/// 本文から種類を見分けて、参照している URL を取り出す。再生リストでも glTF でもなければ None
pub fn manifest_refs(text: &str, base: &str) -> Option<ManifestRefs> {
    if is_hls(text) {
        return Some(hls_refs(text, base));
    }
    if is_dash(text) {
        return Some(dash_refs(text, base));
    }
    if after_bom_and_space(text).starts_with('{') && has_asset_key(text) {
        return Some(gltf_refs(text, base));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: &str = "https://cdn.example.com/live/main.m3u8";

    #[test]
    fn hls_playlists_and_keys() {
        let text = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k.bin\"\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv/a.m3u8\nseg.ts#f\n";
        let r = manifest_refs(text, B).unwrap();
        assert_eq!(
            r.urls,
            [
                "https://cdn.example.com/live/k.bin",
                "https://cdn.example.com/live/v/a.m3u8",
                "https://cdn.example.com/live/seg.ts"
            ]
        );
        assert_eq!(r.playlists, ["https://cdn.example.com/live/v/a.m3u8"]);
    }

    #[test]
    fn dash_template_matches_numbers() {
        let mpd = "<MPD><Period><Representation id=\"v1\"/><SegmentTemplate media=\"$RepresentationID$/s-$Number$.m4s\"/></Period></MPD>";
        let r = manifest_refs(mpd, "https://c.example.com/d/m.mpd").unwrap();
        assert_eq!(r.patterns.len(), 1);
        let p = &r.patterns[0];
        assert!(match_pattern(p, "https://c.example.com/d/v1/s-12.m4s"));
        assert!(!match_pattern(p, "https://c.example.com/d/v2/s-12.m4s"));
        assert!(!match_pattern(p, "https://c.example.com/d/v1/s-.m4s"));
        assert!(!match_pattern(p, "https://evil.example.com/d/v1/s-1.m4s"));
    }

    #[test]
    fn gltf_references() {
        let json = "{\"asset\":{},\"buffers\":[{\"uri\":\"a.bin\"},{\"uri\":\"data:x\"}]}";
        let r = manifest_refs(json, "https://c.example.com/m/x.gltf").unwrap();
        assert_eq!(r.urls, ["https://c.example.com/m/a.bin"]);
        assert!(manifest_refs("{}", "https://c.example.com/").is_none());
    }

    #[test]
    fn entities() {
        assert_eq!(decode_xml("a&amp;b&#x41;&#66;&bogus;&#0;"), "a&bAB&bogus;");
        assert_eq!(decode_xml("&lt;&gt;&quot;&apos;&"), "<>\"'&");
    }

    #[test]
    fn many_tokens_stop() {
        let big = format!(
            "<MPD>{}<SegmentURL media=\"late.m4s\"/></MPD>",
            "<x/>".repeat(250_000)
        );
        let r = dash_refs(&big, "https://c.example.com/m.mpd");
        assert!(r.urls.is_empty());
    }
}
