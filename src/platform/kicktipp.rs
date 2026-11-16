//! kicktipp.de platform: logs in with the form login, reads matches, tips and
//! odds from the tip page (`/<community>/tippabgabe`) and submits the tip form.

#[cfg(test)]
mod mock;
mod parse;
#[cfg(test)]
mod testutil;

use std::sync::Arc;
use std::time::Duration;

use reqwest::redirect::Policy;
use reqwest::{Client, RequestBuilder, StatusCode, Url};
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use super::{HistoryScope, PlatformError, TipPlatform};
use crate::credentials::Credentials;
use crate::domain::{Match, OddsMap, OpenMatch, PlacedTip, TipSubmission};
use parse::{ParseError, TipPage};

const BASE_URL: &str = "https://www.kicktipp.de";
const LOGIN_PATH: &str = "/info/profil/login";
/// Minimum time between two requests, to be polite to kicktipp.
const REQUEST_GAP: Duration = Duration::from_millis(500);
const USER_AGENT: &str = concat!("betbot/", env!("CARGO_PKG_VERSION"));
const TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REDIRECTS: usize = 5;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KicktippConfig {
    /// Community (Tipprunde) slug as in `https://www.kicktipp.de/<community>/`.
    pub community: String,
}

/// Enforces a minimum gap between consecutive requests.
struct Throttle {
    gap: Duration,
    last: Mutex<Option<Instant>>,
}

impl Throttle {
    fn new(gap: Duration) -> Self {
        Self {
            gap,
            last: Mutex::new(None),
        }
    }

    async fn wait(&self) {
        let mut last = self.last.lock().await;
        if let Some(t) = *last {
            tokio::time::sleep_until(t + self.gap).await;
        }
        *last = Some(Instant::now());
    }
}

pub struct KicktippPlatform {
    community: String,
    credentials: Credentials,
    base: Url,
    client: Client,
    throttle: Throttle,
    /// Cached current tip page (the one the bot tips on).
    page: Mutex<Option<Arc<TipPage>>>,
}

fn transport(e: reqwest::Error) -> PlatformError {
    PlatformError::Transport(e.without_url().to_string())
}

impl KicktippPlatform {
    pub fn new(cfg: &KicktippConfig, credentials: Credentials) -> Result<Self, PlatformError> {
        let base = Url::parse(BASE_URL)
            .map_err(|e| PlatformError::Transport(format!("invalid base URL: {e}")))?;
        Self::with_base(cfg, credentials, base, REQUEST_GAP)
    }

    /// Production always uses `https://www.kicktipp.de`; tests point this at a local mock.
    fn with_base(
        cfg: &KicktippConfig,
        credentials: Credentials,
        base: Url,
        gap: Duration,
    ) -> Result<Self, PlatformError> {
        // Err only means a provider is already installed (e.g. a second tenant).
        let _ = rustls::crypto::ring::default_provider().install_default();
        let origin = base.origin();
        let redirect = Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS || attempt.url().origin() != origin {
                attempt.stop()
            } else {
                attempt.follow()
            }
        });
        let client = Client::builder()
            .timeout(TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(USER_AGENT)
            .cookie_store(true)
            .redirect(redirect)
            .build()
            .map_err(transport)?;
        Ok(Self {
            community: cfg.community.clone(),
            credentials,
            base,
            client,
            throttle: Throttle::new(gap),
            page: Mutex::new(None),
        })
    }

    /// Resolves a form action against the base URL; refuses other origins.
    fn same_origin(&self, target: &str, what: &str) -> Result<Url, PlatformError> {
        let url = self
            .base
            .join(target)
            .map_err(|_| PlatformError::UnexpectedPage(format!("{what} has an invalid action")))?;
        if url.origin() != self.base.origin() {
            return Err(PlatformError::UnexpectedPage(format!(
                "{what} posts to a foreign host"
            )));
        }
        Ok(url)
    }

    fn tip_page_url(&self) -> Result<Url, PlatformError> {
        self.base
            .join(&format!("/{}/tippabgabe", self.community))
            .map_err(|_| PlatformError::Transport("invalid tip page URL".into()))
    }

    /// Sends a request and returns the body. Errors carry the path only, never
    /// the query or the body.
    async fn send(&self, req: RequestBuilder) -> Result<String, PlatformError> {
        let req = req.build().map_err(transport)?;
        let path = req.url().path().to_owned();
        self.throttle.wait().await;
        let resp = self.client.execute(req).await.map_err(transport)?;
        let status = resp.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(PlatformError::Auth(format!("HTTP {status} from {path}")));
        }
        if !status.is_success() {
            return Err(PlatformError::Transport(format!(
                "HTTP {status} from {path}"
            )));
        }
        resp.text().await.map_err(transport)
    }

    async fn fetch_page(&self, url: Url) -> Result<TipPage, PlatformError> {
        let html = self.send(self.client.get(url)).await?;
        Ok(parse::parse_tip_page(&html)?)
    }

    async fn fetch_matchday(
        &self,
        matchday: u32,
        saison: Option<&str>,
    ) -> Result<TipPage, PlatformError> {
        let mut url = self.tip_page_url()?;
        {
            let mut q = url.query_pairs_mut();
            if let Some(s) = saison {
                q.append_pair("tippsaisonId", s);
            }
            q.append_pair("spieltagIndex", &matchday.to_string());
        }
        let page = self.fetch_page(url).await?;
        if page.matchday != matchday {
            return Err(PlatformError::UnexpectedPage(format!(
                "requested matchday {matchday}, got {}",
                page.matchday
            )));
        }
        Ok(page)
    }

    /// The parameterless tip page may show a running matchday whose rows are
    /// all closed already; then the next matchday is the one to tip (one
    /// step only). The season id always comes from the page. A page with zero
    /// rows never reaches here (parse error).
    async fn resolve_current(&self, page: TipPage) -> Result<TipPage, PlatformError> {
        if page.has_open_rows() || page.matchday >= page.matchday_count {
            return Ok(page);
        }
        debug!(
            matchday = page.matchday,
            "default tip page has no open matches, using the next matchday"
        );
        self.fetch_matchday(page.matchday + 1, page.hidden("tippsaisonId"))
            .await
    }

    async fn current_page(&self) -> Result<Arc<TipPage>, PlatformError> {
        let mut cached = self.page.lock().await;
        if let Some(p) = cached.as_ref() {
            return Ok(Arc::clone(p));
        }
        let page = self.fetch_page(self.tip_page_url()?).await?;
        let page = Arc::new(self.resolve_current(page).await?);
        *cached = Some(Arc::clone(&page));
        Ok(page)
    }

    async fn invalidate(&self) {
        *self.page.lock().await = None;
    }
}

impl TipPlatform for KicktippPlatform {
    async fn login(&self) -> Result<(), PlatformError> {
        self.invalidate().await;
        let login_url = self.same_origin(LOGIN_PATH, "login page")?;
        let html = self.send(self.client.get(login_url)).await?;
        let form = parse::parse_login_form(&html)?;
        let action =
            self.same_origin(form.action.as_deref().unwrap_or(LOGIN_PATH), "login form")?;
        let mut fields = form.hidden;
        fields.push((
            "kennung".into(),
            self.credentials.username().expose().into(),
        ));
        fields.push((
            "passwort".into(),
            self.credentials.password().expose().into(),
        ));
        // The response (usually a redirect target) is not needed; success is
        // confirmed by fetching the tip page with the new session.
        self.send(self.client.post(action).form(&fields)).await?;

        let html = self.send(self.client.get(self.tip_page_url()?)).await?;
        let page = match parse::parse_tip_page(&html) {
            Ok(p) => p,
            Err(ParseError::LoginRequired) => {
                return Err(PlatformError::Auth(
                    "login rejected (wrong credentials or no access to the community)".into(),
                ));
            }
            Err(e) => return Err(e.into()),
        };
        let page = self.resolve_current(page).await?;
        *self.page.lock().await = Some(Arc::new(page));
        info!(community = %self.community, "logged in to kicktipp");
        Ok(())
    }

    async fn open_matches(&self) -> Result<Vec<OpenMatch>, PlatformError> {
        let page = self.current_page().await?;
        debug!(
            matchday = page.matchday,
            matchdays = page.matchday_count,
            "kicktipp tip page"
        );
        if let Some(l) = page.same_result_limit {
            info!(
                "kicktipp reports a same-result limit of {}/{} per matchday",
                l.max, l.of
            );
        }
        Ok(page.open_matches(&self.community))
    }

    /// Tips of the wanted matchdays: for `Season` matchdays 1 up to the
    /// current one, for `Matchdays` exactly those. The current matchday
    /// always comes from the cached tip page (incl. started rows of a mixed
    /// matchday) without an extra request. Limitation: for `Season` tips
    /// already placed on later matchdays are not read.
    async fn placed_tips(&self, scope: &HistoryScope) -> Result<Vec<PlacedTip>, PlatformError> {
        let current = self.current_page().await?;
        let saison = current.hidden("tippsaisonId");
        let wanted: Vec<u32> = match scope {
            HistoryScope::Season => (1..current.matchday).collect(),
            HistoryScope::Matchdays(v) => {
                let mut v: Vec<u32> = v
                    .iter()
                    .copied()
                    .filter(|&md| md != current.matchday)
                    .collect();
                v.sort_unstable();
                v.dedup();
                v
            }
        };
        let mut tips = Vec::new();
        for md in wanted {
            tips.extend(self.fetch_matchday(md, saison).await?.placed_tips());
        }
        tips.extend(current.placed_tips());
        Ok(tips)
    }

    async fn odds(&self, matches: &[Match]) -> Result<OddsMap, PlatformError> {
        Ok(self.current_page().await?.odds_for(matches))
    }

    async fn submit_tips(&self, tips: &[TipSubmission]) -> Result<(), PlatformError> {
        if tips.is_empty() {
            return Ok(());
        }
        self.invalidate().await;
        let page = self.current_page().await?;
        let submission = parse::build_submission(&page, tips)?;
        for id in &submission.skipped {
            warn!(match_id = %id, "match no longer open, not submitted");
        }
        let action = self.same_origin(&page.form.action, "tip form")?;
        self.send(self.client.post(action).form(&submission.fields))
            .await?;

        self.invalidate().await;
        let after = self
            .fetch_matchday(page.matchday, page.hidden("tippsaisonId"))
            .await?;
        parse::verify_stored(&page, &after, &submission.applied)?;
        *self.page.lock().await = Some(Arc::new(after));
        info!(count = submission.applied.len(), "kicktipp stored the tips");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    use super::mock::{MockServer, Request, Response};
    use super::testutil::*;
    use super::*;
    use crate::domain::{MatchId, Tip};

    fn platform_for(server: &MockServer) -> KicktippPlatform {
        let creds = Credentials::from_parts("test-user".into(), "hunter2".into()).unwrap();
        KicktippPlatform::with_base(
            &KicktippConfig {
                community: "testrunde".into(),
            },
            creds,
            server.base(),
            Duration::ZERO,
        )
        .unwrap()
    }

    fn is_tip_page_get(r: &Request) -> bool {
        r.method == "GET" && r.path() == "/testrunde/tippabgabe"
    }

    fn sub(id: &str, home: u8, away: u8) -> TipSubmission {
        TipSubmission {
            match_id: MatchId::new(id),
            tip: Tip { home, away },
        }
    }

    fn login_handler(r: &Request) -> Response {
        match (r.method.as_str(), r.path()) {
            ("GET", "/info/profil/login") => Response::ok(login_page()),
            ("POST", "/info/profil/loginaction") => Response::redirect("/testrunde/tippabgabe")
                .with_header("Set-Cookie", "login=x; Path=/"),
            ("GET", "/testrunde/tippabgabe") if r.header("cookie").contains("login=x") => {
                Response::ok(open_md10())
            }
            ("GET", "/testrunde/tippabgabe") => Response::ok(login_page()),
            _ => Response::not_found(),
        }
    }

    #[tokio::test]
    async fn login_then_open_matches_and_odds_use_one_tip_page_request() {
        let server = MockServer::start(login_handler).await;
        let p = platform_for(&server);
        p.login().await.unwrap();
        let tip_gets_after_login = server
            .requests()
            .iter()
            .filter(|r| is_tip_page_get(r))
            .count();

        let open = p.open_matches().await.unwrap();
        assert_eq!(open.len(), 9);
        assert_eq!(open[0].fixture.id, MatchId::new("1644299506"));
        assert_eq!(open[0].fixture.competition, "testrunde");
        let fixtures: Vec<Match> = open.iter().map(|m| m.fixture.clone()).collect();
        let odds = p.odds(&fixtures[..1]).await.unwrap();
        let o = &odds[&MatchId::new("1644299506")];
        assert_eq!((o.home, o.draw, o.away), (1.10, 12.0, 21.0));

        let reqs = server.requests();
        // One GET follows the login redirect, one confirms the session; the
        // cached page serves open_matches and odds without further requests.
        assert_eq!(tip_gets_after_login, 2);
        assert_eq!(reqs.iter().filter(|r| is_tip_page_get(r)).count(), 2);
        let posts: Vec<&Request> = reqs.iter().filter(|r| r.method == "POST").collect();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].target, "/info/profil/loginaction");
        assert_eq!(
            posts[0].form(),
            vec![
                ("kennung".to_owned(), "test-user".to_owned()),
                ("passwort".to_owned(), "hunter2".to_owned())
            ]
        );
        for r in &reqs {
            assert!(r.header("user-agent").starts_with("betbot/"), "{r:?}");
            assert_ne!(r.target, "/", "unexpected request {r:?}");
        }
        assert!(
            reqs.iter()
                .filter(|r| is_tip_page_get(r))
                .all(|r| r.header("cookie").contains("login=x"))
        );
    }

    #[tokio::test]
    async fn wrong_password_is_auth_error() {
        let server = MockServer::start(|r: &Request| match (r.method.as_str(), r.path()) {
            ("GET", "/info/profil/login") | ("POST", "/info/profil/loginaction") => {
                Response::ok(login_page())
            }
            ("GET", "/testrunde/tippabgabe") => Response::ok(login_page()),
            _ => Response::not_found(),
        })
        .await;
        let err = platform_for(&server).login().await.unwrap_err();
        assert!(matches!(err, PlatformError::Auth(_)), "{err:?}");
        for s in [err.to_string(), format!("{err:?}")] {
            assert!(!s.contains("hunter2"), "{s}");
            assert!(!s.contains("test-user"), "{s}");
        }
    }

    #[tokio::test]
    async fn open_matches_after_expired_session_is_auth() {
        let server = MockServer::start(|_: &Request| Response::ok(login_page())).await;
        let err = platform_for(&server).open_matches().await.unwrap_err();
        assert!(matches!(err, PlatformError::Auth(_)), "{err:?}");
    }

    #[tokio::test]
    async fn server_error_is_transport_without_query() {
        let server = MockServer::start(|_: &Request| Response::status(500)).await;
        let p = platform_for(&server);
        for err in [
            p.open_matches().await.unwrap_err(),
            p.fetch_matchday(1, Some("1000001")).await.unwrap_err(),
            p.login().await.unwrap_err(),
        ] {
            let PlatformError::Transport(m) = &err else {
                panic!("expected transport error, got {err:?}");
            };
            assert!(m.contains("500"), "{m}");
            assert!(!m.contains('?'), "{m}");
            assert!(!m.contains("hunter2") && !m.contains("test-user"), "{m}");
        }
    }

    fn season_handler(wrong: bool) -> impl Fn(&Request) -> Response + Send + Sync + 'static {
        move |r: &Request| {
            if !is_tip_page_get(r) {
                return Response::not_found();
            }
            match r.query("spieltagIndex").as_deref() {
                None => Response::ok(as_matchday(open_md10(), 10, 3)),
                Some(_) if wrong => Response::ok(md9()),
                Some(n @ ("1" | "2")) => Response::ok(as_matchday(md9(), 9, n.parse().unwrap())),
                Some(_) => Response::not_found(),
            }
        }
    }

    #[tokio::test]
    async fn season_tips_reads_all_matchdays() {
        let server = MockServer::start(season_handler(false)).await;
        let tips = platform_for(&server)
            .placed_tips(&HistoryScope::Season)
            .await
            .unwrap();
        let ids: Vec<String> = tips.iter().map(|t| t.match_id.to_string()).collect();
        let expected: Vec<String> = (1..=2)
            .flat_map(|md| (0..9).map(move |i| format!("md{md}-row{i}")))
            .collect();
        assert_eq!(ids, expected);
        assert!(tips[..9].iter().all(|t| t.matchday == 1));
        assert!(tips[9..].iter().all(|t| t.matchday == 2));
        let targets: Vec<String> = server.requests().iter().map(|r| r.target.clone()).collect();
        assert!(
            targets
                .iter()
                .any(|t| t.ends_with("?tippsaisonId=1000001&spieltagIndex=1")),
            "{targets:?}"
        );
        assert_eq!(targets.len(), 3, "{targets:?}");

        let server = MockServer::start(season_handler(true)).await;
        let err = platform_for(&server)
            .placed_tips(&HistoryScope::Season)
            .await
            .unwrap_err();
        assert!(matches!(err, PlatformError::UnexpectedPage(_)), "{err:?}");
    }

    fn tip_tuples(tips: &[PlacedTip]) -> Vec<(String, u32, Tip)> {
        tips.iter()
            .map(|t| (t.match_id.to_string(), t.matchday, t.tip))
            .collect()
    }

    #[tokio::test]
    async fn matchday_scope_needs_no_extra_request() {
        let server = MockServer::start(season_handler(false)).await;
        let p = platform_for(&server);
        let tips = p
            .placed_tips(&HistoryScope::Matchdays(vec![3]))
            .await
            .unwrap();
        assert_eq!(server.requests().len(), 1);
        let current = p.current_page().await.unwrap();
        assert_eq!(tip_tuples(&tips), tip_tuples(&current.placed_tips()));

        // Another matchday costs exactly one GET; the current page stays cached.
        let tips = p
            .placed_tips(&HistoryScope::Matchdays(vec![1, 3, 1]))
            .await
            .unwrap();
        let ids: Vec<String> = tips.iter().map(|t| t.match_id.to_string()).collect();
        let expected: Vec<String> = (0..9).map(|i| format!("md1-row{i}")).collect();
        assert_eq!(ids, expected);
        let reqs = server.requests();
        assert_eq!(reqs.len(), 2, "{reqs:?}");
        assert_eq!(reqs[1].query("spieltagIndex").as_deref(), Some("1"));
        assert_eq!(reqs[1].query("tippsaisonId").as_deref(), Some("1000001"));
    }

    #[tokio::test]
    async fn matchday_scope_of_mixed_matchday_returns_started_rows() {
        let server = MockServer::start(|r: &Request| {
            if is_tip_page_get(r) && r.query("spieltagIndex").is_none() {
                Response::ok(mixed_md10())
            } else {
                Response::not_found()
            }
        })
        .await;
        let tips = platform_for(&server)
            .placed_tips(&HistoryScope::Matchdays(vec![10]))
            .await
            .unwrap();
        assert_eq!(
            tip_tuples(&tips),
            [
                ("md10-row0".to_owned(), 10, Tip { home: 2, away: 0 }),
                ("md10-row1".to_owned(), 10, Tip { home: 1, away: 2 }),
            ]
        );
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn closed_default_page_moves_to_next_matchday() {
        let server = MockServer::start(|r: &Request| {
            if !is_tip_page_get(r) {
                return Response::not_found();
            }
            match r.query("spieltagIndex").as_deref() {
                None => Response::ok(md9()),
                Some("10") if r.query("tippsaisonId").as_deref() == Some("1000001") => {
                    Response::ok(open_md10())
                }
                Some(_) => Response::not_found(),
            }
        })
        .await;
        let p = platform_for(&server);
        let open = p.open_matches().await.unwrap();
        assert_eq!(open.len(), 9);
        assert!(open.iter().all(|m| m.fixture.matchday == 10));
        assert_eq!(server.requests().len(), 2);
        // The resolved page is cached.
        p.open_matches().await.unwrap();
        assert_eq!(server.requests().len(), 2);

        let server = MockServer::start(|r: &Request| {
            if is_tip_page_get(r) {
                Response::ok(open_md10())
            } else {
                Response::not_found()
            }
        })
        .await;
        assert_eq!(platform_for(&server).open_matches().await.unwrap().len(), 9);
        assert_eq!(server.requests().len(), 1);
    }

    /// Mock of the tip page that stores POSTed tips. Before the first POST
    /// it serves `initial`; afterwards `template` prefilled with the state.
    struct TipMock {
        initial: String,
        template: String,
        apply_posts: bool,
        state: StdMutex<Option<HashMap<String, (String, String)>>>,
    }

    impl TipMock {
        fn start(
            initial: String,
            template: String,
            apply_posts: bool,
        ) -> impl Future<Output = MockServer> {
            let mock = Arc::new(Self {
                initial,
                template,
                apply_posts,
                state: StdMutex::new(None),
            });
            MockServer::start(move |r: &Request| mock.handle(r))
        }

        fn handle(&self, r: &Request) -> Response {
            if r.path() != "/testrunde/tippabgabe" {
                return Response::not_found();
            }
            let mut state = self.state.lock().unwrap();
            if r.method == "POST" {
                if self.apply_posts {
                    let mut s = HashMap::new();
                    for (k, v) in r.form() {
                        let Some(rest) = k.strip_prefix("spieltippForms[") else {
                            continue;
                        };
                        let Some((id, field)) = rest.split_once("].") else {
                            continue;
                        };
                        let e: &mut (String, String) = s.entry(id.to_owned()).or_default();
                        match field {
                            "heimTipp" => e.0 = v,
                            "gastTipp" => e.1 = v,
                            _ => {}
                        }
                    }
                    *state = Some(s);
                }
                return Response::redirect("/testrunde/tippabgabe");
            }
            match state.as_ref() {
                None => Response::ok(self.initial.clone()),
                Some(s) => {
                    let mut html = self.template.clone();
                    for (id, (h, a)) in s {
                        if !h.is_empty() || !a.is_empty() {
                            html = prefill(&html, id, h, a);
                        }
                    }
                    Response::ok(html)
                }
            }
        }
    }

    fn posts(server: &MockServer) -> Vec<Request> {
        server
            .requests()
            .into_iter()
            .filter(|r| r.method == "POST")
            .collect()
    }

    #[tokio::test]
    async fn submit_tips_happy_path_and_verification() {
        let server = TipMock::start(open_md10().into(), open_md10().into(), true).await;
        let p = platform_for(&server);
        p.submit_tips(&[sub("1644299506", 2, 1)]).await.unwrap();

        let reqs = server.requests();
        let methods: Vec<&str> = reqs.iter().map(|r| r.method.as_str()).collect();
        // GET page, POST form, GET (redirect target), GET verification page.
        assert_eq!(methods.first(), Some(&"GET"));
        assert_eq!(methods.last(), Some(&"GET"));
        let posts = posts(&server);
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].target, "/testrunde/tippabgabe");
        assert!(
            posts[0]
                .body
                .contains("spieltippForms%5B1644299506%5D.heimTipp=2"),
            "{}",
            posts[0].body
        );
        assert!(
            posts[0].body.ends_with("submitbutton="),
            "{}",
            posts[0].body
        );
        let open = p.open_matches().await.unwrap();
        assert_eq!(open[0].tip, Some(Tip { home: 2, away: 1 }));
    }

    #[tokio::test]
    async fn submit_tips_keeps_manual_tip() {
        let server = TipMock::start(saved_md10().into(), open_md10().into(), true).await;
        let p = platform_for(&server);
        p.submit_tips(&[sub("1644299505", 0, 0)]).await.unwrap();
        let form = posts(&server)[0].form();
        let get = |k: &str| {
            form.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.as_str())
                .unwrap()
        };
        assert_eq!(get("spieltippForms[1644299506].heimTipp"), "2");
        assert_eq!(get("spieltippForms[1644299506].gastTipp"), "0");
        assert_eq!(get("spieltippForms[1644299500].heimTipp"), "1");
        assert_eq!(get("spieltippForms[1644299500].gastTipp"), "2");
        assert_eq!(get("spieltippForms[1644299505].heimTipp"), "0");
        assert_eq!(get("spieltippForms[1644299505].gastTipp"), "0");
        assert_eq!(get("spieltippForms[1644299507].heimTipp"), "");
    }

    #[tokio::test]
    async fn submit_tips_detects_ignored_post() {
        let server = TipMock::start(open_md10().into(), open_md10().into(), false).await;
        let err = platform_for(&server)
            .submit_tips(&[sub("1644299506", 2, 1)])
            .await
            .unwrap_err();
        let PlatformError::Rejected(m) = &err else {
            panic!("expected rejection, got {err:?}");
        };
        assert!(m.contains("1644299506"), "{m}");
        assert_eq!(posts(&server).len(), 1);
    }

    #[tokio::test]
    async fn submit_tips_never_posts_closed_match() {
        let server = TipMock::start(mixed_md10(), mixed_md10(), true).await;
        let p = platform_for(&server);
        p.submit_tips(&[sub("1644299506", 3, 0), sub("1644299505", 0, 0)])
            .await
            .unwrap();
        let sent = posts(&server);
        assert_eq!(sent.len(), 1);
        assert!(!sent[0].body.contains("1644299506"), "{}", sent[0].body);
        assert!(!sent[0].body.contains("1644299500"), "{}", sent[0].body);
        assert!(sent[0].body.contains("1644299505"));

        let server = TipMock::start(mixed_md10(), mixed_md10(), true).await;
        let err = platform_for(&server)
            .submit_tips(&[sub("1644299506", 3, 0)])
            .await
            .unwrap_err();
        assert!(matches!(err, PlatformError::Rejected(_)), "{err:?}");
        assert!(posts(&server).is_empty());
    }

    #[tokio::test]
    async fn submit_empty_is_noop() {
        let server = MockServer::start(|_: &Request| Response::not_found()).await;
        platform_for(&server).submit_tips(&[]).await.unwrap();
        assert!(server.requests().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn throttle_enforces_gap() {
        let t = Throttle::new(REQUEST_GAP);
        let start = Instant::now();
        t.wait().await;
        assert_eq!(start.elapsed(), Duration::ZERO);
        t.wait().await;
        let second = start.elapsed();
        assert!(second >= REQUEST_GAP, "{second:?}");
        t.wait().await;
        assert!(start.elapsed() - second >= REQUEST_GAP);
    }

    #[test]
    fn client_builds_with_rustls_ring() {
        let creds = Credentials::from_parts("test-user".into(), "hunter2".into()).unwrap();
        let p = KicktippPlatform::new(
            &KicktippConfig {
                community: "testrunde".into(),
            },
            creds,
        );
        assert!(p.is_ok());
    }
}
