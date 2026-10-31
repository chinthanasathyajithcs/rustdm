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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_table() {
        let cases = [
            ("..", ""),
            ("../../evil.exe", "_.._evil.exe"),   // review focus 2
            ("a<b>.txt", "a_b_.txt"),
            ("  x. ", "x"),
            ("a\u{7}b", "ab"),
            ("CON", "_CON"),                     // review focus 3
            ("nul.txt", "_nul.txt"),             // review focus 3
            ("COM1.log", "_COM1.log"),           // review focus 3
            ("console.txt", "console.txt"),      // not reserved: stem is "console"
        ];
        for (raw, want) in cases {
            assert_eq!(sanitize_component(raw), want, "input {raw:?}");
        }
    }

    #[test]
    fn sanitize_caps_length_at_255_chars() {
        assert_eq!(sanitize_component(&"a".repeat(300)).chars().count(), 255);
    }
}
