//! Telegram Bot API: `telegram://<bot id>:<secret>@telegram?chats=@channel,123456[&notification=no]`.
//! Sends one `sendMessage` per chat (plain text, no parse mode).

use reqwest::{Client, Url};

use super::{
    Message, NotifyError, Params, check_status, decode, flag, invalid, is_token, transport,
    truncate,
};
use crate::credentials::Secret;

const API: &str = "https://api.telegram.org";
/// Maximum message length (characters after entity parsing).
const MAX_TEXT: usize = 4096;
const KNOWN: &[&str] = &["chats", "notification"];

pub struct Telegram {
    /// `<bot id>:<secret>`.
    token: Secret,
    chats: Vec<String>,
    silent: bool,
    pub(super) base: Url,
}

impl Telegram {
    pub fn parse(url: &Url) -> Result<Self, NotifyError> {
        let params = Params::of(url);
        params.reject_unknown("telegram", KNOWN)?;
        let bot = decode(url.username())?;
        let secret = decode(url.password().unwrap_or_default())?;
        if bot.is_empty() || secret.is_empty() {
            return Err(invalid(
                "telegram: bot token missing (telegram://<bot id>:<secret>@telegram?chats=...)",
            ));
        }
        if !bot.bytes().all(|b| b.is_ascii_digit()) || !is_token(&secret) {
            return Err(invalid("telegram: malformed bot token"));
        }
        let chats: Vec<String> = params
            .get("chats")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_owned)
            .collect();
        if chats.is_empty() {
            return Err(invalid("telegram: chats is required"));
        }
        let silent = !flag("telegram", "notification", params.get("notification"))?.unwrap_or(true);
        let base = Url::parse(API).map_err(|e| invalid(e.to_string()))?;
        Ok(Self {
            token: Secret::new(format!("{bot}:{secret}")),
            chats,
            silent,
            base,
        })
    }

    pub async fn send(&self, client: &Client, msg: &Message) -> Result<(), NotifyError> {
        let mut endpoint = self.base.clone();
        endpoint
            .path_segments_mut()
            .map_err(|()| invalid("telegram: cannot build API URL"))?
            .pop_if_empty()
            .push(&format!("bot{}", self.token.expose()))
            .push("sendMessage");
        let text = truncate(&format!("{}\n\n{}", msg.title, msg.body), MAX_TEXT).into_owned();
        let silent = if self.silent { "true" } else { "false" };
        let mut errors = Vec::new();
        for chat in &self.chats {
            let form = [
                ("chat_id", chat.as_str()),
                ("text", text.as_str()),
                ("disable_notification", silent),
            ];
            let result = match client.post(endpoint.clone()).form(&form).send().await {
                Ok(resp) => check_status(resp),
                Err(e) => Err(transport(e)),
            };
            if let Err(e) = result {
                errors.push(e);
            }
        }
        let total = self.chats.len();
        let failed = errors.len();
        match errors.into_iter().next() {
            None => Ok(()),
            Some(first) if total == 1 => Err(first),
            Some(first) => Err(NotifyError::Partial {
                failed,
                total,
                first: Box::new(first),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::Level;
    use crate::notify::tests::{msg, parse};
    use crate::testutil::http_mock::{MockServer, Response};

    fn telegram(url: &str) -> Telegram {
        Telegram::parse(&Url::parse(url).unwrap()).unwrap()
    }

    #[test]
    fn parses_url() {
        let t = telegram(
            "telegram://123456:AA-b_c@telegram?chats=@my_channel,%20-100123&notification=no",
        );
        assert_eq!(t.token.expose(), "123456:AA-b_c");
        assert_eq!(t.chats, ["@my_channel", "-100123"]);
        assert!(t.silent);
        assert_eq!(t.base.as_str(), "https://api.telegram.org/");
        let t = telegram("telegram://1:x@telegram/?chats=42");
        assert!(!t.silent);
        assert_eq!(t.chats, ["42"]);
    }

    #[test]
    fn rejects_invalid() {
        for url in [
            "telegram://telegram?chats=1",
            "telegram://123456@telegram?chats=1",
            "telegram://:secret@telegram?chats=1",
            "telegram://abc:secret@telegram?chats=1",
            "telegram://1:se%2Fcret@telegram?chats=1",
            "telegram://1:x@telegram",
            "telegram://1:x@telegram?chats=,",
            "telegram://1:x@telegram?chats=1&notification=maybe",
            "telegram://1:x@telegram?chats=1&chat=2",
        ] {
            assert!(
                matches!(parse(url), Err(NotifyError::InvalidUrl(_))),
                "{url}"
            );
        }
    }

    #[tokio::test]
    async fn sends_one_message_per_chat() {
        let server = MockServer::start(|r| {
            if r.form().iter().any(|(k, v)| k == "chat_id" && v == "bad") {
                Response::status(400)
            } else {
                Response::ok(r#"{"ok":true}"#)
            }
        })
        .await;
        let n = parse("telegram://123456:AA-b_c@telegram?chats=@chan,bad,42")
            .unwrap()
            .with_base(server.base());
        let err = crate::notify::Notify::send(&n, &msg(Level::Warning))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, NotifyError::Partial { failed: 1, total: 3, first } if matches!(**first, NotifyError::Rejected(400))),
            "{err:?}"
        );
        let reqs = server.requests();
        assert_eq!(reqs.len(), 3);
        for r in &reqs {
            assert_eq!(r.method, "POST");
            assert_eq!(r.path(), "/bot123456:AA-b_c/sendMessage");
            assert_eq!(
                r.header("content-type"),
                "application/x-www-form-urlencoded"
            );
        }
        let form = reqs[0].form();
        assert_eq!(
            form,
            [
                ("chat_id".to_owned(), "@chan".to_owned()),
                (
                    "text".to_owned(),
                    "betbot alice: Spieltag 12 – 2 Tipps abgegeben\n\nSpieltag 12\n\nFC Nordstadt - SV Südhafen  2:1".to_owned()
                ),
                ("disable_notification".to_owned(), "false".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn single_chat_error_is_returned_as_is() {
        let server = MockServer::start(|_| Response::status(401)).await;
        let n = parse("telegram://1:x@telegram?chats=42")
            .unwrap()
            .with_base(server.base());
        let err = crate::notify::Notify::send(&n, &msg(Level::Info))
            .await
            .unwrap_err();
        assert!(matches!(err, NotifyError::Rejected(401)), "{err:?}");
    }

    #[tokio::test]
    async fn long_text_is_truncated() {
        let server = MockServer::start(|_| Response::ok("{}")).await;
        let n = parse("telegram://1:x@telegram?chats=42")
            .unwrap()
            .with_base(server.base());
        let long = Message {
            body: "ü".repeat(5000),
            ..msg(Level::Info)
        };
        crate::notify::Notify::send(&n, &long).await.unwrap();
        let form = server.requests()[0].form();
        let text = &form.iter().find(|(k, _)| k == "text").unwrap().1;
        assert_eq!(text.encode_utf16().count(), MAX_TEXT);
        assert!(text.ends_with("ü…"));
    }
}
