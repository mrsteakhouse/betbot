//! Generic webhook: `generic://[user:password@]host[:port]/path[?disabletls=yes&template=json|plain&titlekey=title&messagekey=message]`.
//! POSTs to `https://host[:port]/path` (`http` with `disabletls=yes`). Other
//! query parameters are forwarded to the target. `template=json` (default)
//! sends `{"title", "message", "level"}` (keys configurable), `plain` sends
//! title and body as `text/plain`.

use std::collections::BTreeMap;

use reqwest::{Client, Url};

use super::{Message, NotifyError, Params, check_status, decode, flag, invalid, transport};
use crate::credentials::Secret;

const KNOWN: &[&str] = &["disabletls", "template", "titlekey", "messagekey"];
const LEVEL_KEY: &str = "level";

enum Template {
    Json {
        title_key: String,
        message_key: String,
    },
    Plain,
}

pub struct Generic {
    /// May carry tokens in path or query: never logged.
    target: Url,
    basic_auth: Option<(String, Secret)>,
    template: Template,
}

impl Generic {
    pub fn parse(url: &Url) -> Result<Self, NotifyError> {
        let params = Params::of(url);
        let scheme = if flag("generic", "disabletls", params.get("disabletls"))?.unwrap_or(false) {
            "http"
        } else {
            "https"
        };
        let host = url.host_str().unwrap_or_default();
        if host.is_empty() {
            return Err(invalid("generic: host missing"));
        }
        let mut target = Url::parse(&format!("{scheme}://{host}/"))
            .map_err(|e| invalid(format!("generic: {e}")))?;
        target
            .set_port(url.port())
            .map_err(|()| invalid("generic: invalid port"))?;
        target.set_path(url.path());
        let forwarded: Vec<(&str, &str)> = params.unknown(KNOWN).collect();
        if !forwarded.is_empty() {
            target.query_pairs_mut().extend_pairs(forwarded);
        }

        let user = decode(url.username())?;
        let password = url.password().map(decode).transpose()?;
        let basic_auth = match (user.is_empty(), password) {
            (true, None) => None,
            (_, password) => Some((user, Secret::new(password.unwrap_or_default()))),
        };

        let template = match params
            .get("template")
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            None | Some("json") => {
                let title_key = params.get("titlekey").unwrap_or("title").to_owned();
                let message_key = params.get("messagekey").unwrap_or("message").to_owned();
                if title_key.is_empty()
                    || message_key.is_empty()
                    || title_key == message_key
                    || title_key == LEVEL_KEY
                    || message_key == LEVEL_KEY
                {
                    return Err(invalid(
                        "generic: titlekey and messagekey must be distinct, non-empty and not \"level\"",
                    ));
                }
                Template::Json {
                    title_key,
                    message_key,
                }
            }
            Some("plain") => Template::Plain,
            Some(_) => return Err(invalid("generic: template must be json or plain")),
        };
        Ok(Self {
            target,
            basic_auth,
            template,
        })
    }

    pub async fn send(&self, client: &Client, msg: &Message) -> Result<(), NotifyError> {
        let mut req = client.post(self.target.clone());
        if let Some((user, password)) = &self.basic_auth {
            req = req.basic_auth(user, Some(password.expose()));
        }
        req = match &self.template {
            Template::Json {
                title_key,
                message_key,
            } => {
                let payload = BTreeMap::from([
                    (title_key.as_str(), msg.title.as_str()),
                    (message_key.as_str(), msg.body.as_str()),
                    (LEVEL_KEY, msg.level.as_str()),
                ]);
                req.json(&payload)
            }
            Template::Plain => req
                .header(reqwest::header::CONTENT_TYPE, "text/plain; charset=utf-8")
                .body(format!("{}\n\n{}", msg.title, msg.body)),
        };
        let resp = req.send().await.map_err(transport)?;
        check_status(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::tests::{msg, parse};
    use crate::notify::{Level, Notify};
    use crate::testutil::http_mock::{MockServer, Response};

    fn generic(url: &str) -> Generic {
        Generic::parse(&Url::parse(url).unwrap()).unwrap()
    }

    #[test]
    fn parses_url() {
        let g = generic("generic://hooks.example.test/api/hook");
        assert_eq!(g.target.as_str(), "https://hooks.example.test/api/hook");
        assert!(g.basic_auth.is_none());
        assert!(
            matches!(&g.template, Template::Json { title_key, message_key } if title_key == "title" && message_key == "message")
        );

        let g = generic(
            "generic://u:p%40ss@hooks.example.test:8443/h%20x?disabletls=yes&token=a%2Bb&template=plain&x=1",
        );
        assert_eq!(
            g.target.as_str(),
            "http://hooks.example.test:8443/h%20x?token=a%2Bb&x=1"
        );
        assert!(matches!(&g.basic_auth, Some((u, p)) if u == "u" && p.expose() == "p@ss"));
        assert!(matches!(g.template, Template::Plain));
    }

    #[test]
    fn rejects_invalid() {
        for url in [
            "generic:///path",
            "generic://host/?template=xml",
            "generic://host/?disabletls=maybe",
            "generic://host/?titlekey=a&messagekey=a",
            "generic://host/?titlekey=level",
            "generic://host/?messagekey=",
        ] {
            assert!(
                matches!(parse(url), Err(NotifyError::InvalidUrl(_))),
                "{url}"
            );
        }
    }

    fn url_for(server: &MockServer, rest: &str) -> String {
        let b = server.base();
        format!(
            "generic://{}:{}{rest}",
            b.host_str().unwrap(),
            b.port().unwrap()
        )
    }

    #[tokio::test]
    async fn posts_json_with_custom_keys() {
        let server = MockServer::start(|_| Response::ok("")).await;
        let url = url_for(
            &server,
            "/hook?disabletls=yes&titlekey=subject&messagekey=text&token=abc",
        );
        parse(&url).unwrap().send(&msg(Level::Error)).await.unwrap();
        let reqs = server.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].method, "POST");
        assert_eq!(reqs[0].path(), "/hook");
        assert_eq!(reqs[0].query("token").as_deref(), Some("abc"));
        assert_eq!(reqs[0].header("content-type"), "application/json");
        assert_eq!(
            reqs[0].body,
            r#"{"level":"error","subject":"betbot alice: Spieltag 12 – 2 Tipps abgegeben","text":"Spieltag 12\n\nFC Nordstadt - SV Südhafen  2:1"}"#
        );
    }

    #[tokio::test]
    async fn posts_plain_text_with_basic_auth() {
        let server = MockServer::start(|_| Response::status(500)).await;
        let b = server.base();
        let url = format!(
            "generic://bob:pw@{}:{}/?disabletls=yes&template=plain",
            b.host_str().unwrap(),
            b.port().unwrap()
        );
        let err = parse(&url)
            .unwrap()
            .send(&msg(Level::Info))
            .await
            .unwrap_err();
        assert!(matches!(err, NotifyError::Rejected(500)), "{err:?}");
        let r = &server.requests()[0];
        assert_eq!(r.header("content-type"), "text/plain; charset=utf-8");
        assert_eq!(r.header("authorization"), "Basic Ym9iOnB3");
        assert_eq!(
            r.body,
            "betbot alice: Spieltag 12 – 2 Tipps abgegeben\n\nSpieltag 12\n\nFC Nordstadt - SV Südhafen  2:1"
        );
    }
}
