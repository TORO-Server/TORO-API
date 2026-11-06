//! 原稿のパス。どの入口から来ても docs/ の下の .md だけを指すようにする

/// 保存先のパスの長さの上限 (バイト)
const MAX_SAVE_PATH_BYTES: usize = 512;

/// `07_System/a` → `docs/07_System/a.md`。docs/ の外に出るものなどは None
pub fn normalize(path: &str) -> Option<String> {
    let trimmed = path.trim_start_matches('/');
    if trimmed.contains('\\') || trimmed.chars().any(char::is_control) {
        return None;
    }
    if trimmed
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return None;
    }
    let mut clean = if trimmed.starts_with("docs/") {
        trimmed.to_string()
    } else {
        format!("docs/{trimmed}")
    };
    if !clean.ends_with(".md") {
        clean.push_str(".md");
    }
    Some(clean)
}

/// 保存してよいパス。`docs/**/*.md` のうち、Wiki のビルドがページとして読むものだけを通す。
/// ビルドは `docs/README.md`、`attachments` (サイトにそのまま公開する)、`.` で始まるフォルダーを別に扱う
pub fn normalize_for_save(path: &str) -> Option<String> {
    if path.starts_with('/') || path.len() > MAX_SAVE_PATH_BYTES {
        return None;
    }
    // 拡張子の違うものに、黙って .md を付けて保存しない
    let file_name = path.rsplit('/').next().unwrap_or(path);
    if !file_name.ends_with(".md") && file_name.contains('.') {
        return None;
    }
    let clean = normalize(path)?;
    let inside_docs = clean.strip_prefix("docs/")?;
    let is_page = !inside_docs.eq_ignore_ascii_case("README.md")
        && inside_docs.split('/').all(is_page_segment);
    is_page.then_some(clean)
}

fn is_page_segment(segment: &str) -> bool {
    !segment.starts_with('.')
        && !segment.eq_ignore_ascii_case("attachments")
        && segment == segment.trim()
        // 手元の Windows で git が取り出せない名前も、1 つあると pull ごと止まる
        && !segment.ends_with('.')
        && !is_windows_device_name(segment)
        && !segment.chars().any(is_unsafe_char)
}

fn is_windows_device_name(segment: &str) -> bool {
    let stem = segment
        .split('.')
        .next()
        .unwrap_or(segment)
        .to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    stem.len() == 4
        && (stem.starts_with("COM") || stem.starts_with("LPT"))
        && stem.ends_with(|c: char| c.is_ascii_digit())
}

/// Windows で使えない文字と、見えない文字 (見た目の同じ別のパスが作れてしまう)
fn is_unsafe_char(c: char) -> bool {
    matches!(
        c,
        '<' | '>'
            | ':'
            | '"'
            | '|'
            | '?'
            | '*'
            | '\u{00AD}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_prefix_and_suffix() {
        assert_eq!(
            normalize("07_System/a").as_deref(),
            Some("docs/07_System/a.md")
        );
        assert_eq!(
            normalize("/docs/日本語.md").as_deref(),
            Some("docs/日本語.md")
        );
    }

    #[test]
    fn rejects_paths_leaving_docs() {
        for bad in [
            "",
            "../secret",
            "a/../../b",
            "a/./b",
            "a//b",
            "a\\b",
            "a\nb",
            "docs/../.github/x",
        ] {
            assert_eq!(normalize(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn saved_paths_are_pages_under_docs() {
        for (good, clean) in [
            ("a", "docs/a.md"),
            ("a.md", "docs/a.md"),
            ("docs/a.md", "docs/a.md"),
            (
                "07_System/日本語のページ.md",
                "docs/07_System/日本語のページ.md",
            ),
            (
                "docs/07_System/Midori City",
                "docs/07_System/Midori City.md",
            ),
            ("ver.2.md", "docs/ver.2.md"),
            ("世界政府（WFG）", "docs/世界政府（WFG）.md"),
            // 名前の一部に使われているだけのものは通す
            ("attachments2/a", "docs/attachments2/a.md"),
            ("Console", "docs/Console.md"),
            ("COMMANDS", "docs/COMMANDS.md"),
            ("sub/README", "docs/sub/README.md"),
        ] {
            assert_eq!(normalize_for_save(good).as_deref(), Some(clean), "{good:?}");
        }
    }

    #[test]
    fn saved_paths_refuse_what_the_wiki_build_treats_specially() {
        for bad in [
            "",
            "/a",
            "/docs/a.md",
            "../secret",
            "docs/../outside",
            "a\\b",
            "a\nb",
            // .md 以外の拡張子
            "a.html",
            "sub/a.json",
            "a.MD",
            "ver.2",
            // ビルドがページとして読まないもの
            ".md",
            "docs/.md",
            ".hidden/x",
            "docs/.hidden/x.md",
            "a/.hidden/b",
            ".x",
            "attachments/x",
            "docs/attachments/x.md",
            "a/Attachments/x",
            "README",
            "README.md",
            "docs/README.md",
            "docs/readme.md",
            // 手元の Windows で取り出せないもの
            "a:b",
            "a?b",
            "a*b",
            "a|b",
            "a<b>",
            "a\"b",
            "con",
            "docs/NUL.md",
            "a/COM1/b",
            "LPT9",
            "a./b",
            " a/b",
            "a /b",
            // 見えない文字
            "a\u{200B}b",
            "a\u{202E}b",
            "\u{FEFF}a",
        ] {
            assert_eq!(normalize_for_save(bad), None, "{bad:?}");
        }
        assert_eq!(
            normalize_for_save(&"a".repeat(MAX_SAVE_PATH_BYTES + 1)),
            None
        );
    }
}
