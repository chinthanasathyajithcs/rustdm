//! What the extension and the host say to each other, and the shape checks on it.
//! Content limits for cookies, Referer and User-Agent belong to the daemon.

use crate::ipc::protocol::AddDownloadParams;
use crate::request_context::RequestContext;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Version of this message protocol, reported by `ping`.
pub const PROTOCOL_VERSION: u32 = 1;
/// Longest URL the host passes on.
pub const MAX_URL_BYTES: usize = 8 * 1024;
/// Longest file name the host passes on.
pub const MAX_FILENAME_BYTES: usize = 255;

/// The only two things the extension can ask for, as they arrive. Anything
/// else, including any extra field (such as `destination`), is refused.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum WireRequest {
    Ping {
        id: u64,
    },
    Add {
        id: u64,
        url: String,
        #[serde(default)]
        filename: Option<String>,
        #[serde(default)]
        context: Option<Map<String, Value>>,
    },
}

/// A checked request. The context is already the daemon's own type, so its
/// content limits (cookie count, sizes, names) are enforced by the daemon.
#[derive(Debug, Clone, PartialEq)]
pub enum HostRequest {
    Ping {
        id: u64,
    },
    Add {
        id: u64,
        url: String,
        filename: Option<String>,
        context: Option<RequestContext>,
    },
}

impl HostRequest {
    /// Decode one frame. On failure, returns the request id when one can be
    /// found, so the reply can still name it, and a fixed message. serde's own
    /// text is never passed on: it can quote the URL or file name.
    pub fn parse(bytes: &[u8]) -> Result<Self, (Option<u64>, String)> {
        let wire: WireRequest = serde_json::from_slice(bytes).map_err(|e| {
            let id = serde_json::from_slice::<Value>(bytes)
                .ok()
                .and_then(|v| v.get("id").and_then(Value::as_u64));
            let message = if e.is_data() {
                "invalid request: unknown type, unknown or missing field, or wrong value type"
            } else {
                "invalid request: not valid JSON"
            };
            (id, message.to_string())
        })?;
        match wire {
            WireRequest::Ping { id } => Ok(Self::Ping { id }),
            WireRequest::Add {
                id,
                url,
                filename,
                context,
            } => {
                let context = check_add(&url, filename.as_deref(), context)
                    .map_err(|message| (Some(id), message))?;
                Ok(Self::Add {
                    id,
                    url,
                    filename,
                    context,
                })
            }
        }
    }

    pub fn id(&self) -> u64 {
        match self {
            Self::Ping { id } | Self::Add { id, .. } => *id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HostErrorKind {
    NotRunning,
    SpawnFailed,
    Rejected,
    Timeout,
    Duplicate,
    Invalid,
    TooLarge,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HostError {
    pub kind: HostErrorKind,
    /// The daemon's JSON-RPC error code, when the daemon was the one that refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HostReply {
    pub id: Option<u64>,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<HostError>,
}

impl HostReply {
    pub fn success(id: u64, result: Value) -> Self {
        Self {
            id: Some(id),
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn failure(
        id: Option<u64>,
        kind: HostErrorKind,
        code: Option<i32>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(HostError {
                kind,
                code,
                message: message.into(),
            }),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        // A reply is plain data; serialising it cannot fail.
        serde_json::to_vec(self).unwrap_or_default()
    }
}

fn check_add(
    url: &str,
    filename: Option<&str>,
    context: Option<Map<String, Value>>,
) -> Result<Option<RequestContext>, String> {
    if url.len() > MAX_URL_BYTES {
        return Err(format!("url is longer than {MAX_URL_BYTES} bytes"));
    }
    match url::Url::parse(url) {
        Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => {}
        Ok(_) => return Err("url must start with http:// or https://".to_string()),
        Err(_) => return Err("url is not a valid web address".to_string()),
    }
    if let Some(name) = filename {
        check_filename(name)?;
    }
    context.map(typed_context).transpose()
}

fn check_filename(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_FILENAME_BYTES {
        return Err(format!("filename must be 1 to {MAX_FILENAME_BYTES} bytes"));
    }
    if name.contains(['/', '\\']) || name.contains("..") || name.chars().any(char::is_control) {
        return Err("filename must not contain path separators, '..' or control characters".into());
    }
    Ok(())
}

/// The context as the daemon's own type. Shape problems get fixed messages
/// that name the field only: serde's text can quote cookie values.
fn typed_context(context: Map<String, Value>) -> Result<RequestContext, String> {
    for (key, value) in &context {
        let fits = match key.as_str() {
            "cookies" => value.is_array(),
            "referer" | "user_agent" => value.is_string(),
            _ => true,
        };
        if !fits {
            return Err(format!("context.{key} has the wrong type"));
        }
    }
    serde_json::from_value(Value::Object(context))
        .map_err(|_| "context has a missing or wrongly typed field".to_string())
}

/// The `download.add` parameters for an `add` request. The host never sets a
/// destination, connection count or speed cap, so the daemon's defaults apply.
pub fn add_params(
    url: &str,
    filename: Option<&str>,
    context: Option<&RequestContext>,
) -> AddDownloadParams {
    AddDownloadParams {
        url: url.to_string(),
        destination: None,
        filename: filename.map(str::to_string),
        connections: None,
        max_bytes_per_sec: None,
        context: context.cloned(),
        quality: None,
        added_by: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(value: Value) -> Result<HostRequest, (Option<u64>, String)> {
        HostRequest::parse(value.to_string().as_bytes())
    }

    #[test]
    fn ping_and_add_parse() {
        assert_eq!(
            parse(json!({"type": "ping", "id": 1})).unwrap(),
            HostRequest::Ping { id: 1 }
        );
        let add = parse(json!({
            "type": "add", "id": 2, "url": "https://example.com/x.zip",
            "filename": "x.zip", "context": {"referer": "https://example.com/"}
        }))
        .unwrap();
        assert_eq!(add.id(), 2);
    }

    #[test]
    fn an_unknown_type_is_refused_and_keeps_its_id() {
        let (id, _) = parse(json!({"type": "shutdown", "id": 7})).unwrap_err();
        assert_eq!(id, Some(7));
        let (id, _) = parse(json!({"type": "daemon.shutdown"})).unwrap_err();
        assert_eq!(id, None);
    }

    #[test]
    fn unknown_fields_are_refused_including_destination() {
        for extra in ["destination", "connections", "max_bytes_per_sec", "other"] {
            let mut message = json!({"type": "add", "id": 3, "url": "https://example.com/x"});
            message[extra] = json!("/etc");
            assert!(parse(message).is_err(), "{extra} must be refused");
        }
        assert!(parse(json!({"type": "ping", "id": 1, "url": "https://example.com"})).is_err());
    }

    #[test]
    fn error_messages_never_quote_what_the_extension_sent() {
        let secret = "https://example.com/private-token-123";
        let cases = [
            json!({"type": secret, "id": 1}),
            json!({"type": "add", "id": 1, "url": secret, "destination": secret}),
            json!({"type": "add", "id": 1, "url": 5, "filename": secret}),
            json!({"type": "add", "id": 1, "url": secret, "filename": format!("{secret}/x")}),
            json!({"type": "add", "id": 1, "url": format!("ftp://{secret}")}),
        ];
        for case in cases {
            let (_, message) = parse(case).unwrap_err();
            assert!(!message.contains("private-token"), "{message}");
        }
        let (_, message) = HostRequest::parse(b"{\"type\":\"ping\", private-token").unwrap_err();
        assert!(!message.contains("private-token"), "{message}");
    }

    #[test]
    fn garbage_is_refused() {
        assert!(HostRequest::parse(b"not json").is_err());
        assert!(HostRequest::parse(b"[]").is_err());
        assert!(parse(json!({"type": "ping"})).is_err());
    }

    #[test]
    fn only_http_and_https_urls_pass() {
        for url in [
            "file:///etc/passwd",
            "blob:https://example.com/x",
            "data:text/plain,hi",
            "ftp://example.com/x",
            "not a url",
            "",
        ] {
            assert!(
                parse(json!({"type": "add", "id": 1, "url": url})).is_err(),
                "{url}"
            );
        }
        assert!(parse(json!({"type": "add", "id": 1, "url": "http://example.com/x"})).is_ok());
    }

    #[test]
    fn a_refused_add_names_its_request() {
        let (id, message) = parse(json!({"type": "add", "id": 9, "url": "ftp://x/y"})).unwrap_err();
        assert_eq!(id, Some(9));
        assert!(message.contains("http"), "{message}");
    }

    #[test]
    fn url_and_filename_limits() {
        let long_url = format!("https://example.com/{}", "a".repeat(MAX_URL_BYTES));
        assert!(parse(json!({"type": "add", "id": 1, "url": long_url})).is_err());

        for name in ["", "a/b", "a\\b", "..", "a..b", "a\nb", &"n".repeat(256)] {
            let message =
                json!({"type": "add", "id": 1, "url": "https://example.com/x", "filename": name});
            assert!(parse(message).is_err(), "{name:?}");
        }
        let ok = json!({"type": "add", "id": 1, "url": "https://example.com/x",
            "filename": "n".repeat(255)});
        assert!(parse(ok).is_ok());
    }

    #[test]
    fn context_must_have_the_right_shape() {
        for context in [
            json!({"cookies": "x"}),
            json!({"referer": 1}),
            json!({"user_agent": []}),
        ] {
            let message = json!({"type": "add", "id": 1, "url": "https://example.com/x",
                "context": context});
            assert!(parse(message).is_err(), "{context}");
        }
        let not_an_object =
            json!({"type": "add", "id": 1, "url": "https://example.com/x", "context": 5});
        assert!(parse(not_an_object).is_err());
    }

    #[test]
    fn a_cookie_of_the_wrong_shape_is_refused_without_quoting_it() {
        for cookies in [
            json!([5]),
            json!([{"value": "secret-value", "domain": "example.com"}]),
            json!([{"name": "a", "value": "secret-value", "domain": "example.com",
                "expires": "secret-value"}]),
        ] {
            let message = json!({"type": "add", "id": 1, "url": "https://example.com/x",
                "context": {"cookies": cookies}});
            let (id, text) = parse(message).unwrap_err();
            assert_eq!(id, Some(1));
            assert!(!text.contains("secret-value"), "{text}");
        }
    }

    #[test]
    fn the_extensions_cookie_shape_parses_into_the_typed_context() {
        let message = json!({"type": "add", "id": 1, "url": "https://example.com/x",
            "context": {
                "cookies": [{"name": "sid", "value": "v", "domain": ".example.com", "path": "/a",
                    "secure": true, "host_only": false, "expires": 1893456000.5}],
                "referer": "https://example.com/page", "user_agent": "UA"}});
        let HostRequest::Add { context, .. } = parse(message).unwrap() else {
            panic!("an add");
        };
        let context = context.unwrap();
        assert_eq!(context.cookies.len(), 1);
        assert_eq!(context.cookies[0].expires, Some(1_893_456_000));
        assert!(context.cookies[0].secure);
        assert_eq!(context.referer.as_deref(), Some("https://example.com/page"));
        assert_eq!(context.user_agent.as_deref(), Some("UA"));
    }

    #[test]
    fn daemon_params_carry_the_context_and_no_other_settings() {
        let context = RequestContext {
            referer: Some("https://example.com/".to_string()),
            ..RequestContext::default()
        };
        let params = add_params("https://example.com/x", Some("x.zip"), Some(&context));
        assert_eq!(
            serde_json::to_value(params).unwrap(),
            json!({"url": "https://example.com/x", "filename": "x.zip",
                "context": {"referer": "https://example.com/"}})
        );
        let bare = add_params("https://example.com/x", None, None);
        assert_eq!(
            serde_json::to_value(bare).unwrap(),
            json!({"url": "https://example.com/x"})
        );
    }

    #[test]
    fn replies_have_the_documented_shape() {
        let ok = HostReply::success(2, json!({"download_id": "dl_5", "filename": "x.zip"}));
        assert_eq!(
            serde_json::from_slice::<Value>(&ok.to_bytes()).unwrap(),
            json!({"id": 2, "ok": true, "result": {"download_id": "dl_5", "filename": "x.zip"}})
        );
        let err = HostReply::failure(Some(2), HostErrorKind::Duplicate, Some(-32005), "dup");
        assert_eq!(
            serde_json::from_slice::<Value>(&err.to_bytes()).unwrap(),
            json!({"id": 2, "ok": false,
                "error": {"kind": "duplicate", "code": -32005, "message": "dup"}})
        );
        let no_id = HostReply::failure(None, HostErrorKind::TooLarge, None, "big");
        let value = serde_json::from_slice::<Value>(&no_id.to_bytes()).unwrap();
        assert_eq!(value["id"], Value::Null);
        assert_eq!(value["error"]["kind"], "too_large");
        assert!(value["error"].get("code").is_none());
    }
}
