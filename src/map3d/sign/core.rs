//! 看板の URL の確かめと、中身 (先頭のバイト列) からの種類の判別。
//!
//! 正本は TORO-3D-WebMap の `app/bluemap/marumasign/core.ts` (サーバーで使う部分だけを移した)。
//! `tests/vectors/marumasign-shared.json` の入出力と同じ結果になることを `tests/sign_vectors.rs` が確かめる。
//! 外から来た本文に使うので、正規表現は使わず (この crate にも無い)、前から読む。

use url::Url;

use super::xml_head::{is_js_space, skip_space, starts_after_xml_head, tag_follows};

pub const MAX_URL_LENGTH: usize = 2000;
pub const MAX_IMAGE_BYTES: u64 = 12 * 1024 * 1024;
/// PDF の大きさの上限 (MarumaSign 本体の PdfFile.MAX_SIZE と同じ)
pub const MAX_PDF_BYTES: u64 = 64 * 1024 * 1024;

/// JavaScript の `String.length` (UTF-16 の単位の数)
pub fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// http(s) の URL として使ってよければ正規化した文字列、だめなら None。`https_only` なら https のみ
pub fn safe_url(value: &str, https_only: bool) -> Option<String> {
    if value.is_empty() || utf16_len(value) > MAX_URL_LENGTH {
        return None;
    }
    for c in value.chars() {
        let c = c as u32;
        if c <= 0x20
            || c == 0x7F
            || c == 0x22
            || c == 0x27
            || c == 0x3C
            || c == 0x3E
            || c == 0x5C
            || c == 0x60
        {
            return None;
        }
    }
    let u = Url::parse(value).ok()?;
    match u.scheme() {
        "https" => {}
        "http" if !https_only => {}
        _ => return None,
    }
    if !u.username().is_empty() || u.password().is_some_and(|p| !p.is_empty()) {
        return None;
    }
    if !matches!(u.port(), None | Some(80 | 443 | 8080 | 8443)) {
        return None;
    }
    let host = u.host_str()?.to_lowercase();
    if host.is_empty() || !host.contains('.') {
        return None;
    }
    if host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host.ends_with(".lan")
        || host.ends_with(".home.arpa")
    {
        return None;
    }
    // IP アドレスそのもののホストは使わない
    if host.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        || host.contains(':')
        || host.starts_with('[')
    {
        return None;
    }
    Some(u.to_string())
}

/// URL の「#」から後ろを除く
pub fn without_fragment(url: &str) -> &str {
    match url.find('#') {
        Some(i) => &url[..i],
        None => url,
    }
}

/// URL のパスの拡張子 (小文字。なければ空文字)
pub fn extension_of(url: &str) -> String {
    let Ok(u) = Url::parse(url) else {
        return String::new();
    };
    let path = u.path();
    let name = &path[path.rfind('/').map_or(0, |i| i + 1)..];
    match name.rfind('.') {
        Some(dot) if dot + 1 < name.len() => name[dot + 1..].to_lowercase(),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------- 中身の判別

/// 中身から分かった形式
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sniffed {
    /// image / video / hls / dash / flv / mjpeg / pdf / probe / model / splat / structure
    pub route: &'static str,
    pub format: &'static str,
    pub container: Option<&'static str>,
}

fn s(route: &'static str, format: &'static str) -> Option<Sniffed> {
    Some(Sniffed {
        route,
        format,
        container: None,
    })
}

fn sc(route: &'static str, format: &'static str, container: &'static str) -> Option<Sniffed> {
    Some(Sniffed {
        route,
        format,
        container: Some(container),
    })
}

fn ascii(b: &[u8], at: usize, text: &[u8]) -> bool {
    b.len() >= at + text.len() && &b[at..at + text.len()] == text
}

/// 先頭 `limit` バイトのうち、`text` が最初に出てくる位置
fn find_ascii(b: &[u8], text: &[u8], limit: usize) -> Option<usize> {
    let m = b.len().min(limit);
    if m < text.len() {
        return None;
    }
    (0..=m - text.len()).find(|&i| ascii(b, i, text))
}

/// 1 バイトを 1 文字にした文字列 (先頭 `limit` バイトまで)
fn text_head(b: &[u8], limit: usize) -> String {
    b.iter().take(limit).map(|&x| x as char).collect()
}

fn sub(b: &[u8], from: usize, to: usize) -> &[u8] {
    let to = to.min(b.len());
    let from = from.min(to);
    &b[from..to]
}

fn u16_at(b: &[u8], p: usize, little: bool) -> u32 {
    let x = |i: usize| u32::from(b.get(i).copied().unwrap_or(0));
    if little {
        x(p) | (x(p + 1) << 8)
    } else {
        (x(p) << 8) | x(p + 1)
    }
}

fn i32le(b: &[u8], p: usize) -> i32 {
    let x = |i: usize| u32::from(b.get(i).copied().unwrap_or(0));
    (x(p) | (x(p + 1) << 8) | (x(p + 2) << 16) | (x(p + 3) << 24)) as i32
}

const STRUCTURE_EXT: [&str; 5] = ["schem", "litematic", "nbt", "mcstructure", "schematic"];
const TGA_EXT: [&str; 5] = ["tga", "icb", "vda", "vst", "targa"];
const AUDIO_ONLY_EXT: [&str; 17] = [
    "wma", "amr", "aif", "aiff", "aifc", "au", "snd", "caf", "ape", "wv", "mpc", "ac3", "eac3",
    "dts", "tta", "voc", "ra",
];
const ASF_GUID: [u8; 16] = [
    0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C,
];

/// 前置き (`<?xml ...>`、コメント、DOCTYPE) のあとが `<svg` か
fn starts_with_svg(text: &str) -> bool {
    starts_after_xml_head(text, |rest| tag_follows(rest, "svg", true), true, true)
}

/// `^(ï»¿)?\s*\{` (先頭の BOM は、1 バイトを 1 文字にした形)
fn starts_with_json_object(text: &str) -> bool {
    let text = text.strip_prefix("\u{ef}\u{bb}\u{bf}").unwrap_or(text);
    text[skip_space(text, 0)..].starts_with('{')
}

/// `^\s*(<\?xml[^>]*>\s*)?<MPD[\s>]`
fn starts_with_mpd(text: &str) -> bool {
    let at = skip_space(text, 0);
    let rest = &text[at..];
    if rest.starts_with("<?xml") {
        let Some(end) = rest.find('>') else {
            return false;
        };
        let after = &rest[end + 1..];
        return tag_follows(&after[skip_space(after, 0)..], "MPD", false);
    }
    tag_follows(rest, "MPD", false)
}

/// `^\/\*\s*XPM\s*\*\/`
fn starts_with_xpm(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("/*") else {
        return false;
    };
    let rest = &rest[skip_space(rest, 0)..];
    let Some(rest) = rest.strip_prefix("XPM") else {
        return false;
    };
    rest[skip_space(rest, 0)..].starts_with("*/")
}

/// `^\s*#define\s+\S+_width\s+\d+`
fn starts_with_xbm_width(text: &str) -> bool {
    let Some(rest) = text[skip_space(text, 0)..].strip_prefix("#define") else {
        return false;
    };
    let after = skip_space(rest, 0);
    if after == 0 {
        return false;
    }
    let rest = &rest[after..];
    let token_end = rest.find(is_js_space).unwrap_or(rest.len());
    let token = &rest[..token_end];
    if token.len() <= "_width".len() || !token.ends_with("_width") {
        return false;
    }
    let rest = &rest[token_end..];
    let after = skip_space(rest, 0);
    after > 0 && rest[after..].starts_with(|c: char| c.is_ascii_digit())
}

/// `"means"\s*:\s*\{[^{}]*"files"\s*:\s*\[`
fn has_sog_means(text: &str) -> bool {
    let mut from = 0;
    while let Some(i) = text[from..].find("\"means\"") {
        let start = from + i;
        from = start + 1;
        let rest = &text[start + 7..];
        let rest = &rest[skip_space(rest, 0)..];
        let Some(rest) = rest.strip_prefix(':') else {
            continue;
        };
        let rest = &rest[skip_space(rest, 0)..];
        let Some(rest) = rest.strip_prefix('{') else {
            continue;
        };
        let run = &rest[..rest.find(['{', '}']).unwrap_or(rest.len())];
        let mut search = 0;
        while let Some(j) = run[search..].find("\"files\"") {
            let after = &run[search + j + 7..];
            let after = &after[skip_space(after, 0)..];
            if let Some(after) = after.strip_prefix(':')
                && after[skip_space(after, 0)..].starts_with('[')
            {
                return true;
            }
            search += j + 1;
        }
    }
    false
}

/// `property\s+\S+\s+(f_dc_0|packed_position)\s*$` (複数行の指定。`header` のどこから始まってもよい)
fn has_gaussian_property(header: &str) -> bool {
    let mut from = 0;
    while let Some(i) = header[from..].find("property") {
        let start = from + i;
        from = start + 1;
        let rest = &header[start + 8..];
        let ws = skip_space(rest, 0);
        if ws == 0 {
            continue;
        }
        let rest = &rest[ws..];
        let token = rest.find(is_js_space).unwrap_or(rest.len());
        if token == 0 {
            continue;
        }
        let rest = &rest[token..];
        let ws = skip_space(rest, 0);
        if ws == 0 {
            continue;
        }
        let rest = &rest[ws..];
        let Some(rest) = rest
            .strip_prefix("f_dc_0")
            .or_else(|| rest.strip_prefix("packed_position"))
        else {
            continue;
        };
        // `\s*$`: 空白のあとが行の終わり (改行を含む空白が続くか、文字列の終わり)
        let run = &rest[..skip_space(rest, 0)];
        if run.len() == rest.len() || run.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
            return true;
        }
    }
    false
}

/// ISO BMFF (ftyp) の画像の形式 (主なブランドと互換ブランドを見る)
fn iso_image(b: &[u8]) -> Option<&'static str> {
    let word = |i: usize| u32::from(b.get(i).copied().unwrap_or(0));
    let size = (word(0) << 24) | (word(1) << 16) | (word(2) << 8) | word(3);
    let end = if size > 0 {
        b.len().min(size as usize)
    } else {
        b.len()
    };
    let (mut avif, mut heic, mut avis, mut sequence) = (false, false, false, false);
    let mut i = 8;
    while i + 4 <= end {
        if i != 12 {
            match &b[i..i + 4] {
                b"avis" => avis = true,
                b"msf1" | b"hevc" | b"hevx" => sequence = true,
                b"avif" => avif = true,
                b"heic" | b"heix" | b"heim" | b"heis" | b"mif1" => heic = true,
                _ => {}
            }
        }
        i += 4;
    }
    if avis {
        return Some("avif");
    }
    if sequence {
        return None;
    }
    if avif {
        Some("avif")
    } else if heic {
        Some("heic")
    } else {
        None
    }
}

/// 先頭が NBT (名前つきの複合タグ) らしいか。`little` はリトルエンディアン (Bedrock Edition の .mcstructure)
pub fn looks_like_nbt(b: &[u8], little: bool) -> bool {
    if b.len() < 6 || b[0] != 10 {
        return false;
    }
    let name_length = u16_at(b, 1, little) as usize;
    if name_length > 64 {
        return false;
    }
    let mut p = 3;
    for _ in 0..name_length {
        if p >= b.len() {
            return true;
        }
        if !(0x20..=0x7E).contains(&b[p]) {
            return false;
        }
        p += 1;
    }
    if p + 3 > b.len() {
        return true;
    }
    let kind = b[p];
    if !(1..=12).contains(&kind) {
        return false;
    }
    let child_length = u16_at(b, p + 1, little) as usize;
    if child_length == 0 || child_length > 64 {
        return false;
    }
    for i in 0..child_length {
        if p + 3 + i >= b.len() {
            break;
        }
        if !(0x20..=0x7E).contains(&b[p + 3 + i]) {
            return false;
        }
    }
    true
}

/// ガウシアンスプラッティングの形式 (MarumaSign 本体の SplatLoader.isSplat と同じ判定)。違えば None
pub fn sniff_splat(b: &[u8], ext: &str) -> Option<&'static str> {
    if ascii(b, 0, b"ply\n") || ascii(b, 0, b"ply\r\n") {
        let head = text_head(b, b.len().min(64 * 1024));
        let header = head
            .find("end_header")
            .map_or(&head[..], |end| &head[..end]);
        return has_gaussian_property(header).then_some("ply");
    }
    if ascii(b, 0, b"NGSP") && b.len() >= 32 && i32le(b, 4) == 4 {
        return Some("spz");
    }
    if b.starts_with(&[0x50, 0x4B, 3, 4]) {
        if ext == "sog" {
            return Some("sog");
        }
        if b.len() >= 30 {
            let name_length = u16_at(b, 26, true) as usize;
            let name = text_head(sub(b, 30, 30 + name_length), name_length);
            let base = name.rsplit('/').next().unwrap_or("");
            if base == "meta.json" || base.ends_with(".webp") {
                return Some("sog");
            }
        }
        return None;
    }
    if ext == "ksplat" && b.len() >= 44 && b[0] == 0 && b[1] >= 1 {
        let (sections, max, count, mode) =
            (i32le(b, 4), i32le(b, 12), i32le(b, 16), u16_at(b, 20, true));
        if sections > 0
            && sections <= 1024
            && count >= 0
            && max >= 0
            && (count > 0 || max > 0)
            && mode <= 2
        {
            return Some("ksplat");
        }
        return None;
    }
    if ext == "splat" && b.len() >= 32 {
        for i in 0..6 {
            let v = f32::from_le_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]]);
            if !v.is_finite() {
                return None;
            }
        }
        return Some("splat");
    }
    if matches!(b.first(), Some(0x7B | 0xEF | 0x20 | 0x0A | 0x0D | 0x09)) {
        let text = text_head(b, 4096);
        if starts_with_json_object(&text) && has_sog_means(&text) {
            return Some("sog-meta");
        }
    }
    None
}

/// ブラウザーが再生できない動画・音声の入れ物
fn sniff_other_media(b: &[u8], ty: &str, ext: &str) -> Option<Sniffed> {
    let audio = ty.starts_with("audio/") || AUDIO_ONLY_EXT.contains(&ext);
    let av =
        |container: &'static str| sc("video", if audio { "audio" } else { "video" }, container);
    let a = |container: &'static str| sc("video", "audio", container);
    let g = |i: usize| b.get(i).copied();
    if b.len() >= 16 && b[..16] == ASF_GUID {
        return av("asf");
    }
    if ascii(b, 0, &[0, 0, 1, 0xBA]) {
        return av("mpegps");
    }
    if ascii(b, 0, &[0, 0, 1, 0xB3]) {
        return sc("video", "video", "mpegvideo");
    }
    if b.len() >= 197 && b[4] == 0x47 && b[196] == 0x47 {
        return sc("video", "video", "m2ts");
    }
    if ascii(b, 0, b".RMF") {
        return av("rm");
    }
    if ascii(b, 0, b".ra\xFD") {
        return a("rm");
    }
    if ascii(b, 0, b"#!AMR") {
        return a("amr");
    }
    if ascii(b, 0, b"FORM") && (ascii(b, 8, b"AIFF") || ascii(b, 8, b"AIFC")) {
        return a("aiff");
    }
    if ascii(b, 0, b".snd") {
        return a("au");
    }
    if ascii(b, 0, b"caff") {
        return a("caf");
    }
    if ascii(b, 0, b"MAC ") {
        return a("ape");
    }
    if ascii(b, 0, b"wvpk") {
        return a("wv");
    }
    if ascii(b, 0, b"MPCK") || ascii(b, 0, b"MP+") {
        return a("mpc");
    }
    if ascii(b, 0, b"TTA1") {
        return a("tta");
    }
    if ascii(b, 0, b"Creative Voice File") {
        return a("voc");
    }
    if ascii(b, 0, &[0x7F, 0xFE, 0x80, 0x01]) {
        return a("dts");
    }
    if g(0) == Some(0x0B)
        && g(1) == Some(0x77)
        && (ext == "ac3" || ext == "eac3" || ty == "audio/ac3" || ty == "audio/eac3")
    {
        return a("ac3");
    }
    if ascii(b, 0, &[0x06, 0x0E, 0x2B, 0x34]) {
        return av("mxf");
    }
    if ascii(b, 0, b"YUV4MPEG2") {
        return sc("video", "video", "y4m");
    }
    if ascii(b, 0, b"nut/multimedia container") {
        return av("nut");
    }
    if ascii(b, 0, b"DKIF") {
        return sc("video", "video", "ivf");
    }
    if g(0) == Some(0x1F)
        && g(1) == Some(0x07)
        && g(2) == Some(0x00)
        && (ext == "dv" || ty == "video/dv")
    {
        return sc("video", "video", "dv");
    }
    None
}

/// 先頭のバイト列 (と Content-Type と拡張子) から、何として描くかを決める。分からなければ None。
/// `ty` は Content-Type の主な部分 (小文字。なければ空文字)
pub fn sniff(b: &[u8], ty: &str, ext: &str) -> Option<Sniffed> {
    let g = |i: usize| b.get(i).copied();
    // ---- 画像
    if g(0) == Some(0x89) && ascii(b, 1, b"PNG") {
        return s("image", "png");
    }
    if ascii(b, 0, b"GIF8") {
        return s("image", "gif");
    }
    if ascii(b, 0, &[0xFF, 0xD8, 0xFF]) {
        return s("image", "jpeg");
    }
    if ascii(b, 0, b"RIFF") && ascii(b, 8, b"WEBP") {
        return s("image", "webp");
    }
    if ascii(b, 0, b"RIFF") && ascii(b, 8, b"ACON") {
        return s("image", "ani");
    }
    if ascii(b, 0, b"BM") && b.len() >= 18 {
        return s("image", "bmp");
    }
    if g(0) == Some(0)
        && g(1) == Some(0)
        && matches!(g(2), Some(1 | 2))
        && g(3) == Some(0)
        && b.len() >= 6
        && (b[4] | b[5]) != 0
    {
        return s("image", if b[2] == 1 { "ico" } else { "cur" });
    }
    if ascii(b, 0, b"icns") {
        return s("image", "icns");
    }
    if ascii(b, 0, b"8BPS") {
        return s("image", "psd");
    }
    if ascii(b, 0, b"qoif") {
        return s("image", "qoi");
    }
    if ascii(b, 0, b"DDS ") {
        return s("image", "dds");
    }
    if g(0) == Some(0xAB) && ascii(b, 1, b"KTX 20") {
        return s("image", "ktx2");
    }
    if (ascii(b, 0, b"II") && g(2) == Some(42) && g(3) == Some(0))
        || (ascii(b, 0, b"MM") && g(2) == Some(0) && g(3) == Some(42))
        || (ascii(b, 0, b"II") && g(2) == Some(43) && g(3) == Some(0))
        || (ascii(b, 0, b"MM") && g(2) == Some(0) && g(3) == Some(43))
    {
        return s("image", "tiff");
    }
    if ascii(b, 0, b"farbfeld") {
        return s("image", "farbfeld");
    }
    if ascii(b, 0, b"#?RADIANCE") || ascii(b, 0, b"#?RGBE") {
        return s("image", "hdr");
    }
    if ascii(b, 0, &[0x76, 0x2F, 0x31, 0x01]) {
        return s("image", "exr");
    }
    if g(0) == Some(0x01) && g(1) == Some(0xDA) && matches!(g(2), Some(0 | 1)) {
        return s("image", "sgi");
    }
    if ascii(b, 0, &[0x59, 0xA6, 0x6A, 0x95]) {
        return s("image", "ras");
    }
    if g(0) == Some(0x0A) && g(1).is_some_and(|v| v <= 5) && g(2) == Some(1) && b.len() >= 128 {
        return s("image", "pcx");
    }
    if g(0) == Some(0x50)
        && g(1).is_some_and(|v| (0x31..=0x37).contains(&v))
        && matches!(g(2), Some(0x0A | 0x0D | 0x20 | 0x09))
    {
        return s("image", "pnm");
    }
    if g(0) == Some(0x50)
        && matches!(g(1), Some(0x66 | 0x46))
        && matches!(g(2), Some(0x0A | 0x0D | 0x20))
    {
        return s("image", "pfm");
    }
    if ascii(b, 4, b"ftyp") {
        let brand = sub(b, 8, 12);
        if brand == b"M4A " || brand == b"M4B " {
            return s("video", "audio");
        }
        if let Some(image) = iso_image(b) {
            return s("image", image);
        }
        if brand.starts_with(b"3g") {
            return sc("video", "video", "3gp");
        }
        return s("video", "video");
    }
    if (g(0) == Some(0xFF) && g(1) == Some(0x0A))
        || (ascii(b, 0, &[0, 0, 0, 0x0C]) && ascii(b, 4, b"JXL "))
    {
        return s("image", "jxl");
    }
    if (ascii(b, 0, &[0, 0, 0, 0x0C]) && ascii(b, 4, b"jP  "))
        || ascii(b, 0, &[0xFF, 0x4F, 0xFF, 0x51])
    {
        return s("image", "jp2");
    }
    if ascii(b, 0, b"SIMPLE  =") {
        return s("image", "fits");
    }
    if ascii(b, 0, b"SDPX") || ascii(b, 0, b"XPDS") {
        return s("image", "dpx");
    }
    if ascii(b, 0, b"FORM")
        && (ascii(b, 8, b"ILBM") || ascii(b, 8, b"PBM ") || ascii(b, 8, b"ACBM"))
    {
        return s("image", "iff");
    }
    // ---- PDF
    if find_ascii(b, b"%PDF-", 1024).is_some() {
        return s("pdf", "pdf");
    }
    // ---- 動画・音声・ライブ配信
    if ascii(b, 0, b"#EXTM3U") {
        return s("hls", "video");
    }
    if ascii(b, 0, b"FLV") && g(3) == Some(1) {
        return sc("flv", "video", "flv");
    }
    if ascii(b, 0, &[0x1A, 0x45, 0xDF, 0xA3]) {
        return sc("video", "video", "mkv");
    }
    if ascii(b, 0, b"OggS") {
        if find_ascii(b, b"theora", 512).is_some() {
            return sc("video", "video", "ogg-theora");
        }
        return sc(
            "video",
            if ty.starts_with("video/") {
                "video"
            } else {
                "audio"
            },
            "ogg",
        );
    }
    if ascii(b, 0, b"RIFF") && ascii(b, 8, b"WAVE") {
        return sc("video", "audio", "wav");
    }
    if ascii(b, 0, b"RIFF") && ascii(b, 8, b"AVI ") {
        return sc("video", "video", "avi");
    }
    if ascii(b, 0, b"fLaC") {
        return sc("video", "audio", "flac");
    }
    if ascii(b, 0, b"ID3")
        || (g(0) == Some(0xFF)
            && g(1).is_some_and(|v| v & 0xE0 == 0xE0)
            && b.len() >= 4
            && (ty.starts_with("audio/") || ext == "mp3" || ext == "aac"))
    {
        return s("video", "audio");
    }
    if g(0) == Some(0x47) && b.len() >= 189 && b[188] == 0x47 {
        return sc("video", "video", "ts");
    }
    if let Some(media) = sniff_other_media(b, ty, ext) {
        return Some(media);
    }
    // ---- 3D (3D モデル、構造物、ガウシアンスプラッティングの順)
    if ascii(b, 0, b"glTF") {
        return s("model", "glb");
    }
    if looks_like_nbt(b, false) || looks_like_nbt(b, true) {
        return s("structure", "nbt");
    }
    if g(0) == Some(0x1F) && g(1) == Some(0x8B) && g(2) == Some(8) {
        if ext == "svgz" {
            return s("image", "svgz");
        }
        if STRUCTURE_EXT.contains(&ext) {
            return s("structure", "nbt");
        }
        if ext == "spz" {
            return s("splat", "spz");
        }
        return s("probe", "gzip");
    }
    if let Some(splat) = sniff_splat(b, ext) {
        return s("splat", splat);
    }
    // ---- 文字の形式
    let text = text_head(b, 1024);
    if starts_with_svg(&text) {
        return s("image", "svg");
    }
    if starts_with_mpd(&text) {
        return s("dash", "video");
    }
    if starts_with_xpm(&text) {
        return s("image", "xpm");
    }
    if starts_with_xbm_width(&text) && (ext == "xbm" || ty == "image/x-xbitmap") {
        return s("image", "xbm");
    }
    if starts_with_json_object(&text) {
        return s("model", "gltf");
    }
    // ---- Content-Type (中身に目印がないもの)
    if ty == "multipart/x-mixed-replace" {
        return s("mjpeg", "video");
    }
    if matches!(
        ty,
        "application/vnd.apple.mpegurl"
            | "application/x-mpegurl"
            | "audio/mpegurl"
            | "audio/x-mpegurl"
    ) {
        return s("hls", "video");
    }
    if ty == "application/dash+xml" {
        return s("dash", "video");
    }
    if ty == "video/x-flv" {
        return s("flv", "video");
    }
    if ty == "image/svg+xml" {
        return s("image", "svg");
    }
    // ---- 目印のない画像は拡張子で
    if TGA_EXT.contains(&ext) || ty == "image/x-tga" || ty == "image/x-targa" {
        return s("image", "tga");
    }
    if ext == "wbmp" || ty == "image/vnd.wap.wbmp" {
        return s("image", "wbmp");
    }
    if (ext == "xwd" || ty == "image/x-xwindowdump")
        && b.len() >= 8
        && b[4] == 0
        && b[5] == 0
        && b[6] == 0
        && b[7] == 7
    {
        return s("image", "xwd");
    }
    if ext == "pgx" && ascii(b, 0, b"PG") {
        return s("image", "pgx");
    }
    if ext == "svg" && !b.is_empty() && (b[0] == 0x3C || b[0] == 0xEF || b[0] <= 0x20) {
        return s("image", "svg");
    }
    if ty.starts_with("video/") {
        return s("video", "video");
    }
    if ty.starts_with("audio/") {
        return s("video", "audio");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_url_basics() {
        assert_eq!(
            safe_url("https://Example.com/a#f", true).as_deref(),
            Some("https://example.com/a#f")
        );
        assert_eq!(safe_url("http://example.com/", true), None);
        assert!(safe_url("http://example.com/", false).is_some());
        for bad in [
            "https://localhost/",
            "https://127.0.0.1/",
            "https://[::1]/",
            "https://u:p@example.com/",
            "https://example.com:9999/",
            "https://example.com/a b",
            "https://intranet/",
            "",
        ] {
            assert_eq!(safe_url(bad, true), None, "{bad}");
        }
    }

    #[test]
    fn extension() {
        assert_eq!(extension_of("https://x.test/a.PNG?x=1.jpg"), "png");
        assert_eq!(extension_of("https://x.test/a."), "");
        assert_eq!(extension_of("https://x.test/.png"), "png");
        assert_eq!(extension_of("nope"), "");
    }

    #[test]
    fn sniff_known_heads() {
        let png = [0x89, b'P', b'N', b'G', 13, 10, 26, 10];
        assert_eq!(sniff(&png, "", "").unwrap().format, "png");
        assert_eq!(sniff(b"#EXTM3U\n", "", "").unwrap().route, "hls");
        assert_eq!(sniff(b"<html>", "text/html", ""), None);
        assert_eq!(sniff(b"", "", ""), None);
        assert_eq!(sniff(b"%PDF-1.7", "", "").unwrap().route, "pdf");
    }

    #[test]
    fn gaussian_property_rules() {
        assert!(has_gaussian_property("property float f_dc_0\n"));
        assert!(has_gaussian_property("property float f_dc_0"));
        assert!(has_gaussian_property("x property\nfloat\nf_dc_0 \t"));
        assert!(!has_gaussian_property("property float f_dc_0 x"));
        assert!(!has_gaussian_property("property float xf_dc_0"));
    }
}
