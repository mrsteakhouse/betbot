//! Orchestrates one run: per tenant login, fetch matches/history/odds, rank,
//! apply rules and submit.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use thiserror::Error;
use tracing::{Instrument, error, info, info_span, warn};

use crate::algorithm::{self, TipAlgorithm};
use crate::config::{Config, TenantConfig};
use crate::credentials::{self, CredentialsError};
use crate::domain::{Match, MatchId, OpenMatch, PlacedTip, ScoringScheme, TipSubmission};
use crate::odds::{OddsError, OddsProvider, OddsSource};
use crate::platform::{Platform, PlatformError, TipPlatform};
use crate::rules::{MatchChoice, Rules};

#[derive(Debug, Clone, Copy)]
pub struct RunOptions {
    pub dry_run: bool,
    pub overwrite_existing: bool,
}

#[derive(Debug, Clone)]
pub struct TenantReport {
    pub matches_open: usize,
    /// Tips submitted (or, in a dry run, tips that would have been submitted).
    pub submitted: usize,
    pub skipped: usize,
    pub violations: usize,
    pub dry_run: bool,
}

#[derive(Debug, Error)]
pub enum TenantError {
    #[error("credentials: {0}")]
    Credentials(#[from] CredentialsError),
    #[error("platform: {0}")]
    Platform(#[from] PlatformError),
    #[error("odds: {0}")]
    Odds(#[from] OddsError),
    #[error("timed out after {0}s")]
    Timeout(u64),
    #[error("tenant task panicked")]
    Panicked,
}

#[derive(Debug)]
pub struct TenantOutcome {
    pub name: String,
    pub result: Result<TenantReport, TenantError>,
}

/// Restricts open matches to the tip window: the next matchday plus any
/// postponed stragglers of earlier matchdays, never anything later.
/// The next matchday is the lowest matchday that has at least half as many
/// open matches as the largest open matchday, so a single postponed match
/// of an old matchday does not shift the window backwards.
fn tip_window(open: Vec<OpenMatch>) -> Vec<OpenMatch> {
    let mut counts: BTreeMap<u32, usize> = BTreeMap::new();
    for m in &open {
        *counts.entry(m.fixture.matchday).or_default() += 1;
    }
    let max = counts.values().copied().max().unwrap_or(0);
    let Some(next) = counts
        .iter()
        .find(|&(_, &n)| n * 2 >= max)
        .map(|(&md, _)| md)
    else {
        return Vec::new();
    };
    open.into_iter()
        .filter(|m| m.fixture.matchday <= next)
        .collect()
}

pub async fn run_tenant<P: TipPlatform, O: OddsProvider>(
    platform: &P,
    odds: &O,
    algorithm: &dyn TipAlgorithm,
    scoring: &ScoringScheme,
    rules: &Rules,
    opts: RunOptions,
) -> Result<TenantReport, TenantError> {
    platform.login().await?;
    let all_open = platform.open_matches().await?;
    let total = all_open.len();
    let open = tip_window(all_open);
    let matches_open = open.len();
    if open.len() < total {
        info!(
            dropped = total - open.len(),
            "ignoring matches beyond the next matchday"
        );
    }
    if open.is_empty() {
        info!("no open matches, nothing to do");
        return Ok(TenantReport {
            matches_open: 0,
            submitted: 0,
            skipped: 0,
            violations: 0,
            dry_run: opts.dry_run,
        });
    }

    let mut fixed: Vec<PlacedTip> = Vec::new();
    let mut todo: Vec<Match> = Vec::new();
    for m in open {
        match m.tip {
            Some(tip) if !opts.overwrite_existing => {
                info!(match_id = %m.fixture.id, home = %m.fixture.home, away = %m.fixture.away,
                      %tip, "keeping existing tip");
                fixed.push(PlacedTip {
                    matchday: m.fixture.matchday,
                    match_id: m.fixture.id,
                    tip,
                });
            }
            _ => todo.push(m.fixture),
        }
    }
    info!(
        open = matches_open,
        to_tip = todo.len(),
        "fetched open matches"
    );

    let history: Vec<PlacedTip> = if rules.needs_history() {
        let mut by_id: HashMap<MatchId, PlacedTip> = platform
            .season_tips()
            .await?
            .into_iter()
            .map(|t| (t.match_id.clone(), t))
            .collect();
        for t in fixed {
            by_id.insert(t.match_id.clone(), t);
        }
        for m in &todo {
            by_id.remove(&m.id);
        }
        let mut history: Vec<PlacedTip> = by_id.into_values().collect();
        history.sort_by(|a, b| a.match_id.cmp(&b.match_id));
        history
    } else {
        Vec::new()
    };

    let odds_map = odds.odds(&todo).await?;
    let mut choices = Vec::new();
    let mut skipped = 0;
    for m in &todo {
        let Some(o) = odds_map.get(&m.id) else {
            warn!(match_id = %m.id, home = %m.home, away = %m.away, "no odds, skipping match");
            skipped += 1;
            continue;
        };
        if let Err(e) = o.validate() {
            warn!(match_id = %m.id, home = %m.home, away = %m.away, error = %e, "skipping match");
            skipped += 1;
            continue;
        }
        match algorithm.rank(m, o, scoring) {
            Ok(candidates) if !candidates.is_empty() => choices.push(MatchChoice {
                match_id: m.id.clone(),
                matchday: m.matchday,
                candidates,
            }),
            Ok(_) => {
                warn!(match_id = %m.id, algorithm = algorithm.name(), "no candidates, skipping match");
                skipped += 1;
            }
            Err(e) => {
                warn!(match_id = %m.id, home = %m.home, away = %m.away,
                      algorithm = algorithm.name(), error = %e, "skipping match");
                skipped += 1;
            }
        }
    }

    let report = rules.apply(&history, &choices);
    for v in &report.violations {
        warn!(
            matchday = ?v.matchday,
            result = %v.result,
            count = v.count,
            max = v.max,
            "rule maxSameResult cannot be satisfied"
        );
    }
    for a in &report.assignments {
        let fixture = todo.iter().find(|m| m.id == a.match_id);
        info!(
            match_id = %a.match_id,
            home = fixture.map_or("", |m| m.home.as_str()),
            away = fixture.map_or("", |m| m.away.as_str()),
            tip = %a.tip,
            expected_points = format_args!("{:.3}", a.expected_points),
            rank = a.rank,
            "tip chosen"
        );
    }

    let submissions: Vec<TipSubmission> = report
        .assignments
        .iter()
        .map(|a| TipSubmission {
            match_id: a.match_id.clone(),
            tip: a.tip,
        })
        .collect();
    if submissions.is_empty() {
        info!("nothing to submit");
    } else if opts.dry_run {
        info!(count = submissions.len(), "dry run: would submit tips");
    } else {
        platform.submit_tips(&submissions).await?;
        info!(count = submissions.len(), "tips submitted");
    }

    Ok(TenantReport {
        matches_open,
        submitted: submissions.len(),
        skipped,
        violations: report.violations.len(),
        dry_run: opts.dry_run,
    })
}

async fn run_configured_tenant(
    tenant: TenantConfig,
    dry_run: bool,
) -> Result<TenantReport, TenantError> {
    let creds = match &tenant.credentials_dir {
        Some(dir) => Some(credentials::load(dir).await?),
        None => None,
    };
    let platform = Platform::build(&tenant.platform, creds).await?;
    let odds = OddsSource::build(&tenant.odds, &platform);
    let algorithm = algorithm::build(&tenant.algorithm);
    run_tenant(
        &platform,
        &odds,
        algorithm.as_ref(),
        &tenant.scoring,
        &tenant.rules,
        RunOptions {
            dry_run,
            overwrite_existing: tenant.overwrite_existing_tips,
        },
    )
    .await
}

/// Runs a tenant future on its own task with a timeout, so that a panic or
/// hang fails only this tenant.
async fn supervise<F>(timeout_seconds: u64, fut: F) -> Result<TenantReport, TenantError>
where
    F: Future<Output = Result<TenantReport, TenantError>> + Send + 'static,
{
    let task = tokio::spawn(tokio::time::timeout(
        Duration::from_secs(timeout_seconds),
        fut,
    ));
    match task.await {
        Ok(Ok(result)) => result,
        Ok(Err(_elapsed)) => Err(TenantError::Timeout(timeout_seconds)),
        Err(_join_error) => Err(TenantError::Panicked),
    }
}

/// Runs all tenants sequentially, in config order.
pub async fn run_all(config: Config, dry_run: bool) -> Vec<TenantOutcome> {
    let timeout_seconds = config.tenant_timeout_seconds;
    let mut outcomes = Vec::with_capacity(config.tenants.len());
    for tenant in config.tenants {
        let name = tenant.name.clone();
        let span = info_span!("tenant", tenant = %name);
        let result = supervise(
            timeout_seconds,
            run_configured_tenant(tenant, dry_run).instrument(span.clone()),
        )
        .await;
        span.in_scope(|| match &result {
            Ok(r) => info!(
                open = r.matches_open,
                submitted = r.submitted,
                skipped = r.skipped,
                violations = r.violations,
                dry_run = r.dry_run,
                "tenant finished"
            ),
            Err(e) => error!(error = %e, "tenant failed"),
        });
        outcomes.push(TenantOutcome { name, result });
    }
    outcomes
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::algorithm::poisson::PoissonAlgorithm;
    use crate::domain::{Odds, OverUnder, Tip};
    use crate::odds::PlatformOdds;
    use crate::platform::fake::{FakeData, FakeMatch, FakePlatform};
    use crate::platform::{FakeConfig, KicktippConfig, PlatformConfig};
    use crate::rules::{MaxSameResult, Scope};

    fn odds(home: f64, draw: f64, away: f64) -> Odds {
        Odds {
            home,
            draw,
            away,
            over_under_2_5: None,
        }
    }

    fn fake_match(id: &str, odds: Option<Odds>, tip: Option<Tip>) -> FakeMatch {
        FakeMatch {
            id: MatchId::new(id),
            home: format!("{id}-home"),
            away: format!("{id}-away"),
            kickoff: jiff::Timestamp::UNIX_EPOCH,
            matchday: 12,
            competition: "Test".into(),
            tip,
            odds,
        }
    }

    fn opts(dry_run: bool, overwrite_existing: bool) -> RunOptions {
        RunOptions {
            dry_run,
            overwrite_existing,
        }
    }

    fn top_tip(o: &Odds) -> Tip {
        PoissonAlgorithm::new(5)
            .rank(
                &Match {
                    id: MatchId::new("x"),
                    home: String::new(),
                    away: String::new(),
                    kickoff: jiff::Timestamp::UNIX_EPOCH,
                    matchday: 1,
                    competition: String::new(),
                },
                o,
                &ScoringScheme::default(),
            )
            .unwrap()[0]
            .tip
    }

    async fn run(
        p: &FakePlatform,
        rules: &Rules,
        o: RunOptions,
    ) -> Result<TenantReport, TenantError> {
        run_tenant(
            p,
            &PlatformOdds(p),
            &PoissonAlgorithm::new(5),
            &ScoringScheme::default(),
            rules,
            o,
        )
        .await
    }

    fn two_matches() -> FakeData {
        FakeData {
            matches: vec![
                fake_match("m1", Some(odds(1.5, 4.2, 6.5)), None),
                fake_match("m2", Some(odds(3.8, 3.5, 1.95)), None),
            ],
            history: vec![],
        }
    }

    #[tokio::test]
    async fn happy_path_submits_top_tips() {
        let p = FakePlatform::new(two_matches());
        let r = run(&p, &Rules::default(), opts(false, false))
            .await
            .unwrap();
        assert_eq!(r.matches_open, 2);
        assert_eq!(r.submitted, 2);
        assert_eq!(r.skipped, 0);
        let calls = p.submitted_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0],
            vec![
                TipSubmission {
                    match_id: MatchId::new("m1"),
                    tip: top_tip(&odds(1.5, 4.2, 6.5))
                },
                TipSubmission {
                    match_id: MatchId::new("m2"),
                    tip: top_tip(&odds(3.8, 3.5, 1.95))
                },
            ]
        );
    }

    #[tokio::test]
    async fn dry_run_submits_nothing() {
        let p = FakePlatform::new(two_matches());
        let r = run(&p, &Rules::default(), opts(true, false)).await.unwrap();
        assert!(r.dry_run);
        assert_eq!(r.submitted, 2);
        assert!(p.submitted_calls().is_empty());
    }

    #[tokio::test]
    async fn idempotent_and_overwrite() {
        let existing = Tip { home: 0, away: 0 };
        let p = FakePlatform::new(FakeData {
            matches: vec![
                fake_match("m1", Some(odds(1.5, 4.2, 6.5)), Some(existing)),
                fake_match("m2", Some(odds(3.8, 3.5, 1.95)), None),
            ],
            history: vec![],
        });
        let r = run(&p, &Rules::default(), opts(false, false))
            .await
            .unwrap();
        assert_eq!(r.submitted, 1);
        assert_eq!(p.submitted_calls()[0].len(), 1);
        assert_eq!(p.submitted_calls()[0][0].match_id, MatchId::new("m2"));

        let r = run(&p, &Rules::default(), opts(false, false))
            .await
            .unwrap();
        assert_eq!(r.submitted, 0);
        assert_eq!(p.submitted_calls().len(), 1);

        let r = run(&p, &Rules::default(), opts(false, true)).await.unwrap();
        assert_eq!(r.submitted, 2);
        let calls = p.submitted_calls();
        assert_eq!(calls.len(), 2);
        let m1 = calls[1]
            .iter()
            .find(|s| s.match_id == MatchId::new("m1"))
            .unwrap();
        assert_eq!(m1.tip, top_tip(&odds(1.5, 4.2, 6.5)));
        assert_ne!(m1.tip, existing);
    }

    #[tokio::test]
    async fn missing_odds_skips_match() {
        let p = FakePlatform::new(FakeData {
            matches: vec![
                fake_match("m1", None, None),
                fake_match("m2", Some(odds(3.8, 3.5, 1.95)), None),
            ],
            history: vec![],
        });
        let r = run(&p, &Rules::default(), opts(false, false))
            .await
            .unwrap();
        assert_eq!(r.skipped, 1);
        assert_eq!(r.submitted, 1);
        assert_eq!(p.submitted_calls()[0][0].match_id, MatchId::new("m2"));
    }

    #[tokio::test]
    async fn max_same_result_uses_history() {
        // High-scoring favourites (O/U 2.5 favours over), so the top tip is 2:1 / 1:2.
        let ou = Some(OverUnder {
            over: 1.4,
            under: 2.9,
        });
        let fav = Odds {
            over_under_2_5: ou.clone(),
            ..odds(1.5, 4.6, 5.5)
        };
        let underdog_home = Odds {
            over_under_2_5: ou,
            ..odds(5.5, 4.6, 1.5)
        };
        let top = top_tip(&fav);
        assert_eq!(top_tip(&underdog_home), Tip { home: 1, away: 2 });
        let mirrored = |t: Tip| (t.home.max(t.away), t.home.min(t.away));
        assert_eq!(
            mirrored(top),
            (2, 1),
            "test premise: favourite's top tip is 2:1"
        );
        let history = (0..6)
            .map(|i| PlacedTip {
                matchday: 1 + i,
                match_id: MatchId::new(format!("h{i}")),
                tip: if i % 2 == 0 {
                    Tip { home: 2, away: 1 }
                } else {
                    Tip { home: 1, away: 2 }
                },
            })
            .collect();
        let p = FakePlatform::new(FakeData {
            matches: vec![
                fake_match("m1", Some(fav.clone()), None),
                fake_match("m2", Some(underdog_home), None),
            ],
            history,
        });
        let rules = Rules {
            max_same_result: Some(MaxSameResult {
                max: 6,
                scope: Scope::Season,
                mirrored: true,
            }),
        };
        let r = run(&p, &rules, opts(false, false)).await.unwrap();
        assert_eq!(r.submitted, 2);
        assert_eq!(r.violations, 0);
        for s in &p.submitted_calls()[0] {
            assert_ne!(mirrored(s.tip), (2, 1), "{}", s.tip);
        }
    }

    #[tokio::test]
    async fn mixed_matchday_rule_counts_started_tips() {
        // Matchday 10 is running: six already started matches (synthetic ids
        // as produced by the kicktipp client) carry mirrored 2:1/1:2 tips, so
        // the per-matchday limit of 6 is reached for the open matches.
        let ou = Some(OverUnder {
            over: 1.4,
            under: 2.9,
        });
        let fav = Odds {
            over_under_2_5: ou.clone(),
            ..odds(1.5, 4.6, 5.5)
        };
        let underdog_home = Odds {
            over_under_2_5: ou,
            ..odds(5.5, 4.6, 1.5)
        };
        let mirrored = |t: Tip| (t.home.max(t.away), t.home.min(t.away));
        assert_eq!(
            mirrored(top_tip(&fav)),
            (2, 1),
            "test premise: favourite's top tip is 2:1"
        );
        let history = (0..6)
            .map(|i| PlacedTip {
                matchday: 10,
                match_id: MatchId::new(format!("md10-row{i}")),
                tip: if i % 2 == 0 {
                    Tip { home: 2, away: 1 }
                } else {
                    Tip { home: 1, away: 2 }
                },
            })
            .collect();
        let p = FakePlatform::new(FakeData {
            matches: vec![
                on_matchday(10, fake_match("1644299505", Some(fav), None)),
                on_matchday(10, fake_match("1644299507", Some(underdog_home), None)),
            ],
            history,
        });
        let rules = Rules {
            max_same_result: Some(MaxSameResult {
                max: 6,
                scope: Scope::Matchday,
                mirrored: true,
            }),
        };
        let r = run(&p, &rules, opts(false, false)).await.unwrap();
        assert_eq!(r.submitted, 2);
        assert_eq!(r.violations, 0);
        for s in &p.submitted_calls()[0] {
            assert_ne!(mirrored(s.tip), (2, 1), "{}", s.tip);
        }
    }

    fn on_matchday(matchday: u32, m: FakeMatch) -> FakeMatch {
        FakeMatch { matchday, ..m }
    }

    fn submitted_ids(p: &FakePlatform) -> Vec<String> {
        p.submitted_calls()[0]
            .iter()
            .map(|s| s.match_id.to_string())
            .collect()
    }

    #[test]
    fn tip_window_of_nothing_is_empty() {
        assert!(tip_window(vec![]).is_empty());
    }

    #[tokio::test]
    async fn window_drops_following_matchday() {
        let p = FakePlatform::new(FakeData {
            matches: vec![
                fake_match("m1", Some(odds(1.5, 4.2, 6.5)), None),
                fake_match("m2", Some(odds(3.8, 3.5, 1.95)), None),
                on_matchday(13, fake_match("m3", Some(odds(2.0, 3.4, 3.6)), None)),
            ],
            history: vec![],
        });
        let r = run(&p, &Rules::default(), opts(false, false))
            .await
            .unwrap();
        assert_eq!(r.matches_open, 2);
        assert_eq!(r.submitted, 2);
        assert_eq!(submitted_ids(&p), vec!["m1", "m2"]);
    }

    #[tokio::test]
    async fn window_includes_postponed_straggler_not_next() {
        let o = || Some(odds(2.0, 3.4, 3.6));
        let p = FakePlatform::new(FakeData {
            matches: vec![
                on_matchday(11, fake_match("a1", o(), None)),
                fake_match("b1", o(), None),
                fake_match("b2", o(), None),
                fake_match("b3", o(), None),
                on_matchday(13, fake_match("c1", o(), None)),
                on_matchday(13, fake_match("c2", o(), None)),
                on_matchday(13, fake_match("c3", o(), None)),
            ],
            history: vec![],
        });
        let r = run(&p, &Rules::default(), opts(false, false))
            .await
            .unwrap();
        assert_eq!(r.matches_open, 4);
        assert_eq!(r.submitted, 4);
        assert_eq!(submitted_ids(&p), vec!["a1", "b1", "b2", "b3"]);
    }

    #[tokio::test]
    async fn no_open_matches_is_ok() {
        let p = FakePlatform::new(FakeData::default());
        let r = run(&p, &Rules::default(), opts(false, false))
            .await
            .unwrap();
        assert_eq!(r.matches_open, 0);
        assert_eq!(r.submitted, 0);
        assert!(p.submitted_calls().is_empty());
    }

    #[tokio::test]
    async fn window_keeps_history_of_outside_matches() {
        // Same premise as max_same_result_uses_history: the favourite's top
        // tip is 2:1. Five 2:1/1:2 tips in history plus an existing 2:1 tip on
        // a matchday-13 match (outside the window) reach the limit of 6.
        let fav = Odds {
            over_under_2_5: Some(OverUnder {
                over: 1.4,
                under: 2.9,
            }),
            ..odds(1.5, 4.6, 5.5)
        };
        let mirrored = |t: Tip| (t.home.max(t.away), t.home.min(t.away));
        assert_eq!(
            mirrored(top_tip(&fav)),
            (2, 1),
            "test premise: favourite's top tip is 2:1"
        );
        let history = (0..5)
            .map(|i| PlacedTip {
                matchday: 1 + i,
                match_id: MatchId::new(format!("h{i}")),
                tip: if i % 2 == 0 {
                    Tip { home: 2, away: 1 }
                } else {
                    Tip { home: 1, away: 2 }
                },
            })
            .collect();
        let p = FakePlatform::new(FakeData {
            matches: vec![
                fake_match("m1", Some(fav), None),
                on_matchday(
                    13,
                    fake_match(
                        "m2",
                        Some(odds(2.0, 3.4, 3.6)),
                        Some(Tip { home: 2, away: 1 }),
                    ),
                ),
            ],
            history,
        });
        let rules = Rules {
            max_same_result: Some(MaxSameResult {
                max: 6,
                scope: Scope::Season,
                mirrored: true,
            }),
        };
        let r = run(&p, &rules, opts(false, false)).await.unwrap();
        assert_eq!(r.matches_open, 1);
        assert_eq!(r.submitted, 1);
        assert_eq!(r.violations, 0);
        let calls = p.submitted_calls();
        assert_eq!(calls[0].len(), 1);
        assert_eq!(calls[0][0].match_id, MatchId::new("m1"));
        assert_ne!(mirrored(calls[0][0].tip), (2, 1), "{}", calls[0][0].tip);
    }

    #[tokio::test]
    async fn login_failure_fails_tenant() {
        let p = FakePlatform::new(two_matches()).fail_login(true);
        let err = run(&p, &Rules::default(), opts(false, false))
            .await
            .unwrap_err();
        assert!(matches!(err, TenantError::Platform(PlatformError::Auth(_))));
        assert!(p.submitted_calls().is_empty());
    }

    fn tenant(
        name: &str,
        platform: PlatformConfig,
        credentials_dir: Option<PathBuf>,
    ) -> TenantConfig {
        TenantConfig {
            name: name.into(),
            credentials_dir,
            platform,
            odds: Default::default(),
            algorithm: Default::default(),
            scoring: Default::default(),
            rules: Default::default(),
            overwrite_existing_tips: false,
        }
    }

    #[tokio::test]
    async fn run_all_isolates_tenant_failures() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/fake-data.yaml");
        let config = Config {
            tenant_timeout_seconds: 30,
            tenants: vec![
                tenant("fake", PlatformConfig::Fake(FakeConfig { fixture }), None),
                tenant(
                    "kicktipp",
                    PlatformConfig::Kicktipp(KicktippConfig {
                        community: "test-round".into(),
                    }),
                    Some(PathBuf::from("/nonexistent/betbot-test")),
                ),
            ],
        };
        let outcomes = run_all(config, true).await;
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].name, "fake");
        assert!(outcomes[0].result.is_ok(), "{:?}", outcomes[0].result);
        assert_eq!(outcomes[1].name, "kicktipp");
        assert!(matches!(
            outcomes[1].result,
            Err(TenantError::Credentials(_))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn supervise_times_out() {
        let r = supervise(5, std::future::pending()).await;
        assert!(matches!(r, Err(TenantError::Timeout(5))));
    }

    #[tokio::test]
    async fn supervise_catches_panic() {
        let r = supervise(5, async { panic!("boom") }).await;
        assert!(matches!(r, Err(TenantError::Panicked)));
    }
}
