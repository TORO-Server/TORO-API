//! 原稿の本文の検査
//!
//! Wiki のビルドは、原稿の先頭の `---` のブロックを front matter として読む。
//! 開きの `---` の後ろに言語を書くと読み方が変わり、YAML 以外はデータとして安全に読める保証が無い。
//! 保存できる原稿は、YAML の front matter か、front matter の無いものだけにする。

/// front matter が無いか、YAML か
pub fn front_matter_is_yaml(content: &str) -> bool {
    let text = content.strip_prefix('\u{FEFF}').unwrap_or(content);
    let Some(rest) = text.strip_prefix("---") else {
        return true;
    };
    // `----` は front matter ではなく、ただの罫線
    if rest.starts_with('-') {
        return true;
    }
    match rest.split_once('\n') {
        Some((language, _)) => {
            let language = language.trim();
            language.is_empty()
                || language.eq_ignore_ascii_case("yaml")
                || language.eq_ignore_ascii_case("yml")
        }
        // 改行が無いと、1 行目がそのまま言語の名前として読まれる
        None => rest.trim().is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_and_yaml_front_matter_are_accepted() {
        for good in [
            "",
            "本文だけ",
            "# 見出し\n\n---\n\n罫線の後ろ",
            "----\n罫線",
            "---",
            "--- \n",
            "---\n",
            "---\ntitle: \"a\"\n---\n本文",
            "---\r\ntitle: a\r\n---\r\n本文",
            "---yaml\ntitle: a\n---\n本文",
            "---YML\r\ntitle: a\r\n---\r\n本文",
            "\u{FEFF}---\ntitle: a\n---\n本文",
            // 先頭でなければ front matter ではない
            "\n---toml\n",
            "本文\n---toml\n",
        ] {
            assert!(front_matter_is_yaml(good), "{good:?}");
        }
    }

    #[test]
    fn other_front_matter_languages_are_refused() {
        for bad in [
            "---toml\ntitle = \"a\"\n---\n本文",
            "--- toml \ntitle = \"a\"\n---\n本文",
            "---toml\r\ntitle = \"a\"\r\n---\r\n本文",
            "---json\n{}\n---\n本文",
            "\u{FEFF}---toml\ntitle = \"a\"\n---\n本文",
            // 改行が無いもの
            "---toml",
            "---yaml",
            // 復帰だけでは行が終わらない
            "---toml\rtitle\n---\n本文",
            "---\u{FEFF}toml\n---\n本文",
        ] {
            assert!(!front_matter_is_yaml(bad), "{bad:?}");
        }
    }
}
