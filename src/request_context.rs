//! What a download carries besides its URL: the browser's cookies, the page it
//! came from, and the browser's User-Agent. Sites that gate a file behind a
//! sign-in check these, so the engine's client sends them on every request.
//!
//! Cookie values are secrets. Nothing here prints one: `Debug` redacts them and
//! validation errors name the field, never its contents.

use crate::engine::{client_builder, Timeouts};
use crate::error::Error;
use reqwest::cookie::Jar;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, REFERER};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Most cookies one download may carry.
pub const MAX_COOKIES: usize = 300;
/// Longest cookie name.
pub const MAX_COOKIE_NAME_BYTES: usize = 256;
/// Longest cookie name and value together.
pub const MAX_COOKIE_PAIR_BYTES: usize = 4096;
/// Longest domain name (RFC 1035).
pub const MAX_DOMAIN_BYTES: usize = 253;
/// Longest label inside a domain name.
const MAX_DOMAIN_LABEL_BYTES: usize = 63;
/// Longest Referer URL.
pub const MAX_REFERER_BYTES: usize = 8 * 1024;
/// Longest User-Agent.
pub const MAX_USER_AGENT_BYTES: usize = 1024;
/// A page title longer than this (in characters) is ignored: no real title is
/// near it, so only a misbehaving page sends one. Titles are never cut.
pub const MAX_TITLE_CHARS: usize = 1000;
/// A cookie is never kept longer than this once seeded (browsers cap at 400 days).
const MAX_COOKIE_LIFETIME_SECS: i64 = 400 * 24 * 60 * 60;

fn root_path() -> String {
    "/".to_string()
}

/// Seconds since the epoch; whole and fractional JSON numbers both land here.
fn whole_seconds<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<i64>, D::Error> {
    let seconds = Option::<f64>::deserialize(deserializer)?;
    Ok(seconds.map(|s| s.floor() as i64))
}

/// One cookie as `chrome.cookies.getAll` reports it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    #[serde(default = "root_path")]
    pub path: String,
    #[serde(default)]
    pub secure: bool,
    /// Sent only to exactly `domain`, not to its subdomains.
    #[serde(default)]
    pub host_only: bool,
    /// Expiry in whole seconds since the epoch; none for a session cookie.
    #[serde(
        default,
        deserialize_with = "whole_seconds",
        skip_serializing_if = "Option::is_none"
    )]
    pub expires: Option<i64>,
}

impl fmt::Debug for ContextCookie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContextCookie")
            .field("name", &self.name)
            .field("value", &"<redacted>")
            .field("domain", &self.domain)
            .field("path", &self.path)
            .field("secure", &self.secure)
            .field("host_only", &self.host_only)
            .field("expires", &self.expires)
            .finish()
    }
}

/// Cookies, Referer and User-Agent that go with a download.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestContext {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cookies: Vec<ContextCookie>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub referer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// The title of the page the link came from. Names a video whose own
    /// name is weak; never sent to any server and never logged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl fmt::Debug for RequestContext {
    /// Counts and the Referer's origin only: a Referer's query may hold a token.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestContext")
            .field("cookies", &self.cookies.len())
            .field("referer", &self.referer_origin())
            .field("user_agent", &self.user_agent.is_some())
            .field("title", &self.title.is_some())
            .finish()
    }
}

/// The current time as seconds since the epoch.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl RequestContext {
    /// True when there is nothing to send.
    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty() && self.referer.is_none() && self.user_agent.is_none()
    }

    /// Scheme, host and port of the Referer, for logs.
    pub fn referer_origin(&self) -> Option<String> {
        let url = url::Url::parse(self.referer.as_deref()?).ok()?;
        Some(url.origin().ascii_serialization())
    }

    /// A checked copy: expired cookies are dropped, the Referer loses its
    /// fragment and any user name and password, and an empty Referer or
    /// User-Agent (Chrome reports `""` for none) becomes absent. The error
    /// names the field that is wrong, never its value.
    pub fn validated(&self, now_unix: i64) -> Result<RequestContext, String> {
        if self.cookies.len() > MAX_COOKIES {
            return Err(format!("cookies: more than {MAX_COOKIES} cookies"));
        }
        let mut cookies = Vec::with_capacity(self.cookies.len());
        for (index, cookie) in self.cookies.iter().enumerate() {
            check_cookie(cookie).map_err(|problem| format!("cookies[{index}].{problem}"))?;
            if cookie.expires.is_none_or(|at| at > now_unix) {
                cookies.push(cookie.clone());
            }
        }
        Ok(RequestContext {
            cookies,
            referer: clean_referer(self.referer.as_deref())?,
            user_agent: clean_user_agent(self.user_agent.as_deref())?,
            title: clean_title(self.title.as_deref()),
        })
    }
}

/// The title without control characters and outer spaces. Empty, or longer
/// than `MAX_TITLE_CHARS`, gives none; the download goes ahead either way.
fn clean_title(title: Option<&str>) -> Option<String> {
    let kept: String = title?.chars().filter(|c| !c.is_control()).collect();
    let kept = kept.trim();
    let usable = !kept.is_empty() && kept.chars().count() <= MAX_TITLE_CHARS;
    usable.then(|| kept.to_string())
}

/// RFC 7230 token characters.
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)
}

fn check_cookie(cookie: &ContextCookie) -> Result<(), String> {
    if cookie.name.is_empty()
        || cookie.name.len() > MAX_COOKIE_NAME_BYTES
        || !cookie.name.chars().all(is_token_char)
    {
        return Err("name: not a valid cookie name".to_string());
    }
    if cookie.name.len() + cookie.value.len() > MAX_COOKIE_PAIR_BYTES {
        return Err("value: cookie is too large".to_string());
    }
    if has_cookie_breaker(&cookie.value) {
        return Err("value: contains a control character or ';'".to_string());
    }
    if !cookie.path.starts_with('/') {
        return Err("path: must start with '/'".to_string());
    }
    if has_cookie_breaker(&cookie.path) {
        return Err("path: contains a control character or ';'".to_string());
    }
    check_domain(&cookie.domain, cookie.host_only)
}

/// A control character or `;` would end the cookie early and let the rest act
/// as attributes or as another cookie.
fn has_cookie_breaker(text: &str) -> bool {
    text.chars().any(|c| c.is_control() || c == ';')
}

fn check_domain(domain: &str, host_only: bool) -> Result<(), String> {
    let bad = || Err("domain: not a valid host name".to_string());
    if domain.len() > MAX_DOMAIN_BYTES {
        return bad();
    }
    let host = match domain.strip_prefix('.') {
        Some(_) if host_only => return bad(),
        Some(rest) => rest,
        None => domain,
    };
    let label_ok = |label: &str| {
        !label.is_empty()
            && label.len() <= MAX_DOMAIN_LABEL_BYTES
            && label
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    };
    if host.split('.').all(label_ok) {
        Ok(())
    } else {
        bad()
    }
}

fn clean_referer(referer: Option<&str>) -> Result<Option<String>, String> {
    let Some(text) = referer.filter(|r| !r.is_empty()) else {
        return Ok(None);
    };
    let bad = |why: &str| Err(format!("referer: {why}"));
    if text.len() > MAX_REFERER_BYTES {
        return bad("too long");
    }
    let Ok(mut url) = url::Url::parse(text) else {
        return bad("not a URL");
    };
    if !matches!(url.scheme(), "http" | "https") {
        return bad("must be an http or https URL");
    }
    url.set_fragment(None);
    // These only fail for URLs that cannot carry credentials, which http(s) can.
    let _ = url.set_username("");
    let _ = url.set_password(None);
    Ok(Some(url.to_string()))
}

fn clean_user_agent(user_agent: Option<&str>) -> Result<Option<String>, String> {
    let Some(text) = user_agent.filter(|u| !u.is_empty()) else {
        return Ok(None);
    };
    if text.len() > MAX_USER_AGENT_BYTES {
        return Err("user_agent: too long".to_string());
    }
    if !text.chars().all(|c| c == ' ' || c.is_ascii_graphic()) {
        return Err("user_agent: only visible ASCII and spaces are allowed".to_string());
    }
    Ok(Some(text.to_string()))
}

/// The `Set-Cookie` text that makes the jar treat `cookie` as the browser did.
/// An absolute expiry becomes `Max-Age` from `now_unix`, which the jar reads the same way.
fn set_cookie_text(cookie: &ContextCookie, now_unix: i64) -> String {
    let mut text = format!("{}={}; Path={}", cookie.name, cookie.value, cookie.path);
    if !cookie.host_only {
        text.push_str(&format!(
            "; Domain={}",
            cookie.domain.trim_start_matches('.')
        ));
    }
    if cookie.secure {
        text.push_str("; Secure");
    }
    if let Some(at) = cookie.expires {
        let left = at
            .saturating_sub(now_unix)
            .clamp(0, MAX_COOKIE_LIFETIME_SECS);
        text.push_str(&format!("; Max-Age={left}"));
    }
    text
}

/// A jar holding `ctx`'s cookies, each added as if its own site had set it.
/// The jar then applies domain, path, secure and expiry rules on every request,
/// redirects included.
pub fn seed_jar(ctx: &RequestContext, now_unix: i64) -> Arc<Jar> {
    let jar = Jar::default();
    for cookie in &ctx.cookies {
        let host = cookie.domain.trim_start_matches('.');
        let Ok(origin) = url::Url::parse(&format!("https://{host}/")) else {
            continue;
        };
        jar.add_cookie_str(&set_cookie_text(cookie, now_unix), &origin);
    }
    Arc::new(jar)
}

/// The shared client's settings with `ctx`'s cookie jar, User-Agent and Referer added.
fn browser_builder(
    ctx: &RequestContext,
    timeouts: Timeouts,
) -> Result<reqwest::ClientBuilder, Error> {
    let mut builder = client_builder(timeouts).cookie_provider(seed_jar(ctx, now_unix()));
    if let Some(user_agent) = &ctx.user_agent {
        builder = builder.user_agent(header_value(user_agent, "user agent")?);
    }
    if let Some(referer) = &ctx.referer {
        let mut headers = HeaderMap::new();
        headers.insert(REFERER, header_value(referer, "referer")?);
        // Without this, reqwest would swap in the previous URL after a redirect.
        builder = builder.default_headers(headers).referer(false);
    }
    Ok(builder)
}

/// A client for one download: the usual timeouts, a jar seeded from the
/// browser's cookies, and the browser's Referer and User-Agent on every request.
/// `ctx` should have passed [`RequestContext::validated`].
pub fn client_for(ctx: &RequestContext, timeouts: Timeouts) -> Result<reqwest::Client, Error> {
    browser_builder(ctx, timeouts)?.build().map_err(Error::Http)
}

/// A client for one video stream: built like [`client_for`] (or the shared client
/// when `ctx` is `None`), then `headers` become default headers. A stream's own
/// User-Agent and Referer replace the browser's, because its address was made
/// for another client. A header that cannot be sent is skipped; the caller has
/// already filtered `headers` to an allowlist.
pub fn stream_client(
    ctx: Option<&RequestContext>,
    headers: &[(String, String)],
    timeouts: Timeouts,
) -> Result<reqwest::Client, Error> {
    let mut builder = match ctx {
        Some(ctx) => browser_builder(ctx, timeouts)?,
        None => client_builder(timeouts),
    };
    let mut stream_headers = HeaderMap::new();
    for (name, value) in headers {
        let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) else {
            continue;
        };
        stream_headers.insert(name, value);
    }
    if stream_headers.contains_key(REFERER) {
        // Without this, reqwest would swap in the previous URL after a redirect.
        builder = builder.referer(false);
    }
    // Set after the browser's headers: each key here replaces the browser's.
    builder
        .default_headers(stream_headers)
        .build()
        .map_err(Error::Http)
}

fn header_value(text: &str, what: &str) -> Result<HeaderValue, Error> {
    HeaderValue::from_str(text).map_err(|_| Error::Protocol(format!("invalid {what}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::cookie::CookieStore;

    const NOW: i64 = 1_800_000_000;

    fn cookie(name: &str, value: &str, domain: &str) -> ContextCookie {
        ContextCookie {
            name: name.to_string(),
            value: value.to_string(),
            domain: domain.to_string(),
            path: "/".to_string(),
            secure: false,
            host_only: false,
            expires: None,
        }
    }

    fn context(cookies: Vec<ContextCookie>) -> RequestContext {
        RequestContext {
            cookies,
            ..Default::default()
        }
    }

    fn error_of(ctx: &RequestContext) -> String {
        ctx.validated(NOW).unwrap_err()
    }

    fn sent_to(jar: &Jar, url: &str) -> Option<String> {
        let url = url::Url::parse(url).unwrap();
        jar.cookies(&url)
            .map(|value| value.to_str().unwrap().to_string())
    }

    #[test]
    fn a_browser_cookie_parses_with_defaults_and_fractional_expiry() {
        let json = r#"{"name":"SID","value":"x","domain":".example.com","expires":1767225600.9}"#;
        let parsed: ContextCookie = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.path, "/");
        assert!(!parsed.secure && !parsed.host_only);
        assert_eq!(parsed.expires, Some(1_767_225_600));

        let session: ContextCookie =
            serde_json::from_str(r#"{"name":"a","value":"b","domain":"x.com"}"#).unwrap();
        assert_eq!(session.expires, None);
    }

    #[test]
    fn a_context_survives_a_json_round_trip() {
        let ctx = RequestContext {
            cookies: vec![ContextCookie {
                expires: Some(NOW + 5),
                secure: true,
                ..cookie("SID", "abc", ".example.com")
            }],
            referer: Some("https://example.com/files".to_string()),
            user_agent: Some("Mozilla/5.0".to_string()),
            title: None,
        };
        let back: RequestContext =
            serde_json::from_str(&serde_json::to_string(&ctx).unwrap()).unwrap();
        assert_eq!(back, ctx);
        assert_eq!(
            serde_json::to_string(&RequestContext::default()).unwrap(),
            "{}"
        );
    }

    #[test]
    fn debug_output_never_shows_a_cookie_value_or_the_referer_query() {
        let ctx = RequestContext {
            cookies: vec![cookie("SID", "super-secret-value", "example.com")],
            referer: Some("https://example.com/page?token=hunter2".to_string()),
            user_agent: None,
            title: None,
        };
        let shown = format!("{ctx:?} {:?}", ctx.cookies[0]);
        assert!(!shown.contains("super-secret-value"), "{shown}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains("<redacted>"));
        assert!(shown.contains("https://example.com"));
    }

    #[test]
    fn expired_cookies_are_dropped_and_live_ones_kept() {
        let mut old = cookie("old", "1", "example.com");
        old.expires = Some(NOW - 1);
        let mut at_now = cookie("now", "1", "example.com");
        at_now.expires = Some(NOW);
        let mut future = cookie("new", "1", "example.com");
        future.expires = Some(NOW + 60);
        let session = cookie("session", "1", "example.com");

        let checked = context(vec![old, at_now, future, session])
            .validated(NOW)
            .unwrap();
        let names: Vec<_> = checked.cookies.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["new", "session"]);
    }

    #[test]
    fn header_injection_in_a_cookie_is_refused_by_field_without_echoing_it() {
        let attacks = [
            ("name", cookie("a b", "secret", "example.com")),
            ("name", cookie("a;b", "secret", "example.com")),
            ("name", cookie("", "secret", "example.com")),
            ("value", cookie("a", "secret\r\nX-Evil: 1", "example.com")),
            (
                "value",
                cookie("a", "secret; Domain=evil.com", "example.com"),
            ),
            ("value", cookie("a", "secret\u{7f}", "example.com")),
            ("domain", cookie("a", "secret", "example.com/evil")),
            ("domain", cookie("a", "secret", "exa mple.com")),
            ("domain", cookie("a", "secret", "")),
            ("domain", cookie("a", "secret", "a..com")),
        ];
        for (field, bad) in attacks {
            let message = error_of(&context(vec![bad.clone()]));
            assert!(message.starts_with("cookies[0]."), "{message}");
            assert!(message.contains(&format!(".{field}:")), "{message}");
            assert!(!message.contains("secret"), "{message}");
        }

        let mut bad_path = cookie("a", "v", "example.com");
        bad_path.path = "/x\r\nHost: evil".to_string();
        assert!(error_of(&context(vec![bad_path])).contains(".path:"));
        let mut relative = cookie("a", "v", "example.com");
        relative.path = "x".to_string();
        assert!(error_of(&context(vec![relative])).contains(".path:"));
    }

    #[test]
    fn cookie_size_and_count_limits_apply() {
        let long_name = "n".repeat(MAX_COOKIE_NAME_BYTES + 1);
        assert!(error_of(&context(vec![cookie(&long_name, "v", "example.com")])).contains("name"));

        let big_value = "v".repeat(MAX_COOKIE_PAIR_BYTES);
        assert!(error_of(&context(vec![cookie("a", &big_value, "example.com")])).contains("value"));
        let just_fits = "v".repeat(MAX_COOKIE_PAIR_BYTES - 1);
        context(vec![cookie("a", &just_fits, "example.com")])
            .validated(NOW)
            .unwrap();

        let many = (0..=MAX_COOKIES)
            .map(|i| cookie(&format!("c{i}"), "v", "example.com"))
            .collect();
        assert!(error_of(&context(many)).starts_with("cookies:"));
        let exactly = (0..MAX_COOKIES)
            .map(|i| cookie(&format!("c{i}"), "v", "example.com"))
            .collect();
        context(exactly).validated(NOW).unwrap();

        let long_domain = format!("{}.com", "a".repeat(MAX_DOMAIN_BYTES));
        assert!(error_of(&context(vec![cookie("a", "v", &long_domain)])).contains("domain"));
    }

    #[test]
    fn a_leading_dot_on_the_domain_is_fine_except_for_a_host_only_cookie() {
        context(vec![cookie("a", "v", ".example.com")])
            .validated(NOW)
            .unwrap();
        let mut host_only = cookie("a", "v", ".example.com");
        host_only.host_only = true;
        assert!(error_of(&context(vec![host_only])).contains("domain"));
    }

    #[test]
    fn the_referer_loses_its_fragment_and_credentials() {
        let ctx = RequestContext {
            referer: Some("https://user:pass@example.com/files?a=1#section".to_string()),
            ..Default::default()
        };
        let checked = ctx.validated(NOW).unwrap();
        assert_eq!(
            checked.referer.as_deref(),
            Some("https://example.com/files?a=1")
        );
    }

    #[test]
    fn a_bad_referer_is_refused_and_an_empty_one_means_none() {
        for referer in [
            "ftp://example.com/",
            "javascript:alert(1)",
            "not a url",
            &format!("https://example.com/{}", "a".repeat(MAX_REFERER_BYTES)),
        ] {
            let ctx = RequestContext {
                referer: Some(referer.to_string()),
                ..Default::default()
            };
            assert!(error_of(&ctx).starts_with("referer:"));
        }
        let empty = RequestContext {
            referer: Some(String::new()),
            user_agent: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(empty.validated(NOW).unwrap(), RequestContext::default());
    }

    #[test]
    fn a_user_agent_is_visible_ascii_and_spaces_within_a_kilobyte() {
        let ok = RequestContext {
            user_agent: Some("Mozilla/5.0 (Windows NT 10.0; Win64) Chrome/130".to_string()),
            ..Default::default()
        };
        assert!(ok.validated(NOW).unwrap().user_agent.is_some());
        for bad in ["a\r\nX: y", "tab\there", "caf\u{e9}", &"u".repeat(1025)] {
            let ctx = RequestContext {
                user_agent: Some(bad.to_string()),
                ..Default::default()
            };
            assert!(error_of(&ctx).starts_with("user_agent:"));
        }
    }

    #[test]
    fn a_host_only_cookie_goes_to_its_own_host_and_nowhere_else() {
        let mut host_only = cookie("sid", "1", "example.com");
        host_only.host_only = true;
        let jar = seed_jar(&context(vec![host_only]), NOW);
        assert_eq!(
            sent_to(&jar, "https://example.com/a").as_deref(),
            Some("sid=1")
        );
        assert_eq!(sent_to(&jar, "https://www.example.com/a"), None);
        assert_eq!(sent_to(&jar, "https://other.org/a"), None);
    }

    #[test]
    fn a_domain_cookie_goes_to_subdomains_too() {
        let jar = seed_jar(&context(vec![cookie("sid", "1", ".example.com")]), NOW);
        assert_eq!(
            sent_to(&jar, "https://example.com/").as_deref(),
            Some("sid=1")
        );
        assert_eq!(
            sent_to(&jar, "https://cdn.example.com/").as_deref(),
            Some("sid=1")
        );
        assert_eq!(sent_to(&jar, "https://notexample.com/"), None);
    }

    #[test]
    fn a_cookie_is_sent_only_under_its_path() {
        let mut scoped = cookie("sid", "1", "example.com");
        scoped.path = "/private".to_string();
        let jar = seed_jar(&context(vec![scoped]), NOW);
        assert!(sent_to(&jar, "https://example.com/private/file.zip").is_some());
        assert!(sent_to(&jar, "https://example.com/public/file.zip").is_none());
    }

    #[test]
    fn a_secure_cookie_is_never_sent_over_plain_http() {
        let mut secure = cookie("sid", "1", "example.com");
        secure.secure = true;
        let jar = seed_jar(&context(vec![secure]), NOW);
        assert!(sent_to(&jar, "https://example.com/").is_some());
        assert!(sent_to(&jar, "http://example.com/").is_none());
    }

    #[test]
    fn a_cookie_past_its_expiry_is_not_sent() {
        let mut short = cookie("sid", "1", "example.com");
        short.expires = Some(now_unix() + 3600);
        let jar = seed_jar(&context(vec![short.clone()]), now_unix());
        assert!(sent_to(&jar, "https://example.com/").is_some());

        short.expires = Some(now_unix() - 10);
        let jar = seed_jar(&context(vec![short]), now_unix());
        assert!(sent_to(&jar, "https://example.com/").is_none());
    }

    #[test]
    fn a_far_off_expiry_is_cut_to_400_days_and_a_past_one_to_nothing() {
        let day = 24 * 60 * 60;
        let mut c = cookie("sid", "1", "example.com");
        c.expires = Some(NOW + 1000 * day);
        assert!(set_cookie_text(&c, NOW).ends_with(&format!("; Max-Age={}", 400 * day)));
        c.expires = Some(NOW + 10 * day);
        assert!(set_cookie_text(&c, NOW).ends_with(&format!("; Max-Age={}", 10 * day)));
        c.expires = Some(NOW - day);
        assert!(set_cookie_text(&c, NOW).ends_with("; Max-Age=0"));
        c.expires = None;
        assert!(!set_cookie_text(&c, NOW).contains("Max-Age"));
    }

    #[test]
    fn client_for_rejects_a_header_value_that_cannot_be_sent() {
        let ctx = RequestContext {
            user_agent: Some("bad\r\nagent".to_string()),
            ..Default::default()
        };
        assert!(matches!(
            client_for(&ctx, Timeouts::default()),
            Err(Error::Protocol(_))
        ));
        assert!(client_for(&RequestContext::default(), Timeouts::default()).is_ok());
    }

    fn titled(title: &str) -> Option<String> {
        RequestContext {
            title: Some(title.to_string()),
            ..RequestContext::default()
        }
        .validated(NOW)
        .unwrap()
        .title
    }

    #[test]
    fn a_title_is_trimmed_and_loses_control_characters() {
        assert_eq!(titled("  Angel\u{0}One\n  ").as_deref(), Some("AngelOne"));
    }

    #[test]
    fn an_empty_title_becomes_none() {
        assert_eq!(titled("   "), None);
        assert_eq!(titled("\n\t"), None);
    }

    #[test]
    fn a_long_real_title_is_kept_whole() {
        assert_eq!(titled(&"a".repeat(150)), Some("a".repeat(150)));
        assert_eq!(
            titled(&"a".repeat(MAX_TITLE_CHARS)),
            Some("a".repeat(MAX_TITLE_CHARS))
        );
    }

    #[test]
    fn a_title_over_the_safety_limit_is_ignored() {
        assert_eq!(MAX_TITLE_CHARS, 1000);
        assert_eq!(titled(&"a".repeat(MAX_TITLE_CHARS + 1)), None);
    }

    #[test]
    fn debug_never_shows_the_title() {
        let ctx = RequestContext {
            title: Some("Secret Show".to_string()),
            ..RequestContext::default()
        };
        assert!(!format!("{ctx:?}").contains("Secret"));
    }
}
