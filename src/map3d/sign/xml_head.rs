//! XML や HTML の先頭の前置き (空白、`<?xml ...>`、コメント、DOCTYPE) を飛ばす。
//!
//! 正本は TORO-3D-WebMap の `app/bluemap/marumasign/xml-head.ts`。外から来た本文に使うので、正規表現を使わず、
//! 前から 1 度だけ読む (コメントが続いても、後戻りが増えない)。位置はバイト単位 (JavaScript は UTF-16 の単位)。
//! `tests/vectors/marumasign-shared.json` の入出力と同じ結果になることを `tests/sign_vectors.rs` が確かめる。

/// JavaScript の正規表現の `\s` と同じ空白か (Rust の `char::is_whitespace` とは、U+0085 と U+FEFF が違う)
pub fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// JavaScript の `String.prototype.trim` と同じ (前後の空白と改行を除く)
pub fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_space)
}

/// `at` から先の空白を飛ばした位置
pub fn skip_space(text: &str, at: usize) -> usize {
    let mut i = at;
    for c in text[at..].chars() {
        if !is_js_space(c) {
            break;
        }
        i += c.len_utf8();
    }
    i
}

/// `at` の位置に `word` があるか (大文字小文字を区別しない)
fn word_at(text: &str, at: usize, word: &str) -> bool {
    text.as_bytes()
        .get(at..at + word.len())
        .is_some_and(|s| s.eq_ignore_ascii_case(word.as_bytes()))
}

/// `rest` が `<name` で始まり、そのあとが空白か `>` か (正規表現の `<name[\s>]` の、先頭に固定したもの)
pub fn tag_follows(rest: &str, name: &str, ignore_case: bool) -> bool {
    let bytes = rest.as_bytes();
    let n = name.len() + 1;
    let Some(head) = bytes.get(..n) else {
        return false;
    };
    let same = if ignore_case {
        head[1..].eq_ignore_ascii_case(name.as_bytes())
    } else {
        &head[1..] == name.as_bytes()
    };
    if head[0] != b'<' || !same {
        return false;
    }
    match rest[n..].chars().next() {
        Some(c) => c == '>' || is_js_space(c),
        None => false,
    }
}

/// 前置きの次の位置 (最初の要素の `<` があるはずのところ)。閉じていない前置きがあれば None
pub fn skip_xml_head(text: &str, decl: bool, doctype: bool) -> Option<usize> {
    let mut at = skip_space(text, 0);
    if decl && word_at(text, at, "<?xml") {
        let end = at + text[at..].find('>')?;
        at = skip_space(text, end + 1);
    }
    loop {
        let end;
        if text[at..].starts_with("<!--") {
            end = at + 4 + text[at + 4..].find("-->")? + 2;
        } else if doctype && word_at(text, at, "<!doctype") {
            end = at + text[at..].find('>')?;
        } else {
            return Some(at);
        }
        at = skip_space(text, end + 1);
    }
}

/// 前置きのあとが、`matcher` に合うもので始まるか (`matcher` には、前置きのあとの文字列を渡す)
pub fn starts_after_xml_head(
    text: &str,
    matcher: impl Fn(&str) -> bool,
    decl: bool,
    doctype: bool,
) -> bool {
    match skip_xml_head(text, decl, doctype) {
        Some(at) => matcher(&text[at..]),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_comments_and_declarations() {
        assert_eq!(skip_xml_head("  <a>", false, false), Some(2));
        assert_eq!(
            skip_xml_head("<?xml version=\"1.0\"?> <!-- c --> <svg>", true, true),
            Some(33)
        );
        assert_eq!(skip_xml_head("<?xml?><a>", false, false), Some(0));
        assert_eq!(skip_xml_head("<!-- unclosed", false, false), None);
        assert_eq!(skip_xml_head("<!DOCTYPE a><a>", false, false), Some(0));
        assert_eq!(skip_xml_head("<!DOCTYPE a><a>", false, true), Some(12));
    }

    #[test]
    fn many_comments_are_linear() {
        let text = format!("{}<svg>", "<!---->".repeat(50_000));
        assert_eq!(skip_xml_head(&text, true, true), Some(350_000));
    }

    #[test]
    fn tag_prefix() {
        assert!(tag_follows("<MPD>", "MPD", false));
        assert!(tag_follows("<MPD \n", "MPD", false));
        assert!(!tag_follows("<mpd>", "MPD", false));
        assert!(tag_follows("<SVG>", "svg", true));
        assert!(!tag_follows("<MPDX>", "MPD", false));
        assert!(!tag_follows("<MPD", "MPD", false));
    }
}
