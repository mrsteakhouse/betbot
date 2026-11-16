//! ntfy: `ntfy://[[user]:password@]host[:port]/topic[?scheme=http&priority=3&tags=a,b]`.
//! Publishes as JSON to the server root (JSON instead of headers, because
//! header values cannot carry umlauts). `user:password@` = Basic auth,
//! `:token@` = access token (Bearer).

use reqwest::{Client, RequestBuilder, Url};
use serde::Serialize;

use super::{Level, Message, NotifyError, Params, check_status, decode, invalid, transport};
use crate::credentials::Secret;

const KNOWN: &[&str] = &["scheme", "priority", "tags"];

enum Auth {
    None,
    Basic { user: String, password: Secret },
    Bearer(Secret),
}

pub struct Ntfy {
    endpoint: Url,
    topic: String,
    auth: Auth,
    priority: Option<u8>,
    tags: Vec<String>,
}

#[derive(Serialize)]
struct Publish<'a> {
    topic: &'a str,
    title: &'a str,
    message: &'a str,
    priority: u8,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    tags: &'a [String],
}

fn priority(level: Level) -> u8 {
    match level {
        Level::Info => 3,
        Level::Warning => 4,
        Level::Error => 5,
    }
}

impl Ntfy {
    pub fn parse(url: &Url) -> Result<Self, NotifyError> {
        let params = Params::of(url);
        params.reject_unknown("ntfy", KNOWN)?;
        let scheme = match params.get("scheme").map(str::to_ascii_lowercase).as_deref() {
            None | Some("https") => "https",
            Some("http") => "http",
            Some(_) => return Err(invalid("ntfy: scheme must be http or https")),
        };
        let host = url.host_str().unwrap_or_default();
        if host.is_empty() {
            return Err(invalid("ntfy: host missing"));
        }
        let mut endpoint = Url::parse(&format!("{scheme}://{host}/"))
            .map_err(|e| invalid(format!("ntfy: {e}")))?;
        endpoint
            .set_port(url.port())
            .map_err(|()| invalid("ntfy: invalid port"))?;
        let topic = decode(url.path().trim_start_matches('/'))?;
        if topic.is_empty() || topic.contains('/') {
            return Err(invalid(
                "ntfy: exactly one topic required (ntfy://host/topic)",
            ));
        }
        let user = decode(url.username())?;
        let password = url.password().map(decode).transpose()?.unwrap_or_default();
        let auth = match (user.is_empty(), password.is_empty()) {
            (true, true) => Auth::None,
            (true, false) => Auth::Bearer(Secret::new(password)),
            (false, false) => Auth::Basic {
                user,
                password: Secret::new(password),
            },
            (false, true) => return Err(invalid("ntfy: password missing for user")),
        };
        let priority = match params.get("priority") {
            None => None,
            Some(p) => match p.parse::<u8>() {
                Ok(p @ 1..=5) => Some(p),
                _ => return Err(invalid("ntfy: priority must be 1..=5")),
            },
        };
        let tags = params
            .get("tags")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
            .collect();
        Ok(Self {
            endpoint,
            topic,
            auth,
            priority,
            tags,
        })
    }

    fn authorize(&self, req: RequestBuilder) -> RequestBuilder {
        match &self.auth {
            Auth::None => req,
            Auth::Basic { user, password } => req.basic_auth(user, Some(password.expose())),
            Auth::Bearer(token) => req.bearer_auth(token.expose()),
        }
    }

    pub async fn send(&self, client: &Client, msg: &Message) -> Result<(), NotifyError> {
        let payload = Publish {
            topic: &self.topic,
            title: &msg.title,
            message: &msg.body,
            priority: self.priority.unwrap_or_else(|| priority(msg.level)),
            tags: &self.tags,
        };
        let resp = self
            .authorize(client.post(self.endpoint.clone()))
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
    use crate::notify::Notify;
    use crate::notify::tests::{msg, parse};
    use crate::testutil::http_mock::{MockServer, Response};

    fn ntfy(url: &str) -> Ntfy {
        Ntfy::parse(&Url::parse(url).unwrap()).unwrap()
    }

    #[test]
    fn parses_url() {
        let n = ntfy("ntfy://ntfy.sh/betbot-alerts");
        assert_eq!(n.endpoint.as_str(), "https://ntfy.sh/");
        assert_eq!(n.topic, "betbot-alerts");
        assert!(matches!(n.auth, Auth::None));
        assert_eq!(n.priority, None);
        assert!(n.tags.is_empty());

        let n = ntfy(
            "ntfy://:tk_abc@ntfy.example.test:8080/t?scheme=http&priority=2&tags=soccer,%20bot",
        );
        assert_eq!(n.endpoint.as_str(), "http://ntfy.example.test:8080/");
        assert!(matches!(&n.auth, Auth::Bearer(t) if t.expose() == "tk_abc"));
        assert_eq!(n.priority, Some(2));
        assert_eq!(n.tags, ["soccer", "bot"]);

        let n = ntfy("ntfy://al%40ice:p%3Aw@ntfy.example.test/t");
        assert!(
            matches!(&n.auth, Auth::Basic { user, password } if user == "al@ice" && password.expose() == "p:w")
        );
    }

    #[test]
    fn rejects_invalid() {
        for url in [
            "ntfy:///topic",
            "ntfy://host",
            "ntfy://host/",
            "ntfy://host/a/b",
            "ntfy://user@host/t",
            "ntfy://host/t?priority=0",
            "ntfy://host/t?priority=high",
            "ntfy://host/t?scheme=ftp",
            "ntfy://host/t?prio=3",
        ] {
            assert!(
                matches!(parse(url), Err(NotifyError::InvalidUrl(_))),
                "{url}"
            );
        }
    }

    #[tokio::test]
    async fn publishes_json() {
        let server = MockServer::start(|_| Response::ok("{}")).await;
        let addr = server.base();
        let url = format!(
            "ntfy://alice:pw@{}:{}/betbot?scheme=http&tags=soccer",
            addr.host_str().unwrap(),
            addr.port().unwrap()
        );
        parse(&url)
            .unwrap()
            .send(&msg(Level::Warning))
            .await
            .unwrap();
        let reqs = server.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].method, "POST");
        assert_eq!(reqs[0].path(), "/");
        assert_eq!(reqs[0].header("content-type"), "application/json");
        // base64("alice:pw")
        assert_eq!(reqs[0].header("authorization"), "Basic YWxpY2U6cHc=");
        assert_eq!(
            reqs[0].body,
            r#"{"topic":"betbot","title":"betbot alice: Spieltag 12 – 2 Tipps abgegeben","message":"Spieltag 12\n\nFC Nordstadt - SV Südhafen  2:1","priority":4,"tags":["soccer"]}"#
        );
    }

    #[tokio::test]
    async fn bearer_token_and_fixed_priority() {
        let server = MockServer::start(|_| Response::status(403)).await;
        let addr = server.base();
        let url = format!(
            "ntfy://:tk_secret@{}:{}/t?scheme=http&priority=1",
            addr.host_str().unwrap(),
            addr.port().unwrap()
        );
        let err = parse(&url)
            .unwrap()
            .send(&msg(Level::Error))
            .await
            .unwrap_err();
        assert!(matches!(err, NotifyError::Rejected(403)), "{err:?}");
        let r = &server.requests()[0];
        assert_eq!(r.header("authorization"), "Bearer tk_secret");
        assert!(r.body.contains(r#""priority":1"#), "{}", r.body);
        assert!(!r.body.contains("tags"), "{}", r.body);
    }
}
