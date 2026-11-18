//! SHA-256 helpers: reading the checksum files the projects publish, and
//! hashing a file as it sits on disk.

use ring::digest::{Context, SHA256};
use std::io::{self, Read};
use std::path::Path;

const HEX_LEN: usize = 64;

/// Lower-case hex of `bytes`.
pub fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0xf)] as char);
    }
    out
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    to_hex(ring::digest::digest(&SHA256, bytes).as_ref())
}

/// Hash a file without reading it all into memory.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut context = Context::new(&SHA256);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        context.update(&buf[..n]);
    }
    Ok(to_hex(context.finish().as_ref()))
}

/// A SHA-256 written as 64 hex digits, in either case, as lower case.
pub fn normalize_hash(text: &str) -> Option<String> {
    let text = text.trim();
    if text.len() != HEX_LEN || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(text.to_ascii_lowercase())
}

/// One entry of a checksum file: the file's base name and its SHA-256 (lower case hex).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SumLine {
    pub name: String,
    pub sha256: String,
}

/// Read every entry of a checksum file in any of the three formats the projects use:
///
/// - coreutils `sha256sum` (`<hash>  <name>` or `<hash> *<name>`), which is
///   yt-dlp's `SHA2-256SUMS`, BtbN's `checksums.sha256` and deno's Linux and
///   macOS `.sha256sum`;
/// - PowerShell `Get-FileHash | Format-List` (`Hash : <HASH>` then `Path : <path>`),
///   which is deno's Windows `.sha256sum`.
///
/// Lines in neither shape are skipped; a file with no usable line gives an empty list.
pub fn parse_sums(text: &str) -> Vec<SumLine> {
    let mut found = Vec::new();
    let mut pending_hash: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(entry) = parse_coreutils_line(line) {
            found.push(entry);
        } else if let Some(value) = powershell_value(line, "Hash") {
            pending_hash = normalize_hash(value);
        } else if let Some(value) = powershell_value(line, "Path") {
            if let Some(sha256) = pending_hash.take() {
                found.push(SumLine {
                    name: base_name(value).to_string(),
                    sha256,
                });
            }
        }
    }
    found
}

/// The hash recorded for `name`, if the file lists it exactly once.
pub fn expected_for(text: &str, name: &str) -> Option<String> {
    unique_hash(&parse_sums(text), name)
}

/// The hash recorded for `name` in `lines`, if it is listed exactly once.
pub fn unique_hash(lines: &[SumLine], name: &str) -> Option<String> {
    let mut matches = lines.iter().filter(|line| line.name == name);
    let first = matches.next()?;
    matches.next().is_none().then(|| first.sha256.clone())
}

/// Read a checksum file that must be exactly yt-dlp's `SHA2-256SUMS`: every
/// non-empty line is `<64 lower-case hex digits>  <file name>` (two spaces; the
/// name only letters, digits, `.`, `_` and `-`; a trailing CR is allowed). One
/// line of any other shape makes the whole file unreadable (`None`), so nothing
/// in a signed file is guessed at.
pub fn parse_strict_sums(text: &str) -> Option<Vec<SumLine>> {
    let mut found = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let (hash, name) = line.split_once("  ")?;
        let lower_hex = hash.len() == HEX_LEN
            && hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        let plain_name = !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if !lower_hex || !plain_name {
            return None;
        }
        found.push(SumLine {
            name: name.to_string(),
            sha256: hash.to_string(),
        });
    }
    Some(found)
}

fn parse_coreutils_line(line: &str) -> Option<SumLine> {
    let (hash, rest) = line.split_once(char::is_whitespace)?;
    let sha256 = normalize_hash(hash)?;
    // Binary mode puts a `*` in front of the name.
    let name = rest.trim_start().trim_start_matches('*');
    (!name.is_empty()).then(|| SumLine {
        name: base_name(name).to_string(),
        sha256,
    })
}

fn powershell_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(key)?;
    let value = rest.trim_start().strip_prefix(':')?;
    Some(value.trim())
}

fn base_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const YTDLP_SUMS: &str = include_str!("../../tests/fixtures/yt-dlp/SHA2-256SUMS");
    const BTBN_SUMS: &str = include_str!("../../tests/fixtures/checksums/btbn-checksums.sha256");
    const DENO_WINDOWS: &str =
        include_str!("../../tests/fixtures/checksums/deno-windows.sha256sum");
    const DENO_LINUX: &str = include_str!("../../tests/fixtures/checksums/deno-linux.sha256sum");

    #[test]
    fn reads_the_coreutils_format_of_the_ytdlp_release() {
        assert_eq!(
            expected_for(YTDLP_SUMS, "yt-dlp.exe").as_deref(),
            Some("f5389d3698ecde4ef06f4e6b1ce0294fe9ac1396228c8990b8d3416d6d177a7d")
        );
        assert_eq!(
            expected_for(YTDLP_SUMS, "yt-dlp_linux").as_deref(),
            Some("767ba9dce7052aa196f8b12652f433d76de128ba168966fa84b6394d5e6c6795")
        );
        assert_eq!(expected_for(YTDLP_SUMS, "nothing.exe"), None);
    }

    #[test]
    fn reads_the_btbn_and_deno_linux_files() {
        assert_eq!(
            expected_for(
                BTBN_SUMS,
                "ffmpeg-n8.1.3-27-g8dca1586f4-win64-lgpl-shared-8.1.zip"
            )
            .as_deref(),
            Some("5cef7bc48e513f3cee7c881e8b2999d835541edb1846c3e89443a67f8f8556ab")
        );
        assert_eq!(
            expected_for(DENO_LINUX, "deno-x86_64-unknown-linux-gnu.zip").as_deref(),
            Some("e890975bc2549807bbcc0b584ffafda72cc7c93349dd99c80483dd784f6c4609")
        );
    }

    #[test]
    fn reads_the_powershell_get_filehash_format() {
        let lines = parse_sums(DENO_WINDOWS);
        assert_eq!(
            lines,
            vec![SumLine {
                name: "deno-x86_64-pc-windows-msvc.zip".into(),
                sha256: "cba84b26e0df5ae7f74fc66b86a95b866872a86292ebd84fa1e959927edd93bf".into(),
            }]
        );
    }

    #[test]
    fn binary_mode_marker_and_bad_lines_are_handled() {
        let hash = "a".repeat(64);
        let text = format!(
            "{hash} *tool.zip\nnot a checksum line\n{} short.zip\n",
            "b".repeat(63)
        );
        let lines = parse_sums(&text);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].name, "tool.zip");
        assert_eq!(parse_sums(""), vec![]);
    }

    #[test]
    fn the_strict_reader_takes_the_real_ytdlp_file_and_nothing_loose() {
        let lines = parse_strict_sums(YTDLP_SUMS).expect("the real file is strict");
        assert_eq!(lines.len(), 19);
        assert_eq!(
            unique_hash(&lines, "yt-dlp_linux").as_deref(),
            Some("767ba9dce7052aa196f8b12652f433d76de128ba168966fa84b6394d5e6c6795")
        );
        let hash = "a".repeat(64);
        // CRLF line ends and a missing final newline are fine.
        assert_eq!(
            parse_strict_sums(&format!("{hash}  a.exe\r\n{hash}  b\r\n\r\n{hash}  c"))
                .map(|l| l.len()),
            Some(3)
        );
        let upper = "A".repeat(64);
        for bad in [
            format!("{hash} *a.exe"),
            format!("{hash} a.exe"),
            format!("{hash}   a.exe"),
            format!("{hash}  dir/a.exe"),
            format!("{hash}  dir\\a.exe"),
            format!("{upper}  a.exe"),
            format!("{}  a.exe", "a".repeat(63)),
            format!("{hash}  a.exe\nnot a line"),
            format!("{hash}  a b.exe"),
            format!("{hash}  "),
            format!("Hash : {upper}\nPath : C:\\a.exe"),
            format!("{hash}  a.exe \n"),
        ] {
            assert_eq!(parse_strict_sums(&bad), None, "{bad:?}");
        }
        assert_eq!(parse_strict_sums(""), Some(vec![]));
    }

    #[test]
    fn a_name_listed_twice_is_ambiguous() {
        let text = format!("{0}  a.zip\n{1}  a.zip\n", "a".repeat(64), "b".repeat(64));
        assert_eq!(expected_for(&text, "a.zip"), None);
    }

    #[test]
    fn hashes_a_file_in_pieces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        let data = vec![7u8; 200_000];
        std::fs::write(&path, &data).unwrap();
        assert_eq!(sha256_file(&path).unwrap(), sha256_hex(&data));
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(sha256_file(&dir.path().join("missing")).is_err());
    }
}
