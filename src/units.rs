//! Parsing of the numbers people type: speeds and whole-number counts.
//! Shared by the CLI and the TUI so both accept the same input.

/// Parse a speed such as `500K`, `2M`, `1.5MiB/s` or `0`/`off` (unlimited).
/// Units are binary: K = 1024 bytes.
pub fn parse_speed(raw: &str) -> Result<u64, String> {
    let lower = raw.trim().to_ascii_lowercase();
    if matches!(lower.as_str(), "0" | "off" | "unlimited") {
        return Ok(0);
    }

    let text = lower.strip_suffix("/s").unwrap_or(&lower);
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let value: f64 = number
        .parse()
        .map_err(|_| format!("invalid speed '{raw}'; try 500K or 2M"))?;
    let multiplier: u64 = match unit.trim() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        other => return Err(format!("unknown speed unit '{other}'; use K, M, G or T")),
    };
    Ok((value * multiplier as f64) as u64)
}

/// Parse a whole number from `min` to `max`, ignoring surrounding spaces.
pub fn parse_count(raw: &str, min: u32, max: u32) -> Result<u32, String> {
    raw.trim()
        .parse::<u32>()
        .ok()
        .filter(|n| (min..=max).contains(n))
        .ok_or_else(|| format!("Enter a whole number from {min} to {max}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speeds_accept_binary_units_and_off() {
        assert_eq!(parse_speed("500K"), Ok(500 * 1024));
        assert_eq!(parse_speed("2M"), Ok(2 * 1024 * 1024));
        assert_eq!(parse_speed("1.5MiB/s"), Ok(1536 * 1024));
        assert_eq!(parse_speed("4096"), Ok(4096));
        assert_eq!(parse_speed("off"), Ok(0));
        assert!(parse_speed("fast").is_err());
        assert!(parse_speed("10X").is_err());
    }

    #[test]
    fn speeds_accept_tebibytes_as_shown_for_very_high_caps() {
        assert_eq!(parse_speed("2T"), Ok(2 << 40));
        assert_eq!(parse_speed("1.5 TiB/s"), Ok(3 << 39));
        assert_eq!(parse_speed("1tb"), Ok(1 << 40));
    }

    #[test]
    fn counts_accept_whole_numbers_in_range_only() {
        assert_eq!(parse_count("8", 1, 16), Ok(8));
        assert_eq!(parse_count(" 16 ", 1, 16), Ok(16));
        assert_eq!(
            parse_count("0", 1, 16),
            Err("Enter a whole number from 1 to 16".into())
        );
        assert_eq!(
            parse_count("2.5", 1, 16),
            Err("Enter a whole number from 1 to 16".into())
        );
    }
}
