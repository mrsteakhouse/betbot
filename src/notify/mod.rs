//! Notifications: one summary message per tenant and run, sent to the
//! tenant's notifiers. Targets are configured with shoutrrr-style URLs
//! (`telegram://`, `discord://`, `ntfy://`, `generic://`) that are read from
//! Secret files. These URLs carry tokens: they are kept in [`Secret`]s, never
//! logged, and errors never contain them (reqwest errors are stripped of
//! their URL).

mod discord;
mod generic;
pub mod message;
mod ntfy;
mod telegram;

use std::borrow::Cow;
use std::fmt;
use std::time::Duration;

use reqwest::redirect::Policy;
use reqwest::{Client, Response, Url};
use thiserror::Error;

use crate::credentials::{CredentialsError, Secret};

const USER_AGENT: &str = concat!("betbot/", env!("CARGO_PKG_VERSION"));
const TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warning,
    Error,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub level: Level,
    pub title: String,
    pub body: String,
}

pub trait Notify: Send + Sync {
    fn send(&self, msg: &Message) -> impl Future<Output = Result<(), NotifyError>> + Send;
}

/// Errors never contain the notifier URL or parts of it that may be secret.
#[derive(Debug, Error)]
pub enum NotifyError {
    #[error("invalid notifier URL: {0}")]
    InvalidUrl(String),
    #[error("unsupported notifier URL scheme (supported: telegram, discord, ntfy, generic)")]
    UnsupportedScheme,
    #[error("notifier URL file: {0}")]
    Read(#[from] CredentialsError),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("rejected with HTTP status {0}")]
    Rejected(u16),
    #[error("{failed} of {total} recipients failed, first error: {first}")]
    Partial {
        failed: usize,
        total: usize,
        first: Box<NotifyError>,
    },
}

fn transport(e: reqwest::Error) -> NotifyError {
    NotifyError::Transport(e.without_url().to_string())
}

fn invalid(reason: impl Into<String>) -> NotifyError {
    NotifyError::InvalidUrl(reason.into())
}

/// Maps a non-success status to [`NotifyError::Rejected`]. The response body
/// is not read (it is not needed and might echo request data).
fn check_status(resp: Response) -> Result<(), NotifyError> {
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(NotifyError::Rejected(status.as_u16()))
    }
}

/// HTTP client shared by all notifiers: short timeouts, no redirects, no cookies.
pub fn http_client() -> Result<Client, NotifyError> {
    // Err only means a provider is already installed (e.g. by the platform client).
    let _ = rustls::crypto::ring::default_provider().install_default();
    Client::builder()
        .timeout(TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .user_agent(USER_AGENT)
        .redirect(Policy::none())
        .build()
        .map_err(transport)
}

enum Service {
    Telegram(telegram::Telegram),
    Discord(discord::Discord),
    Ntfy(ntfy::Ntfy),
    Generic(generic::Generic),
}

impl Service {
    fn parse(url: &Secret) -> Result<Self, NotifyError> {
        // url's ParseError never contains the input.
        let url = Url::parse(url.expose()).map_err(|e| invalid(e.to_string()))?;
        match url.scheme() {
            "telegram" => telegram::Telegram::parse(&url).map(Self::Telegram),
            "discord" => discord::Discord::parse(&url).map(Self::Discord),
            "ntfy" => ntfy::Ntfy::parse(&url).map(Self::Ntfy),
            "generic" => generic::Generic::parse(&url).map(Self::Generic),
            _ => Err(NotifyError::UnsupportedScheme),
        }
    }
}

/// A configured notification target.
pub struct Notifier {
    client: Client,
    service: Service,
}

impl fmt::Debug for Notifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Notifier({})", self.kind())
    }
}

/// Checks a notifier URL without network access (for `--check-config`).
pub fn validate_url(url: &Secret) -> Result<(), NotifyError> {
    Service::parse(url).map(|_| ())
}

impl Notifier {
    pub fn from_url(url: &Secret, client: Client) -> Result<Self, NotifyError> {
        Ok(Self {
            client,
            service: Service::parse(url)?,
        })
    }

    /// Service name (safe to log).
    pub fn kind(&self) -> &'static str {
        match self.service {
            Service::Telegram(_) => "telegram",
            Service::Discord(_) => "discord",
            Service::Ntfy(_) => "ntfy",
            Service::Generic(_) => "generic",
        }
    }

    /// Points Telegram/Discord at a local mock server (production always
    /// uses the official API hosts).
    #[cfg(test)]
    pub(crate) fn with_base(mut self, base: Url) -> Self {
        match &mut self.service {
            Service::Telegram(t) => t.base = base,
            Service::Discord(d) => d.base = base,
            Service::Ntfy(_) | Service::Generic(_) => {}
        }
        self
    }
}

impl Notify for Notifier {
    async fn send(&self, msg: &Message) -> Result<(), NotifyError> {
        match &self.service {
            Service::Telegram(s) => s.send(&self.client, msg).await,
            Service::Discord(s) => s.send(&self.client, msg).await,
            Service::Ntfy(s) => s.send(&self.client, msg).await,
            Service::Generic(s) => s.send(&self.client, msg).await,
        }
    }
}

/// Decoded query parameters of a notifier URL.
struct Params(Vec<(String, String)>);

impl Params {
    fn of(url: &Url) -> Self {
        Self(
            url.query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect(),
        )
    }

    /// Value of `key` (case-insensitive; the last occurrence wins).
    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }

    fn is_known(key: &str, known: &[&str]) -> bool {
        known.iter().any(|k| k.eq_ignore_ascii_case(key))
    }

    /// Fails on parameters not in `known` (catches typos). Only the parameter
    /// name is reported, never its value.
    fn reject_unknown(&self, service: &str, known: &[&str]) -> Result<(), NotifyError> {
        match self.0.iter().find(|(k, _)| !Self::is_known(k, known)) {
            Some((k, _)) => Err(invalid(format!("{service}: unknown parameter {k:?}"))),
            None => Ok(()),
        }
    }

    /// Parameters not in `known`, in order.
    fn unknown<'a>(&'a self, known: &'a [&str]) -> impl Iterator<Item = (&'a str, &'a str)> {
        self.0
            .iter()
            .filter(move |(k, _)| !Self::is_known(k, known))
            .map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

/// Parses a `yes`/`no` flag (shoutrrr style; `true`/`false` accepted too).
fn flag(service: &str, key: &str, value: Option<&str>) -> Result<Option<bool>, NotifyError> {
    match value.map(str::to_ascii_lowercase).as_deref() {
        None => Ok(None),
        Some("yes" | "true" | "1") => Ok(Some(true)),
        Some("no" | "false" | "0") => Ok(Some(false)),
        Some(_) => Err(invalid(format!("{service}: {key} must be yes or no"))),
    }
}

/// Decodes percent-encoding in URL components (userinfo, path), which
/// `Url` leaves encoded.
fn decode(s: &str) -> Result<String, NotifyError> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            let byte = bytes
                .get(i + 1..i + 3)
                .filter(|h| h.iter().all(u8::is_ascii_hexdigit))
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| invalid("malformed percent-encoding"))?;
            out.push(byte);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| invalid("percent-encoded value is not UTF-8"))
}

/// Token-like values are interpolated into URL paths.
fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Shortens `s` to at most `max` UTF-16 code units (the unit Telegram and
/// Discord count in), cutting on a char boundary and appending "…".
fn truncate(s: &str, max: usize) -> Cow<'_, str> {
    if s.encode_utf16().count() <= max {
        return Cow::Borrowed(s);
    }
    let budget = max.saturating_sub(1);
    let mut used = 0;
    let mut end = 0;
    for (i, c) in s.char_indices() {
        if used + c.len_utf16() > budget {
            break;
        }
        used += c.len_utf16();
        end = i + c.len_utf8();
    }
    Cow::Owned(format!("{}…", s.get(..end).unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn secret(s: &str) -> Secret {
        Secret::new(s.to_owned())
    }

    pub(super) fn msg(level: Level) -> Message {
        Message {
            level,
            title: "betbot alice: Spieltag 12 – 2 Tipps abgegeben".into(),
            body: "Spieltag 12\n\nFC Nordstadt - SV Südhafen  2:1".into(),
        }
    }

    pub(super) fn parse(url: &str) -> Result<Notifier, NotifyError> {
        Notifier::from_url(&secret(url), http_client().unwrap())
    }

    #[test]
    fn unsupported_scheme_and_garbage() {
        for url in [
            "https://example.test/hook",
            "smtp://user:pass@mail.test",
            "mailto:x@y.test",
        ] {
            assert!(
                matches!(parse(url), Err(NotifyError::UnsupportedScheme)),
                "{url}"
            );
        }
        for url in ["", "not a url", "123456:ABC-secret"] {
            assert!(
                matches!(parse(url), Err(NotifyError::InvalidUrl(_))),
                "{url}"
            );
        }
        assert!(validate_url(&secret("ntfy://ntfy.test/t")).is_ok());
        assert!(validate_url(&secret("ftp://x")).is_err());
    }

    #[test]
    fn errors_never_contain_secrets() {
        let cases = [
            "telegram://123456:SeCrEtToKeN@telegram",
            "telegram://123456:SeCrEtToKeN@telegram?chats=1&notification=maybe",
            "telegram://123456:SeCrEtToKeN@telegram?chats=1&foo=SeCrEtToKeN",
            "discord://SeCrEtToKeN@notanid",
            "discord://SeCrEtToKeN",
            "ntfy://u:SeCrEtToKeN@ntfy.test/",
            "ntfy://u:SeCrEtToKeN@ntfy.test/t?priority=9",
            "generic://u:SeCrEtToKeN@?x=1",
            "SeCrEtToKeN://x",
            "SeCrEtToKeN",
        ];
        for url in cases {
            let err = parse(url).unwrap_err();
            for text in [err.to_string(), format!("{err:?}")] {
                assert!(!text.contains("SeCrEtToKeN"), "{url}: {text}");
            }
        }
        let n = parse("telegram://123456:SeCrEtToKeN@telegram?chats=1").unwrap();
        assert!(!format!("{n:?}").contains("SeCrEtToKeN"));
    }

    #[test]
    fn decode_percent() {
        assert_eq!(decode("a%40b%3Ac").unwrap(), "a@b:c");
        assert_eq!(decode("%C3%BC").unwrap(), "ü");
        assert!(decode("%zz").is_err());
        assert!(decode("%+1").is_err());
        assert!(decode("%4").is_err());
        assert!(decode("%FF").is_err());
    }

    #[test]
    fn truncate_on_char_boundaries() {
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abcd", 3), "ab…");
        assert_eq!(truncate("üüüü", 3), "üü…");
        // An emoji is two UTF-16 units and is never split.
        assert_eq!(truncate("a😀b", 3), "a…");
        assert_eq!(truncate("a😀b", 4), "a😀b");
        assert_eq!(truncate("abc", 0), "…");
    }

    #[tokio::test]
    async fn connection_error_has_no_token() {
        // Bind and drop a listener to get a port nobody listens on.
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let base = Url::parse(&format!("http://{addr}")).unwrap();
        let urls = [
            format!(
                "generic://u:SeCrEtToKeN@{addr}/hook/SeCrEtToKeN?disabletls=yes&token=SeCrEtToKeN"
            ),
            format!("ntfy://:SeCrEtToKeN@{addr}/topic?scheme=http"),
        ];
        for url in urls {
            let n = parse(&url).unwrap();
            let err = n.send(&msg(Level::Info)).await.unwrap_err();
            for text in [err.to_string(), format!("{err:?}")] {
                assert!(!text.contains("SeCrEtToKeN"), "{text}");
            }
        }
        let n = parse("telegram://123456:SeCrEtToKeN@telegram?chats=1,2")
            .unwrap()
            .with_base(base.clone());
        let err = n.send(&msg(Level::Info)).await.unwrap_err();
        assert!(matches!(
            err,
            NotifyError::Partial {
                failed: 2,
                total: 2,
                ..
            }
        ));
        assert!(!format!("{err} {err:?}").contains("SeCrEtToKeN"));
        let n = parse("discord://SeCrEtToKeN@123456789")
            .unwrap()
            .with_base(base);
        let err = n.send(&msg(Level::Info)).await.unwrap_err();
        assert!(matches!(err, NotifyError::Transport(_)));
        assert!(!format!("{err} {err:?}").contains("SeCrEtToKeN"));
    }
}
