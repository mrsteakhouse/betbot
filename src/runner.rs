//! Orchestrates one run: per tenant login, fetch matches/history/odds, rank,
//! apply rules and submit.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use thiserror::Error;
use tracing::{Instrument, debug, error, info, info_span, warn};

use crate::algorithm::{self, TipAlgorithm};
use crate::config::{Config, TenantConfig};
use crate::credentials::{self, CredentialsError};
use crate::domain::{Match, MatchId, OpenMatch, PlacedTip, ScoringScheme, Tip, TipSubmission};
use crate::notify::{self, Notifier, Notify, NotifyError};
use crate::odds::{OddsError, OddsProvider, OddsSource};
use crate::platform::{Platform, PlatformError, TipPlatform};
use crate::rules::{MatchChoice, Rules, Violation};

/// Upper bound for one notification send (outside the tenant timeout).
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(15);

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
    /// Matchdays of the tip window (sorted, unique).
    pub matchdays: Vec<u32>,
    /// The `submitted` tips, in submission order.
    pub tips: Vec<ReportedTip>,
    /// Skipped matches and unsatisfiable rules.
    pub warnings: Vec<TenantWarning>,
}

/// A tip chosen in this run (submitted, or would-be submitted in a dry run).
#[derive(Debug, Clone, PartialEq)]
pub struct ReportedTip {
    pub matchday: u32,
    pub home: String,
    pub away: String,
    pub tip: Tip,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TenantWarning {
    /// A match of the tip window was not tipped.
    Skipped {
        home: String,
        away: String,
        reason: String,
    },
    /// `maxSameResult` cannot be satisfied (best tips kept).
    MaxSameResult(Violation),
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
            matchdays: Vec::new(),
            tips: Vec::new(),
            warnings: Vec::new(),
        });
    }

    let window: Vec<u32> = open.iter().map(|m| m.fixture.matchday).collect();
    let mut matchdays = window.clone();
    matchdays.sort_unstable();
    matchdays.dedup();
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

    let history: Vec<PlacedTip> = if let Some(scope) = rules.history_scope(&window) {
        debug!(?scope, "reading tip history");
        let mut by_id: HashMap<MatchId, PlacedTip> = platform
            .placed_tips(&scope)
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
    let mut warnings = Vec::new();
    let mut skip = |m: &Match, reason: String| {
        skipped += 1;
        warnings.push(TenantWarning::Skipped {
            home: m.home.clone(),
            away: m.away.clone(),
            reason,
        });
    };
    for m in &todo {
        let Some(o) = odds_map.get(&m.id) else {
            warn!(match_id = %m.id, home = %m.home, away = %m.away, "no odds, skipping match");
            skip(m, "keine Quoten".into());
            continue;
        };
        if let Err(e) = o.validate() {
            warn!(match_id = %m.id, home = %m.home, away = %m.away, error = %e, "skipping match");
            skip(m, e.to_string());
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
                skip(m, "keine Tipp-Kandidaten".into());
            }
            Err(e) => {
                warn!(match_id = %m.id, home = %m.home, away = %m.away,
                      algorithm = algorithm.name(), error = %e, "skipping match");
                skip(m, e.to_string());
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
        warnings.push(TenantWarning::MaxSameResult(v.clone()));
    }
    let mut tips = Vec::with_capacity(report.assignments.len());
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
        tips.push(ReportedTip {
            matchday: fixture.map_or(0, |m| m.matchday),
            home: fixture.map_or_else(String::new, |m| m.home.clone()),
            away: fixture.map_or_else(String::new, |m| m.away.clone()),
            tip: a.tip,
        });
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
        matchdays,
        tips,
        warnings,
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

/// Builds the notifiers referenced by at least one tenant. A notifier whose
/// URL file cannot be read or parsed is disabled with a warning; this never
/// fails the run.
async fn build_notifiers(config: &Config) -> HashMap<String, Notifier> {
    let referenced: HashSet<&str> = config
        .tenants
        .iter()
        .flat_map(|t| t.notifiers.iter().map(String::as_str))
        .collect();
    let mut notifiers = HashMap::new();
    if referenced.is_empty() {
        return notifiers;
    }
    let client = match notify::http_client() {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "cannot create HTTP client, notifications disabled");
            return notifiers;
        }
    };
    for n in &config.notifiers {
        if !referenced.contains(n.name.as_str()) {
            continue;
        }
        let built = match credentials::load_secret(&n.url_file, "notifier url").await {
            Ok(url) => Notifier::from_url(&url, client.clone()),
            Err(e) => Err(NotifyError::from(e)),
        };
        match built {
            Ok(notifier) => {
                debug!(notifier = %n.name, kind = notifier.kind(), "notifier ready");
                notifiers.insert(n.name.clone(), notifier);
            }
            Err(e) => warn!(notifier = %n.name, error = %e, "notifier disabled"),
        }
    }
    notifiers
}

/// Sends the tenant's run summary to its notifiers (if there is anything to
/// report). Failures are logged only; they never change the run outcome.
async fn notify_tenant(
    tenant: &str,
    result: &Result<TenantReport, TenantError>,
    refs: &[String],
    notifiers: &HashMap<String, Notifier>,
    dry_run: bool,
) {
    if refs.is_empty() {
        return;
    }
    let Some(msg) = notify::message::summary(tenant, result, dry_run) else {
        debug!("nothing to notify");
        return;
    };
    for name in refs {
        if dry_run {
            info!(notifier = %name, title = %msg.title, body = %msg.body, "dry run: would notify");
            continue;
        }
        let Some(notifier) = notifiers.get(name) else {
            debug!(notifier = %name, "notifier disabled, skipping");
            continue;
        };
        match tokio::time::timeout(NOTIFY_TIMEOUT, notifier.send(&msg)).await {
            Ok(Ok(())) => info!(notifier = %name, "notification sent"),
            Ok(Err(e)) => warn!(notifier = %name, error = %e, "notification failed"),
            Err(_elapsed) => warn!(
                notifier = %name,
                timeout_seconds = NOTIFY_TIMEOUT.as_secs(),
                "notification failed: timed out"
            ),
        }
    }
}

/// Runs all tenants sequentially, in config order, and sends each tenant's
/// summary to its notifiers.
pub async fn run_all(config: Config, dry_run: bool) -> Vec<TenantOutcome> {
    let timeout_seconds = config.tenant_timeout_seconds;
    let notifiers = build_notifiers(&config).await;
    let mut outcomes = Vec::with_capacity(config.tenants.len());
    for tenant in config.tenants {
        let name = tenant.name.clone();
        let refs = tenant.notifiers.clone();
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
        notify_tenant(&name, &result, &refs, &notifiers, dry_run)
            .instrument(span)
            .await;
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
    use crate::platform::{FakeConfig, HistoryScope, KicktippConfig, PlatformConfig};
    use crate::rules::{MaxSameResult, Scope};
    use crate::testutil::http_mock::{MockServer, Response};

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

    #[tokio::test]
    async fn matchday_rule_reads_only_window_history() {
        // Six mirrored 2:1/1:2 tips on other matchdays must not affect a
        // per-matchday rule for matchday 12.
        let fav = Odds {
            over_under_2_5: Some(OverUnder {
                over: 1.4,
                under: 2.9,
            }),
            ..odds(1.5, 4.6, 5.5)
        };
        let top = top_tip(&fav);
        let mirrored = |t: Tip| (t.home.max(t.away), t.home.min(t.away));
        assert_eq!(mirrored(top), (2, 1), "test premise: top tip is 2:1");
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
            matches: vec![fake_match("m1", Some(fav), None)],
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
        assert_eq!(r.submitted, 1);
        assert_eq!(r.violations, 0);
        assert_eq!(p.submitted_calls()[0][0].tip, top);
        assert_eq!(p.requested_scopes(), [HistoryScope::Matchdays(vec![12])]);
    }

    #[tokio::test]
    async fn history_scope_requested_per_rule() {
        let season = Rules {
            max_same_result: Some(MaxSameResult {
                max: 6,
                scope: Scope::Season,
                mirrored: true,
            }),
        };
        let p = FakePlatform::new(two_matches());
        run(&p, &season, opts(true, false)).await.unwrap();
        assert_eq!(p.requested_scopes(), [HistoryScope::Season]);

        let p = FakePlatform::new(two_matches());
        run(&p, &Rules::default(), opts(true, false)).await.unwrap();
        assert!(p.requested_scopes().is_empty());
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
            notifiers: Vec::new(),
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
            notifiers: Vec::new(),
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

    /// Temp dir with a fake fixture and a generic notifier URL file.
    struct NotifyEnv {
        dir: PathBuf,
    }

    impl NotifyEnv {
        fn new(tag: &str, fixture: &str, url: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("betbot-notify-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("fixture.yaml"), fixture).unwrap();
            std::fs::write(dir.join("url"), format!("{url}\n")).unwrap();
            Self { dir }
        }

        fn config(&self, url_file: PathBuf) -> Config {
            let mut t = tenant(
                "alice",
                PlatformConfig::Fake(FakeConfig {
                    fixture: self.dir.join("fixture.yaml"),
                }),
                None,
            );
            t.notifiers = vec!["hook".into()];
            Config {
                tenant_timeout_seconds: 30,
                tenants: vec![t],
                notifiers: vec![crate::config::NotifierConfig {
                    name: "hook".into(),
                    url_file,
                }],
            }
        }

        fn url_file(&self) -> PathBuf {
            self.dir.join("url")
        }
    }

    impl Drop for NotifyEnv {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn generic_url(server: &MockServer) -> String {
        let b = server.base();
        format!(
            "generic://{}:{}/hook?disabletls=yes",
            b.host_str().unwrap(),
            b.port().unwrap()
        )
    }

    fn example_fixture() -> String {
        std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/fake-data.yaml"),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn run_all_notifies_once_after_submitting() {
        let server = MockServer::start(|_| Response::ok("")).await;
        let env = NotifyEnv::new("submit", &example_fixture(), &generic_url(&server));
        let outcomes = run_all(env.config(env.url_file()), false).await;
        assert!(outcomes[0].result.is_ok(), "{:?}", outcomes[0].result);
        let reqs = server.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].path(), "/hook");
        assert!(
            reqs[0]
                .body
                .contains("betbot alice: Spieltag 12 – 2 Tipps abgegeben"),
            "{}",
            reqs[0].body
        );
        assert!(reqs[0].body.contains("FC Nordstadt - SV Südhafen"));
    }

    #[tokio::test]
    async fn run_all_sends_nothing_when_nothing_to_do() {
        let server = MockServer::start(|_| Response::ok("")).await;
        let env = NotifyEnv::new("idle", "matches: []\n", &generic_url(&server));
        let outcomes = run_all(env.config(env.url_file()), false).await;
        assert!(outcomes[0].result.is_ok());
        assert!(server.requests().is_empty());
    }

    #[tokio::test]
    async fn run_all_dry_run_does_not_send() {
        let server = MockServer::start(|_| Response::ok("")).await;
        let env = NotifyEnv::new("dry", &example_fixture(), &generic_url(&server));
        let outcomes = run_all(env.config(env.url_file()), true).await;
        assert!(outcomes[0].result.is_ok());
        assert!(server.requests().is_empty());
    }

    #[tokio::test]
    async fn run_all_notifies_failure() {
        let server = MockServer::start(|_| Response::ok("")).await;
        let env = NotifyEnv::new("fail", "not: [valid", &generic_url(&server));
        let outcomes = run_all(env.config(env.url_file()), false).await;
        assert!(outcomes[0].result.is_err());
        let reqs = server.requests();
        assert_eq!(reqs.len(), 1);
        assert!(
            reqs[0].body.contains(r#""level":"error""#),
            "{}",
            reqs[0].body
        );
        assert!(reqs[0].body.contains("betbot alice: Fehler"));
    }

    #[tokio::test]
    async fn notification_failures_do_not_change_outcome() {
        let server = MockServer::start(|_| Response::status(500)).await;
        let env = NotifyEnv::new("http500", &example_fixture(), &generic_url(&server));
        let outcomes = run_all(env.config(env.url_file()), false).await;
        assert!(outcomes[0].result.is_ok(), "{:?}", outcomes[0].result);
        assert_eq!(server.requests().len(), 1);

        // Unreadable URL file: notifier disabled, run unaffected.
        let outcomes = run_all(env.config(env.dir.join("missing")), false).await;
        assert!(outcomes[0].result.is_ok(), "{:?}", outcomes[0].result);
        // Malformed URL: same.
        std::fs::write(env.url_file(), "smtp://x").unwrap();
        let outcomes = run_all(env.config(env.url_file()), false).await;
        assert!(outcomes[0].result.is_ok(), "{:?}", outcomes[0].result);
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn report_carries_tips_and_warnings() {
        let p = FakePlatform::new(FakeData {
            matches: vec![
                fake_match("m1", None, None),
                fake_match("m2", Some(odds(3.8, 3.5, 1.95)), None),
            ],
            history: vec![],
        });
        let r = run(&p, &Rules::default(), opts(true, false)).await.unwrap();
        assert_eq!(r.matchdays, [12]);
        assert_eq!(
            r.tips,
            [ReportedTip {
                matchday: 12,
                home: "m2-home".into(),
                away: "m2-away".into(),
                tip: top_tip(&odds(3.8, 3.5, 1.95)),
            }]
        );
        assert_eq!(
            r.warnings,
            [TenantWarning::Skipped {
                home: "m1-home".into(),
                away: "m1-away".into(),
                reason: "keine Quoten".into(),
            }]
        );
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
