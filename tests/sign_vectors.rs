//! 看板 (MarumaSign) の共有ロジックの試験。
//! `tests/vectors/marumasign-shared.json` (tools/gen-sign-vectors.mjs が TypeScript から作る) を読み、
//! Rust への移植が TypeScript と同じ結果を返すことを確かめる。
#![allow(dead_code)]

use std::sync::OnceLock;

use serde_json::Value;
use toro_api::map3d::sign::core::{extension_of, safe_url, sniff, utf16_len, without_fragment};
use toro_api::map3d::sign::manifest::{
    ManifestRefs, UrlSlot, manifest_refs, match_pattern, pattern_edges_match,
};
use toro_api::map3d::sign::xml_head::{skip_xml_head, starts_after_xml_head, tag_follows};

pub fn vectors() -> &'static Value {
    static VECTORS: OnceLock<Value> = OnceLock::new();
    VECTORS.get_or_init(|| {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/vectors/marumasign-shared.json"
        );
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    })
}

pub fn cases(name: &str) -> &'static Vec<Value> {
    vectors()[name]
        .as_array()
        .unwrap_or_else(|| panic!("ベクタに {name} が無い"))
}

/// `{head, unit, count, tail}` の形を文字列に広げる (unit の `{i}` は 0 から数える番号)
pub fn expand(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    let mut out = v["head"].as_str().unwrap_or("").to_string();
    let unit = v["unit"].as_str().unwrap();
    for i in 0..v["count"].as_u64().unwrap() {
        out.push_str(&unit.replace("{i}", &i.to_string()));
    }
    out.push_str(v["tail"].as_str().unwrap_or(""));
    out
}

pub fn from_hex(h: &str) -> Vec<u8> {
    assert!(h.len().is_multiple_of(2));
    (0..h.len() / 2)
        .map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap())
        .collect()
}

/// 16 進の `{head, unit, count, tail}` をバイト列に広げる
fn expand_hex(v: &Value) -> Vec<u8> {
    let mut out = from_hex(v["head"].as_str().unwrap());
    let unit = from_hex(v["unit"].as_str().unwrap());
    for _ in 0..v["count"].as_u64().unwrap() {
        out.extend_from_slice(&unit);
    }
    out.extend(from_hex(v["tail"].as_str().unwrap()));
    out
}

#[test]
fn extension_of_matches() {
    for c in cases("extensionOf") {
        let url = c["url"].as_str().unwrap();
        assert_eq!(extension_of(url), c["expect"].as_str().unwrap(), "{url:?}");
    }
}

#[test]
fn safe_url_matches() {
    for c in cases("safeUrl") {
        let https_only = c["httpsOnly"].as_bool().unwrap();
        let got = match c["value"].as_str() {
            Some(value) => safe_url(value, https_only),
            None => None, // 文字列でない値は、必ず None
        };
        let want = c["expect"].as_str().map(str::to_string);
        assert_eq!(got, want, "{} (https_only={https_only})", c["value"]);
    }
}

#[test]
fn without_fragment_matches() {
    for c in cases("withoutFragment") {
        let value = c["value"].as_str().unwrap();
        assert_eq!(without_fragment(value), c["expect"].as_str().unwrap());
    }
}

/// 位置は JavaScript の文字列の位置 (UTF-16 の単位) で比べる
#[test]
fn skip_xml_head_matches() {
    for c in cases("skipXmlHead") {
        let text = expand(&c["text"]);
        let decl = c["decl"].as_bool().unwrap();
        let doctype = c["doctype"].as_bool().unwrap();
        let got = skip_xml_head(&text, decl, doctype).map(|at| utf16_len(&text[..at]) as u64);
        assert_eq!(got, c["expect"].as_u64(), "{:?}", c["text"]);
    }
}

#[test]
fn starts_after_xml_head_matches() {
    for c in cases("startsAfterXmlHead") {
        let text = expand(&c["text"]);
        let decl = c["decl"].as_bool().unwrap();
        let doctype = c["doctype"].as_bool().unwrap();
        let got = match c["pattern"].as_str().unwrap() {
            "mpd" => starts_after_xml_head(&text, |r| tag_follows(r, "MPD", false), decl, doctype),
            "svg" => starts_after_xml_head(&text, |r| tag_follows(r, "svg", true), decl, doctype),
            other => panic!("知らない pattern: {other}"),
        };
        assert_eq!(got, c["expect"].as_bool().unwrap(), "{:?}", c["text"]);
    }
}

fn slot_name(slot: UrlSlot) -> &'static str {
    match slot {
        UrlSlot::Id => "id",
        UrlSlot::Num => "num",
        UrlSlot::Any => "any",
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

fn check_refs(got: &ManifestRefs, want: &Value, what: &str) {
    assert_eq!(got.urls, strings(&want["urls"]), "urls: {what}");
    assert_eq!(
        got.playlists,
        strings(&want["playlists"]),
        "playlists: {what}"
    );
    let patterns = want["patterns"].as_array().unwrap();
    assert_eq!(got.patterns.len(), patterns.len(), "patterns 件数: {what}");
    for (p, w) in got.patterns.iter().zip(patterns) {
        assert_eq!(p.parts, strings(&w["parts"]), "parts: {what}");
        let slots: Vec<&str> = p.slots.iter().map(|s| slot_name(*s)).collect();
        assert_eq!(slots, strings(&w["slots"]), "slots: {what}");
        assert_eq!(p.ids.order, strings(&w["ids"]), "ids: {what}");
        // 長さは、TypeScript は UTF-16 の文字数、Rust は UTF-8 のバイト数 (URL のバイト列と照らすため)。
        // 同じ id の組から作った、バイト数 (重複なし、長い順) と比べる
        let mut lengths: Vec<u64> = strings(&w["ids"])
            .iter()
            .map(|id| id.len() as u64)
            .collect();
        lengths.sort_unstable_by(|a, b| b.cmp(a));
        lengths.dedup();
        let got_lengths: Vec<u64> = p.id_lengths.iter().map(|n| *n as u64).collect();
        assert_eq!(got_lengths, lengths, "idLengths: {what}");
    }
}

fn check_summary(got: &ManifestRefs, want: &Value, what: &str) {
    let ends = |list: &[String], count: &str, first: &str, last: &str| {
        assert_eq!(
            list.len() as u64,
            want[count].as_u64().unwrap(),
            "{count}: {what}"
        );
        assert_eq!(
            list.first().map(String::as_str),
            want[first].as_str(),
            "{first}: {what}"
        );
        assert_eq!(
            list.last().map(String::as_str),
            want[last].as_str(),
            "{last}: {what}"
        );
    };
    ends(&got.urls, "urlsCount", "urlsFirst", "urlsLast");
    ends(
        &got.playlists,
        "playlistsCount",
        "playlistsFirst",
        "playlistsLast",
    );
    // パターンは、件数と最初・最後の形だけ
    assert_eq!(
        got.patterns.len() as u64,
        want["patternsCount"].as_u64().unwrap(),
        "patternsCount: {what}"
    );
    for (p, key) in [
        (got.patterns.first(), "patternsFirst"),
        (got.patterns.last(), "patternsLast"),
    ] {
        match (p, want[key].is_null()) {
            (None, true) => {}
            (Some(p), false) => {
                let w = &want[key];
                assert_eq!(p.parts, strings(&w["parts"]), "{key}.parts: {what}");
                assert_eq!(p.ids.order, strings(&w["ids"]), "{key}.ids: {what}");
            }
            _ => panic!("{key} が合わない: {what}"),
        }
    }
}

#[test]
fn manifest_refs_match() {
    for c in cases("manifestRefs") {
        let text = expand(&c["text"]);
        let base = c["base"].as_str().unwrap();
        let what = format!("{:.80}", c["text"].to_string());
        let got = manifest_refs(&text, base);
        if let Some(summary) = c.get("expectSummary") {
            match (&got, summary.is_null()) {
                (None, true) => {}
                (Some(g), false) => check_summary(g, summary, &what),
                _ => panic!("None かどうかが合わない: {what}"),
            }
        } else {
            let want = &c["expect"];
            match (&got, want.is_null()) {
                (None, true) => {}
                (Some(g), false) => check_refs(g, want, &what),
                _ => panic!("None かどうかが合わない: {what}"),
            }
        }
        if let (Some(g), Some(probes)) = (&got, c.get("probes")) {
            let matches = c["matches"].as_array().unwrap();
            let edges = c["edges"].as_array().unwrap();
            for (i, probe) in probes.as_array().unwrap().iter().enumerate() {
                let url = probe.as_str().unwrap();
                let m: Vec<bool> = g.patterns.iter().map(|p| match_pattern(p, url)).collect();
                let e: Vec<bool> = g
                    .patterns
                    .iter()
                    .map(|p| pattern_edges_match(p, url))
                    .collect();
                let want_m: Vec<bool> = matches[i]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|b| b.as_bool().unwrap())
                    .collect();
                let want_e: Vec<bool> = edges[i]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|b| b.as_bool().unwrap())
                    .collect();
                assert_eq!(m, want_m, "match_pattern {url:?}: {what}");
                assert_eq!(e, want_e, "pattern_edges_match {url:?}: {what}");
            }
        }
    }
}

#[test]
fn sniff_matches() {
    for c in cases("sniff") {
        let head = match c.get("headParts") {
            Some(parts) => expand_hex(parts),
            None => from_hex(c["head"].as_str().unwrap()),
        };
        let ty = c["type"].as_str().unwrap();
        let ext = c["ext"].as_str().unwrap();
        let got = sniff(&head, ty, ext);
        let want = &c["expect"];
        let what = format!("{} type={ty:?} ext={ext:?} len={}", c["name"], head.len());
        match (got, want.is_null()) {
            (None, true) => {}
            (Some(g), false) => {
                assert_eq!(g.route, want["route"].as_str().unwrap(), "route: {what}");
                assert_eq!(g.format, want["format"].as_str().unwrap(), "format: {what}");
                assert_eq!(g.container, want["container"].as_str(), "container: {what}");
            }
            (g, _) => panic!("None かどうかが合わない: {what}: {g:?} / {want}"),
        }
    }
}
