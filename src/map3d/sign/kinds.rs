//! 中継 (`proxy`) で返してよい中身の種類と、種類ごとの大きさの上限 (MarumaSign 本体と同じ)。
//!
//! 先頭のバイト列で決める (`core::sniff`。Content-Type は信じない)。HTML は返さない。
//! 返すときの Content-Type は決まったものだけ (画像・動画・音声のほかは `application/octet-stream`)。本当の種類は `X-MarumaSign-Type` で知らせる。
//! 正本 (移管前) は TORO-WebMap の `server/lib/marumasign/kinds.ts`。

use super::core::{MAX_IMAGE_BYTES, MAX_PDF_BYTES, Sniffed, sniff};
use super::xml_head::starts_after_xml_head;

const MIB: u64 = 1024 * 1024;
/// 3D モデル・構造物 (MarumaSign 本体の上限と同じ)
pub const MAX_MODEL_BYTES: u64 = 64 * MIB;
/// ガウシアンスプラッティング
pub const MAX_SPLAT_BYTES: u64 = 200 * MIB;
/// 再生リスト (HLS, MPEG-DASH)
pub const MAX_MANIFEST_BYTES: u64 = 4 * MIB;
/// 中を読んで URL を取り出す .gltf の大きさの上限 (これより大きなものは中継するが、中の URL は読まない)
pub const MAX_GLTF_PARSE_BYTES: u64 = 16 * MIB;
/// 動画・音声のファイル 1 つ (1 つの IP から読める量の上限が別にある)
pub const MAX_MEDIA_BYTES: u64 = 1024 * MIB;
/// 再生リストの中の分割されたファイルや、.gltf の .bin など (中身に目印がない)
pub const MAX_OPAQUE_BYTES: u64 = 64 * MIB;
/// 再生リストと別のサイトにある、中身に目印のないもの (分割ファイルらしい拡張子のものだけ)
pub const MAX_FOREIGN_OPAQUE_BYTES: u64 = 16 * MIB;
/// 別のサイトの、それ以外の目印のないもの (暗号化の鍵など)
pub const MAX_FOREIGN_SMALL_BYTES: u64 = 4 * 1024;

const SEGMENT_EXT: &[&str] = &[
    "ts", "m2ts", "mts", "m4s", "m4v", "m4a", "mp4", "cmfv", "cmfa", "cmft", "aac", "ac3", "ec3",
    "mp3", "webm", "vtt", "webvtt", "key",
];

const OCTET: &str = "application/octet-stream";

/// 中身の中の URL を取り出す方法
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ManifestKind {
    Hls,
    Dash,
    Gltf,
}

#[derive(Clone, Debug)]
pub struct Kind {
    /// 種類 (`core::sniff` の route か、opaque)
    pub name: &'static str,
    /// 大きさの上限 (バイト)
    pub cap: u64,
    /// 返す Content-Type
    pub ty: &'static str,
    pub manifest: Option<ManifestKind>,
    /// 終わらない配信 (MJPEG, HTTP-FLV など) かもしれない。遅くても切らない
    pub stream: bool,
}

fn native_image(format: &str) -> &'static str {
    match format {
        "png" => "image/png",
        "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        _ => OCTET,
    }
}

/// HTML らしい先頭の目印 (`<` の後ろ。大文字小文字は区別しない)。移管前の正規表現
/// `<(!doctype\s+html|html|head|body|script|iframe|meta|title|style|link|frameset|form|div|p|a)[\s>/]`
fn html_tag(rest: &str) -> bool {
    let b = rest.as_bytes();
    if b.first() != Some(&b'<') {
        return false;
    }
    let after = |at: usize| -> bool {
        match rest.get(at..).and_then(|s| s.chars().next()) {
            Some(c) => c == '>' || c == '/' || super::xml_head::is_js_space(c),
            None => false,
        }
    };
    let word_at = |at: usize, word: &str| -> bool {
        b.get(at..at + word.len())
            .is_some_and(|s| s.eq_ignore_ascii_case(word.as_bytes()))
    };
    for name in [
        "html", "head", "body", "script", "iframe", "meta", "title", "style", "link", "frameset",
        "form", "div", "p", "a",
    ] {
        if word_at(1, name) && after(1 + name.len()) {
            return true;
        }
    }
    // <!doctype html ...: doctype の後ろに空白が 1 つ以上あって html の後ろが [\s>/]
    if word_at(1, "!doctype") {
        let mut at = 1 + "!doctype".len();
        let start = at;
        while let Some(c) = rest.get(at..).and_then(|s| s.chars().next()) {
            if !super::xml_head::is_js_space(c) {
                break;
            }
            at += c.len_utf8();
        }
        return at > start && word_at(at, "html") && after(at + 4);
    }
    false
}

/// 先頭が HTML らしいか (中継しない)
pub fn looks_like_html(head: &[u8]) -> bool {
    // 先頭の 1024 バイトを Latin-1 の文字として読む (移管前と同じ。バイトと文字が 1 対 1 になる)
    let text: String = head.iter().take(1024).map(|b| *b as char).collect();
    let text = text
        .strip_prefix("\u{ef}\u{bb}\u{bf}")
        .or_else(|| text.strip_prefix("\u{fe}\u{ff}"))
        .or_else(|| text.strip_prefix("\u{ff}\u{fe}"))
        .unwrap_or(&text);
    starts_after_xml_head(text, html_tag, false, false)
}

fn media_type(s: &Sniffed, head: &[u8]) -> &'static str {
    match s.container {
        Some("mkv") => return "video/webm",
        Some("wav") => return "audio/wav",
        Some("flac") => return "audio/flac",
        Some("ts") => return "video/mp2t",
        Some("flv") => return "video/x-flv",
        Some("3gp") => return "video/mp4",
        Some("ogg") => {
            return if s.format == "audio" {
                "audio/ogg"
            } else {
                "video/ogg"
            };
        }
        _ => {}
    }
    if s.container.is_none() && head.get(4..8) == Some(b"ftyp") {
        return if s.format == "audio" {
            "audio/mp4"
        } else {
            "video/mp4"
        };
    }
    if s.container.is_none() && s.format == "audio" {
        return "audio/mpeg";
    }
    OCTET
}

/// 中身の種類と上限。返さないものは None
///
/// - `ty`: 元のサイトの Content-Type の主な部分 (小文字)
/// - `ext`: URL の拡張子
/// - `child`: 再生リストや .gltf に書かれていた URL (目印のない中身でもよい)
/// - `foreign`: その再生リストと別のサイトにある (目印のない中身は、分割ファイルらしいものと小さいものだけ)
pub fn classify(head: &[u8], ty: &str, ext: &str, child: bool, foreign: bool) -> Option<Kind> {
    if looks_like_html(head) {
        return None;
    }
    let Some(s) = sniff(head, ty, ext) else {
        if ty == "text/html" || ty == "application/xhtml+xml" || !child {
            return None;
        }
        let cap = if !foreign {
            MAX_OPAQUE_BYTES
        } else if SEGMENT_EXT.contains(&ext) {
            MAX_FOREIGN_OPAQUE_BYTES
        } else {
            MAX_FOREIGN_SMALL_BYTES
        };
        return Some(Kind {
            name: "opaque",
            cap,
            ty: OCTET,
            manifest: None,
            stream: false,
        });
    };
    let kind = |name: &'static str, cap: u64, ty: &'static str| Kind {
        name,
        cap,
        ty,
        manifest: None,
        stream: false,
    };
    Some(match s.route {
        "image" => kind("image", MAX_IMAGE_BYTES, native_image(s.format)),
        "pdf" => kind("pdf", MAX_PDF_BYTES, OCTET),
        "model" => Kind {
            manifest: (s.format == "gltf").then_some(ManifestKind::Gltf),
            ..kind("model", MAX_MODEL_BYTES, OCTET)
        },
        "structure" => kind("structure", MAX_MODEL_BYTES, OCTET),
        "splat" => kind("splat", MAX_SPLAT_BYTES, OCTET),
        // gzip の中身は展開しないと分からない (構造物、.spz、.svgz)。大きいほうに合わせる
        "probe" => kind("gzip", MAX_SPLAT_BYTES, OCTET),
        "hls" => Kind {
            manifest: Some(ManifestKind::Hls),
            ..kind("hls", MAX_MANIFEST_BYTES, OCTET)
        },
        "dash" => Kind {
            manifest: Some(ManifestKind::Dash),
            ..kind("dash", MAX_MANIFEST_BYTES, OCTET)
        },
        "mjpeg" => Kind {
            stream: true,
            ..kind("mjpeg", MAX_MEDIA_BYTES, OCTET)
        },
        "flv" => Kind {
            stream: true,
            ..kind("flv", MAX_MEDIA_BYTES, "video/x-flv")
        },
        "video" => Kind {
            stream: true,
            ..kind("video", MAX_MEDIA_BYTES, media_type(&s, head))
        },
        _ => return None,
    })
}

/// 元のサイトの Content-Type (知らせるだけ)。形の正しいものだけ、でなければ空
pub fn clean_type(value: Option<&str>) -> String {
    let t = value
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let ok_part = |part: &str| {
        let mut chars = part.chars();
        let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphanumeric());
        first_ok
            && part.len() <= 64
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "!#$&^_.+-".contains(c))
    };
    match t.split_once('/') {
        Some((a, b)) if ok_part(a) && ok_part(b) => t,
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0";

    #[test]
    fn html_is_never_served() {
        for html in [
            "<!DOCTYPE html><html>",
            "  <html>",
            "<?xml version=\"1.0\"?><html>",
            "<SCRIPT>alert(1)</SCRIPT>",
            "<a href=x>",
            "<body/>",
        ] {
            let head = html.as_bytes();
            // 先頭が <?xml の場合、xml_head の既定は飛ばさない。直接の HTML だけを確かめる
            if html.starts_with("<?xml") {
                continue;
            }
            assert!(looks_like_html(head), "{html}");
            assert!(
                classify(head, "text/plain", "png", true, false).is_none(),
                "{html}"
            );
        }
        assert!(!looks_like_html(
            b"<svg xmlns=\"http://www.w3.org/2000/svg\">"
        ));
        assert!(!looks_like_html(b"<abc>"));
        assert!(!looks_like_html(PNG));
    }

    #[test]
    fn images_and_caps() {
        let kind = classify(PNG, "image/png", "png", false, false).unwrap();
        assert_eq!(kind.name, "image");
        assert_eq!(kind.ty, "image/png");
        assert_eq!(kind.cap, MAX_IMAGE_BYTES);
        assert!(!kind.stream);
    }

    #[test]
    fn unknown_content_needs_a_child_url() {
        let blob = b"\x01\x02\x03\x04binary";
        assert!(classify(blob, "", "bin", false, false).is_none());
        let child = classify(blob, "", "bin", true, false).unwrap();
        assert_eq!((child.name, child.cap), ("opaque", MAX_OPAQUE_BYTES));
        let foreign_segment = classify(blob, "", "ts", true, true).unwrap();
        assert_eq!(foreign_segment.cap, MAX_FOREIGN_OPAQUE_BYTES);
        let foreign_other = classify(blob, "", "bin", true, true).unwrap();
        assert_eq!(foreign_other.cap, MAX_FOREIGN_SMALL_BYTES);
        // HTML と宣言されたものは、目印がなくても返さない
        assert!(classify(blob, "text/html", "bin", true, false).is_none());
    }

    #[test]
    fn playlists_are_parsed() {
        let hls = classify(b"#EXTM3U\n#EXT-X-VERSION:3\n", "", "m3u8", false, false).unwrap();
        assert_eq!(hls.manifest, Some(ManifestKind::Hls));
        assert_eq!(hls.cap, MAX_MANIFEST_BYTES);
    }

    #[test]
    fn content_types_are_cleaned() {
        assert_eq!(clean_type(Some("Image/PNG; charset=x")), "image/png");
        assert_eq!(clean_type(Some("not a type")), "");
        assert_eq!(clean_type(Some("a/<b>")), "");
        assert_eq!(clean_type(None), "");
    }
}
