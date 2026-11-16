//! Discord webhook: `discord://<token>@<webhook id>` posts to
//! `https://discord.com/api/webhooks/<webhook id>/<token>`.

use reqwest::{Client, Url};
use serde::Serialize;

use super::{
    Message, NotifyError, Params, check_status, decode, invalid, is_token, transport, truncate,
};
use crate::credentials::Secret;

const API: &str = "https://discord.com";
/// Maximum length of `content`.
const MAX_CONTENT: usize = 2000;

pub struct Discord {
    webhook_id: String,
    token: Secret,
    pub(super) base: Url,
}

#[derive(Serialize)]
struct AllowedMentions {
    parse: [&'static str; 0],
}

#[derive(Serialize)]
struct Payload<'a> {
    content: &'a str,
    /// Never ping anyone, whatever the text contains.
    allowed_mentions: AllowedMentions,
}

impl Discord {
    pub fn parse(url: &Url) -> Result<Self, NotifyError> {
        Params::of(url).reject_unknown("discord", &[])?;
        let token = decode(url.username())?;
        if !is_token(&token) || url.password().is_some() {
            return Err(invalid(
                "discord: webhook token missing or malformed (discord://<token>@<webhook id>)",
            ));
        }
        let webhook_id = url.host_str().unwrap_or_default();
        if webhook_id.is_empty() || !webhook_id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid("discord: webhook id must be numeric"));
        }
        if !matches!(url.path(), "" | "/") {
            return Err(invalid("discord: unexpected path"));
        }
        let base = Url::parse(API).map_err(|e| invalid(e.to_string()))?;
        Ok(Self {
            webhook_id: webhook_id.to_owned(),
            token: Secret::new(token),
            base,
        })
    }

    pub async fn send(&self, client: &Client, msg: &Message) -> Result<(), NotifyError> {
        let mut endpoint = self.base.clone();
        endpoint
            .path_segments_mut()
            .map_err(|()| invalid("discord: cannot build webhook URL"))?
            .pop_if_empty()
            .extend([
                "api",
                "webhooks",
                self.webhook_id.as_str(),
                self.token.expose(),
            ]);
        let text = format!("**{}**\n{}", msg.title, msg.body);
        let content = truncate(&text, MAX_CONTENT);
        let payload = Payload {
            content: &content,
            allowed_mentions: AllowedMentions { parse: [] },
        };
        let resp = client
            .post(endpoint)
            .json(&payload)
            .send()
            .await
            .map_err(transport)?;
        check_status(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::tests::{msg, parse};
    use crate::notify::{Level, Notify};
    use crate::testutil::http_mock::{MockServer, Response};

    #[test]
    fn parses_url() {
        let d =
            Discord::parse(&Url::parse("discord://Ab-c_D1@123456789012345678").unwrap()).unwrap();
        assert_eq!(d.webhook_id, "123456789012345678");
        assert_eq!(d.token.expose(), "Ab-c_D1");
        assert_eq!(d.base.as_str(), "https://discord.com/");
    }

    #[test]
    fn rejects_invalid() {
        for url in [
            "discord://123456",
            "discord://tok@abc",
            "discord://tok:x@123",
            "discord://to%2Fk@123",
            "discord://tok@123/extra",
            "discord://tok@123?splitlines=yes",
        ] {
            assert!(
                matches!(parse(url), Err(NotifyError::InvalidUrl(_))),
                "{url}"
            );
        }
    }

    #[tokio::test]
    async fn posts_json_to_webhook() {
        let server = MockServer::start(|_| Response::status(204)).await;
        let n = parse("discord://Ab-c_D1@123456789")
            .unwrap()
            .with_base(server.base());
        n.send(&msg(Level::Info)).await.unwrap();
        let reqs = server.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].method, "POST");
        assert_eq!(reqs[0].path(), "/api/webhooks/123456789/Ab-c_D1");
        assert_eq!(reqs[0].header("content-type"), "application/json");
        assert_eq!(
            reqs[0].body,
            r#"{"content":"**betbot alice: Spieltag 12 – 2 Tipps abgegeben**\nSpieltag 12\n\nFC Nordstadt - SV Südhafen  2:1","allowed_mentions":{"parse":[]}}"#
        );
    }

    #[tokio::test]
    async fn rejection_and_truncation() {
        let server = MockServer::start(|r| {
            if r.body.len() > 3000 {
                Response::status(400)
            } else {
                Response::status(404)
            }
        })
        .await;
        let n = parse("discord://tok@1").unwrap().with_base(server.base());
        let long = Message {
            body: "x".repeat(5000),
            ..msg(Level::Error)
        };
        // Truncated below 3000 bytes, so the mock answers 404.
        let err = n.send(&long).await.unwrap_err();
        assert!(matches!(err, NotifyError::Rejected(404)), "{err:?}");
    }
}
