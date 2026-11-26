//! Reading an HLS playlist (`.m3u8`): a master lists the qualities, a media
//! playlist lists the pieces of one stream (spec 1c, section 6.2).

use crate::pieces::Piece;
use url::Url;

/// The most bytes of a playlist that are read.
pub const MAX_PLAYLIST_BYTES: usize = 16 * 1024 * 1024;

/// What a playlist turned out to be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Playlist {
    Master(Master),
    Media(Media),
}

/// A playlist of qualities.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Master {
    pub variants: Vec<Variant>,
    pub audio: Vec<AudioRendition>,
}

/// One quality of a master playlist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Variant {
    pub url: Url,
    pub height: Option<u32>,
    pub bandwidth: Option<u64>,
    pub codecs: Option<String>,
    pub audio_group: Option<String>,
}

/// A sound track a quality may use (`EXT-X-MEDIA:TYPE=AUDIO`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioRendition {
    pub group: String,
    pub name: String,
    pub language: Option<String>,
    pub default: bool,
    /// None when the sound is inside the quality's own stream.
    pub url: Option<Url>,
}

/// A playlist of pieces. `pieces[0]` is the init piece when it has `EXT-X-MAP`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Media {
    pub pieces: Vec<Piece>,
    /// False for a live stream (no `EXT-X-ENDLIST`).
    pub ended: bool,
}

/// Why a playlist cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HlsError {
    NotAPlaylist,
    Empty,
    TooBig,
    /// Encrypted in a way rustdm does not decrypt (DRM).
    Protected,
}

/// Read a playlist fetched from `base` (its final address, after redirects);
/// every link in it is resolved against `base`.
pub fn parse_playlist(text: &str, base: &Url) -> Result<Playlist, HlsError> {
    if text.len() > MAX_PLAYLIST_BYTES {
        return Err(HlsError::TooBig);
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    if !lines
        .next()
        .is_some_and(|first| first.starts_with("#EXTM3U"))
    {
        return Err(HlsError::NotAPlaylist);
    }
    let lines: Vec<&str> = lines.collect();
    if lines.iter().any(|l| l.starts_with(STREAM_INF)) {
        read_master(&lines, base).map(Playlist::Master)
    } else {
        read_media(&lines, base).map(Playlist::Media)
    }
}

const STREAM_INF: &str = "#EXT-X-STREAM-INF:";
const MEDIA_TAG: &str = "#EXT-X-MEDIA:";
const EXTINF: &str = "#EXTINF:";
const BYTERANGE: &str = "#EXT-X-BYTERANGE:";
const KEY_TAG: &str = "#EXT-X-KEY:";
const MAP_TAG: &str = "#EXT-X-MAP:";
const SEQUENCE: &str = "#EXT-X-MEDIA-SEQUENCE:";
const ENDLIST: &str = "#EXT-X-ENDLIST";

/// `KEY=VALUE` pairs separated by commas outside quotes; quotes are dropped.
fn attributes(list: &str) -> Vec<(String, String)> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in list.chars() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => parts.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    parts.push(current);
    parts
        .iter()
        .filter_map(|part| part.split_once('='))
        .map(|(k, v)| (k.trim().to_ascii_uppercase(), v.trim().to_string()))
        .collect()
}

fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

fn link(base: &Url, text: &str) -> Result<Url, HlsError> {
    base.join(text).map_err(|_| HlsError::NotAPlaylist)
}

fn read_master(lines: &[&str], base: &Url) -> Result<Master, HlsError> {
    let mut master = Master::default();
    let mut pending: Option<Vec<(String, String)>> = None;
    for line in lines {
        if let Some(list) = line.strip_prefix(STREAM_INF) {
            pending = Some(attributes(list));
        } else if let Some(list) = line.strip_prefix(MEDIA_TAG) {
            let attrs = attributes(list);
            if attr(&attrs, "TYPE") == Some("AUDIO") {
                master.audio.push(rendition(&attrs, base)?);
            }
        } else if !line.starts_with('#') {
            if let Some(attrs) = pending.take() {
                master.variants.push(variant(&attrs, link(base, line)?));
            }
        }
    }
    if master.variants.is_empty() {
        return Err(HlsError::Empty);
    }
    Ok(master)
}

fn variant(attrs: &[(String, String)], url: Url) -> Variant {
    let height = attr(attrs, "RESOLUTION")
        .and_then(|r| r.split_once(['x', 'X']))
        .and_then(|(_, h)| h.parse().ok());
    Variant {
        url,
        height,
        bandwidth: attr(attrs, "BANDWIDTH").and_then(|b| b.parse().ok()),
        codecs: attr(attrs, "CODECS").map(String::from),
        audio_group: attr(attrs, "AUDIO").map(String::from),
    }
}

fn rendition(attrs: &[(String, String)], base: &Url) -> Result<AudioRendition, HlsError> {
    Ok(AudioRendition {
        group: attr(attrs, "GROUP-ID").unwrap_or_default().to_string(),
        name: attr(attrs, "NAME").unwrap_or_default().to_string(),
        language: attr(attrs, "LANGUAGE").map(String::from),
        default: attr(attrs, "DEFAULT") == Some("YES"),
        url: attr(attrs, "URI").map(|uri| link(base, uri)).transpose()?,
    })
}

/// The key in force: its link and its explicit IV, if any.
type KeyInForce = Option<(Url, Option<[u8; 16]>)>;

/// What the tags before a piece's link said about it.
#[derive(Default)]
struct Pending {
    duration_ms: u64,
    /// Length, and offset when given.
    range: Option<(u64, Option<u64>)>,
}

/// Builds the piece list of a media playlist line by line.
struct MediaReader<'a> {
    base: &'a Url,
    sequence: u64,
    key: KeyInForce,
    pending: Pending,
    /// The previous slice's link and end, for a range without an offset.
    last_slice: Option<(Url, u64)>,
    media: Media,
    /// Pieces that play (the init piece is not counted).
    played: u64,
}

fn read_media(lines: &[&str], base: &Url) -> Result<Media, HlsError> {
    let mut reader = MediaReader {
        base,
        sequence: 0,
        key: None,
        pending: Pending::default(),
        last_slice: None,
        media: Media::default(),
        played: 0,
    };
    for line in lines {
        reader.line(line)?;
        if reader.media.pieces.len() > crate::pieces::MAX_PIECES {
            return Err(HlsError::TooBig);
        }
    }
    if reader.played == 0 {
        return Err(HlsError::Empty);
    }
    Ok(reader.media)
}

impl MediaReader<'_> {
    fn line(&mut self, line: &str) -> Result<(), HlsError> {
        if let Some(value) = line.strip_prefix(SEQUENCE) {
            self.sequence = value.trim().parse().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix(EXTINF) {
            let seconds = value.split(',').next().unwrap_or_default().trim();
            let seconds: f64 = seconds.parse().unwrap_or(0.0);
            self.pending.duration_ms = (seconds * 1000.0).round().max(0.0) as u64;
        } else if let Some(value) = line.strip_prefix(BYTERANGE) {
            self.pending.range = Some(byte_range(value)?);
        } else if let Some(list) = line.strip_prefix(KEY_TAG) {
            self.key = key_in_force(&attributes(list), self.base)?;
        } else if let Some(list) = line.strip_prefix(MAP_TAG) {
            self.init_piece(&attributes(list))?;
        } else if line.starts_with(ENDLIST) {
            self.media.ended = true;
        } else if !line.starts_with('#') {
            let pending = std::mem::take(&mut self.pending);
            let piece = self.piece(link(self.base, line)?, pending.range, pending.duration_ms);
            self.media.pieces.push(piece);
            self.played += 1;
        }
        Ok(())
    }

    /// `EXT-X-MAP` before any piece is the init piece; a later one is ignored.
    fn init_piece(&mut self, attrs: &[(String, String)]) -> Result<(), HlsError> {
        if !self.media.pieces.is_empty() {
            return Ok(());
        }
        let Some(uri) = attr(attrs, "URI") else {
            return Err(HlsError::NotAPlaylist);
        };
        let range = attr(attrs, "BYTERANGE").map(byte_range).transpose()?;
        let piece = self.piece(link(self.base, uri)?, range, 0);
        self.media.pieces.push(piece);
        Ok(())
    }

    fn piece(&mut self, url: Url, range: Option<(u64, Option<u64>)>, duration_ms: u64) -> Piece {
        let range = range.map(|(len, offset)| {
            let follows = || match &self.last_slice {
                Some((last, end)) if *last == url => *end,
                _ => 0,
            };
            let offset = offset.unwrap_or_else(follows);
            crate::pieces::Slice { offset, len }
        });
        if let Some(slice) = range {
            self.last_slice = Some((url.clone(), slice.offset + slice.len));
        }
        let key = self.key.as_ref().map(|(key_url, iv)| crate::pieces::Key {
            url: key_url.clone(),
            iv: iv.unwrap_or_else(|| (u128::from(self.sequence + self.played)).to_be_bytes()),
        });
        Piece {
            url,
            range,
            duration_ms,
            key,
        }
    }
}

/// `len[@offset]`.
fn byte_range(value: &str) -> Result<(u64, Option<u64>), HlsError> {
    let value = value.trim();
    let (len, offset) = match value.split_once('@') {
        Some((len, offset)) => (len, Some(offset)),
        None => (value, None),
    };
    let number = |s: &str| s.trim().parse::<u64>().map_err(|_| HlsError::NotAPlaylist);
    Ok((number(len)?, offset.map(number).transpose()?))
}

fn key_in_force(attrs: &[(String, String)], base: &Url) -> Result<KeyInForce, HlsError> {
    match attr(attrs, "METHOD") {
        Some("NONE") => Ok(None),
        Some("AES-128") => {
            let uri = attr(attrs, "URI").ok_or(HlsError::NotAPlaylist)?;
            let iv = attr(attrs, "IV").map(iv_of).transpose()?;
            Ok(Some((link(base, uri)?, iv)))
        }
        _ => Err(HlsError::Protected),
    }
}

/// `0x` and 32 hex digits.
fn iv_of(text: &str) -> Result<[u8; 16], HlsError> {
    let hex = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .filter(|h| h.len() == 32)
        .ok_or(HlsError::NotAPlaylist)?;
    u128::from_str_radix(hex, 16)
        .map(u128::to_be_bytes)
        .map_err(|_| HlsError::NotAPlaylist)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pieces::{Key, Slice, MAX_PIECES};

    fn base() -> Url {
        Url::parse("https://cdn.test/v/index.m3u8").unwrap()
    }

    fn media(text: &str) -> Media {
        match parse_playlist(text, &base()) {
            Ok(Playlist::Media(media)) => media,
            other => panic!("not a media playlist: {other:?}"),
        }
    }

    const MASTER: &str = "#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud1\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,URI=\"a1/prog_index.m3u8\"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud1\",NAME=\"Inside\",DEFAULT=NO
#EXT-X-STREAM-INF:AVERAGE-BANDWIDTH=7968416,BANDWIDTH=8001098,CODECS=\"avc1.64002a,mp4a.40.2\",RESOLUTION=1920x1080,FRAME-RATE=60.000,AUDIO=\"aud1\"
v9/prog_index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=3216424,CODECS=\"avc1.640020,mp4a.40.2\",RESOLUTION=1280x720,AUDIO=\"aud1\"
v6/prog_index.m3u8
";

    #[test]
    fn a_master_lists_variants_and_audio() {
        let Ok(Playlist::Master(master)) = parse_playlist(MASTER, &base()) else {
            panic!("not a master");
        };

        let heights: Vec<_> = master.variants.iter().map(|v| v.height).collect();
        assert_eq!(heights, [Some(1080), Some(720)]);
        let top = &master.variants[0];
        assert_eq!(top.bandwidth, Some(8_001_098));
        assert_eq!(top.codecs.as_deref(), Some("avc1.64002a,mp4a.40.2"));
        assert_eq!(top.audio_group.as_deref(), Some("aud1"));
        assert_eq!(top.url.as_str(), "https://cdn.test/v/v9/prog_index.m3u8");
        let english = &master.audio[0];
        assert_eq!(english.group, "aud1");
        assert_eq!(english.name, "English");
        assert_eq!(english.language.as_deref(), Some("en"));
        assert!(english.default);
        assert_eq!(
            english.url.as_ref().map(Url::as_str),
            Some("https://cdn.test/v/a1/prog_index.m3u8")
        );
    }

    #[test]
    fn a_rendition_without_uri_has_no_url() {
        let Ok(Playlist::Master(master)) = parse_playlist(MASTER, &base()) else {
            panic!("not a master");
        };

        assert_eq!(master.audio[1].name, "Inside");
        assert!(!master.audio[1].default);
        assert_eq!(master.audio[1].url, None);
    }

    #[test]
    fn separate_pieces_keep_order_and_durations() {
        let list = media(
            "#EXTM3U\n#EXT-X-TARGETDURATION:7\n#EXTINF:6.006,\nseg0.ts\n#EXTINF:6.006,\nseg1.ts\n\
             #EXTINF:6.006,title\nseg2.ts\n#EXT-X-ENDLIST\n",
        );

        let names: Vec<_> = list.pieces.iter().map(|p| p.url.path()).collect();
        assert_eq!(names, ["/v/seg0.ts", "/v/seg1.ts", "/v/seg2.ts"]);
        let durations: Vec<_> = list.pieces.iter().map(|p| p.duration_ms).collect();
        assert_eq!(durations, [6006, 6006, 6006]);
        assert!(list.ended);
        assert!(list
            .pieces
            .iter()
            .all(|p| p.range.is_none() && p.key.is_none()));
    }

    #[test]
    fn byte_ranges_with_and_without_offset() {
        let list = media(
            "#EXTM3U\n#EXT-X-MAP:URI=\"main.mp4\",BYTERANGE=\"719@0\"\n\
             #EXTINF:6.00000,\t\n#EXT-X-BYTERANGE:1508000@719\nmain.mp4\n\
             #EXTINF:6.00000,\t\n#EXT-X-BYTERANGE:1510244\nmain.mp4\n#EXT-X-ENDLIST\n",
        );

        let ranges: Vec<_> = list.pieces.iter().map(|p| p.range).collect();
        assert_eq!(
            ranges,
            [
                Some(Slice {
                    offset: 0,
                    len: 719
                }),
                Some(Slice {
                    offset: 719,
                    len: 1_508_000
                }),
                Some(Slice {
                    offset: 1_508_719,
                    len: 1_510_244
                }),
            ]
        );
        assert_eq!(
            list.pieces[0].duration_ms, 0,
            "the init piece plays no time"
        );
        assert_eq!(list.pieces[1].duration_ms, 6000);
        assert!(list.pieces.iter().all(|p| p.url.path() == "/v/main.mp4"));
    }

    #[test]
    fn keys_apply_from_their_tag_on() {
        let list = media(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:7\n#EXTINF:4,\na.ts\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"k.bin\"\n#EXTINF:4,\nb.ts\n#EXTINF:4,\nc.ts\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"k2.bin\",IV=0x000102030405060708090A0B0C0D0E0F\n\
             #EXTINF:4,\nd.ts\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:4,\ne.ts\n#EXT-X-ENDLIST\n",
        );

        let sequence_iv = |n: u8| {
            let mut iv = [0u8; 16];
            iv[15] = n;
            iv
        };
        let key = |name: &str, iv| {
            Some(Key {
                url: base().join(name).unwrap(),
                iv,
            })
        };
        let keys: Vec<_> = list.pieces.iter().map(|p| p.key.clone()).collect();
        let explicit: [u8; 16] = std::array::from_fn(|i| i as u8);
        assert_eq!(
            keys,
            [
                None,
                key("k.bin", sequence_iv(8)),
                key("k.bin", sequence_iv(9)),
                key("k2.bin", explicit),
                None,
            ]
        );
    }

    #[test]
    fn links_resolve_against_the_playlist_address() {
        let at = Url::parse("https://cdn.test/v/x/index.m3u8?tok=1").unwrap();
        let text = "#EXTM3U\n#EXTINF:4,\nseg1.ts\n#EXTINF:4,\n/abs/seg2.ts\n\
                    #EXTINF:4,\nhttps://other.test/seg3.ts?sig=2\n#EXT-X-ENDLIST\n";

        let Ok(Playlist::Media(list)) = parse_playlist(text, &at) else {
            panic!("not media");
        };

        let urls: Vec<_> = list.pieces.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://cdn.test/v/x/seg1.ts",
                "https://cdn.test/abs/seg2.ts",
                "https://other.test/seg3.ts?sig=2",
            ]
        );
    }

    #[test]
    fn no_endlist_is_not_ended() {
        let list = media("#EXTM3U\n#EXTINF:4,\na.ts\n");

        assert!(!list.ended);
    }

    #[test]
    fn sample_aes_is_protected() {
        for method in ["SAMPLE-AES", "SAMPLE-AES-CTR", "SAMPLE-AES-CENC"] {
            let text = format!(
                "#EXTM3U\n#EXT-X-KEY:METHOD={method},URI=\"skd://x\"\n#EXTINF:4,\na.ts\n#EXT-X-ENDLIST\n"
            );
            assert_eq!(
                parse_playlist(&text, &base()),
                Err(HlsError::Protected),
                "{method}"
            );
        }
    }

    #[test]
    fn not_a_playlist() {
        assert_eq!(
            parse_playlist("<html>", &base()),
            Err(HlsError::NotAPlaylist)
        );
        assert_eq!(parse_playlist("#EXTM3U\n", &base()), Err(HlsError::Empty));
        let big = format!("#EXTM3U\n#{}", "x".repeat(MAX_PLAYLIST_BYTES));
        assert_eq!(parse_playlist(&big, &base()), Err(HlsError::TooBig));
        let many = format!(
            "#EXTM3U\n{}#EXT-X-ENDLIST\n",
            "#EXTINF:1,\na.ts\n".repeat(MAX_PIECES + 1)
        );
        assert_eq!(parse_playlist(&many, &base()), Err(HlsError::TooBig));
    }

    #[test]
    fn a_byte_order_mark_and_blank_lines_are_fine() {
        let list = media("\u{feff}#EXTM3U\r\n\r\n#EXTINF:4,\r\na.ts\r\n#EXT-X-ENDLIST\r\n");

        assert_eq!(list.pieces.len(), 1);
        assert_eq!(list.pieces[0].url.path(), "/v/a.ts");
    }

    #[test]
    fn unknown_tags_are_ignored() {
        let list = media(
            "#EXTM3U\n#EXT-X-PROGRAM-DATE-TIME:2026-01-01T00:00:00Z\n#EXTINF:4,\na.ts\n\
             #EXT-X-DISCONTINUITY\n#EXTINF:4,\nb.ts\n#EXT-X-ENDLIST\n",
        );

        assert_eq!(list.pieces.len(), 2);
    }
}
