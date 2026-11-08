fn is_reserved_device_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

pub fn sanitize_component(raw: &str) -> String {
    // 1. Strip control characters (U+0000 to U+001F, U+007F).
    let s: String = raw
        .chars()
        .filter(|&c| {
            let u = c as u32;
            !(u <= 0x1F || u == 0x7F)
        })
        .collect();

    // 2. Replace each of < > : " / \ | ? * with _.
    let s: String = s
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            other => other,
        })
        .collect();

    // 3. If the result is . or .., return an empty string.
    if s == "." || s == ".." {
        return String::new();
    }

    // 4. Trim leading and trailing dots and spaces.
    let s = s.trim_matches(|c: char| c == '.' || c == ' ');

    // 5. If the part before the first dot is a reserved device name, prefix _.
    let stem = match s.split_once('.') {
        Some((before, _)) => before,
        None => s,
    };

    let s = if is_reserved_device_name(stem) {
        format!("_{s}")
    } else {
        s.to_string()
    };

    // 6. Cap at 255 characters.
    s.chars().take(255).collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tokens {
    pub downloads: String,
    pub host: String,
    pub name: String,
    pub date: String,
}

pub fn split_stem_ext(name: &str) -> (String, String) {
    match name.rfind('.') {
        Some(0) => (name.to_string(), String::new()),
        Some(i) => (name[..i].to_string(), name[i + 1..].to_string()),
        None => (name.to_string(), String::new()),
    }
}

/// Fill in the `{token}`s of `template` in one pass, so text inside a token's
/// value (such as a server-supplied name containing `{date}`) is never expanded.
/// Unknown tokens and unclosed braces are kept as written.
pub fn expand(template: &str, tokens: &Tokens) -> String {
    let s_host = sanitize_component(&tokens.host);
    let s_name = sanitize_component(&tokens.name);
    let (stem, ext) = split_stem_ext(&s_name);
    let s_date = sanitize_component(&tokens.date);
    let value = |token: &str| -> Option<&str> {
        match token {
            "downloads" => Some(&tokens.downloads),
            "host" => Some(&s_host),
            "name" => Some(&s_name),
            "stem" => Some(&stem),
            "ext" => Some(&ext),
            "date" => Some(&s_date),
            _ => None,
        }
    };

    let mut out = String::with_capacity(template.len() + 64);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after
            .find('}')
            .and_then(|close| Some((close, value(&after[..close])?)))
        {
            Some((close, v)) => {
                out.push_str(v);
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn parse_content_disposition(cd: &str) -> (Option<String>, Option<String>) {
    let mut filename_star = None;
    let mut filename = None;

    for part in cd.split(';') {
        let part = part.trim();
        if let Some((k, v)) = part.split_once('=') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim();
            if key == "filename*" {
                let val = val.trim_matches('"');
                let pieces: Vec<&str> = val.splitn(3, '\'').collect();
                if pieces.len() == 3 {
                    let encoded = pieces[2];
                    let decoded = percent_encoding::percent_decode_str(encoded)
                        .decode_utf8_lossy()
                        .to_string();
                    if !decoded.is_empty() {
                        filename_star = Some(decoded);
                    }
                }
            } else if key == "filename" {
                let val = val.trim_matches('"');
                if !val.is_empty() {
                    filename = Some(val.to_string());
                }
            }
        }
    }

    (filename_star, filename)
}

pub fn pick_name(content_disposition: Option<&str>, url: &url::Url) -> String {
    if let Some(cd) = content_disposition {
        let (star, plain) = parse_content_disposition(cd);
        if let Some(name) = star {
            return name;
        }
        if let Some(name) = plain {
            return name;
        }
    }

    if let Some(mut segments) = url.path_segments() {
        if let Some(last) = segments.rfind(|s| !s.is_empty()) {
            let decoded = percent_encoding::percent_decode_str(last)
                .decode_utf8_lossy()
                .to_string();
            if !decoded.is_empty() {
                return decoded;
            }
        }
    }

    "download".to_string()
}

pub fn is_inside(base: &str, candidate: &str) -> bool {
    let has_prefix = if let Some(rem) = candidate.strip_prefix(base) {
        if base.ends_with('\\') || base.ends_with('/') {
            true
        } else {
            rem.starts_with('\\') || rem.starts_with('/')
        }
    } else {
        false
    };

    if !has_prefix {
        return false;
    }

    for seg in candidate.split(['\\', '/']) {
        if seg == ".." {
            return false;
        }
    }

    true
}

pub fn collision_name(file_name: &str, n: u32) -> String {
    let (stem, ext) = split_stem_ext(file_name);
    if ext.is_empty() {
        format!("{stem} ({n})")
    } else {
        format!("{stem} ({n}).{ext}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(downloads: &str, name: &str) -> Tokens {
        Tokens {
            downloads: downloads.to_string(),
            host: "example.com".to_string(),
            name: name.to_string(),
            date: "2026-10-14".to_string(),
        }
    }

    #[test]
    fn sanitize_table() {
        let cases = [
            ("..", ""),
            ("../../evil.exe", "_.._evil.exe"), // review focus 2
            ("a<b>.txt", "a_b_.txt"),
            ("  x. ", "x"),
            ("a\u{7}b", "ab"),
            ("CON", "_CON"),                // review focus 3
            ("nul.txt", "_nul.txt"),        // review focus 3
            ("COM1.log", "_COM1.log"),      // review focus 3
            ("console.txt", "console.txt"), // not reserved: stem is "console"
        ];
        for (raw, want) in cases {
            assert_eq!(sanitize_component(raw), want, "input {raw:?}");
        }
    }

    #[test]
    fn sanitize_caps_length_at_255_chars() {
        assert_eq!(sanitize_component(&"a".repeat(300)).chars().count(), 255);
    }

    #[test]
    fn hostile_name_stays_inside_base() {
        // review focus 2
        let t = tokens("C:\\Users\\me\\Downloads", "../../evil.exe");
        let out = expand("{downloads}\\{name}", &t);
        assert_eq!(out, "C:\\Users\\me\\Downloads\\_.._evil.exe");
        assert!(is_inside("C:\\Users\\me\\Downloads", &out));
    }

    #[test]
    fn is_inside_rejects_escapes_and_prefix_tricks() {
        assert!(!is_inside(
            "C:\\Users\\me\\Downloads",
            "C:\\Users\\me\\Downloads\\..\\..\\Windows\\x"
        ));
        assert!(!is_inside(
            "C:\\Users\\me\\Downloads",
            "C:\\Users\\me\\Downloads2\\x"
        ));
    }

    #[test]
    fn split_stem_ext_handles_dotfiles_and_multi_dot() {
        assert_eq!(
            split_stem_ext("file.tar.gz"),
            ("file.tar".into(), "gz".into())
        );
        assert_eq!(split_stem_ext(".bashrc"), (".bashrc".into(), "".into()));
    }

    #[test]
    fn pick_name_priority() {
        let u = url::Url::parse("https://x.test/dir/file%20one.iso").unwrap();
        assert_eq!(
            pick_name(Some("attachment; filename*=UTF-8''na%C3%AFve.zip"), &u),
            "naïve.zip"
        );
        assert_eq!(
            pick_name(Some("attachment; filename=\"a.txt\""), &u),
            "a.txt"
        );
        assert_eq!(pick_name(None, &u), "file one.iso");
        let root = url::Url::parse("https://x.test/").unwrap();
        assert_eq!(pick_name(None, &root), "download");
    }

    #[test]
    fn collision_names_use_numbered_suffix() {
        assert_eq!(collision_name("file.iso", 1), "file (1).iso");
        assert_eq!(collision_name("README", 2), "README (2)");
    }

    #[test]
    fn token_values_are_never_expanded_again() {
        let t = tokens("/dl", "{date} {downloads}.zip");
        assert_eq!(
            expand("{downloads}/{name}", &t),
            "/dl/{date} {downloads}.zip"
        );
        assert_eq!(
            expand("{stem}-{date}.{ext}", &t),
            "{date} {downloads}-2026-10-14.zip"
        );
    }

    #[test]
    fn unknown_or_unclosed_braces_are_kept_as_written() {
        let t = tokens("/dl", "a.bin");
        assert_eq!(expand("{downloads}/{nope}/{name", &t), "/dl/{nope}/{name");
    }
}
