//! In-memory platform backed by a YAML fixture, for dry runs and tests.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::Deserialize;

use super::{HistoryScope, PlatformError, TipPlatform};
use crate::domain::{Match, MatchId, Odds, OddsMap, OpenMatch, PlacedTip, Tip, TipSubmission};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FakeConfig {
    /// YAML file with matches, current tips, odds and tip history.
    pub fixture: PathBuf,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FakeData {
    pub matches: Vec<FakeMatch>,
    #[serde(default)]
    pub history: Vec<PlacedTip>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FakeMatch {
    pub id: MatchId,
    pub home: String,
    pub away: String,
    pub kickoff: jiff::Timestamp,
    pub matchday: u32,
    pub competition: String,
    #[serde(default)]
    pub tip: Option<Tip>,
    #[serde(default)]
    pub odds: Option<Odds>,
}

impl FakeMatch {
    fn fixture(&self) -> Match {
        Match {
            id: self.id.clone(),
            home: self.home.clone(),
            away: self.away.clone(),
            kickoff: self.kickoff,
            matchday: self.matchday,
            competition: self.competition.clone(),
        }
    }
}

struct FakeState {
    data: FakeData,
    submitted: Vec<Vec<TipSubmission>>,
    scopes: Vec<HistoryScope>,
}

pub struct FakePlatform {
    state: Mutex<FakeState>,
    fail_login: bool,
}

impl FakePlatform {
    pub fn new(data: FakeData) -> Self {
        Self {
            state: Mutex::new(FakeState {
                data,
                submitted: Vec::new(),
                scopes: Vec::new(),
            }),
            fail_login: false,
        }
    }

    pub async fn from_file(path: &Path) -> Result<Self, PlatformError> {
        let text = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| PlatformError::Fixture(format!("{}: {e}", path.display())))?;
        let data: FakeData = serde_saphyr::from_str(&text)
            .map_err(|e| PlatformError::Fixture(format!("{}: {e}", path.display())))?;
        Ok(Self::new(data))
    }

    /// Test knob: make `login` fail with `PlatformError::Auth`.
    #[cfg_attr(not(test), expect(dead_code, reason = "test knob"))]
    pub fn fail_login(mut self, fail: bool) -> Self {
        self.fail_login = fail;
        self
    }

    /// Every `submit_tips` call so far, in order.
    #[cfg_attr(not(test), expect(dead_code, reason = "test inspection"))]
    pub fn submitted_calls(&self) -> Vec<Vec<TipSubmission>> {
        self.state().submitted.clone()
    }

    /// The scope of every `placed_tips` call so far, in order.
    #[cfg_attr(not(test), expect(dead_code, reason = "test inspection"))]
    pub fn requested_scopes(&self) -> Vec<HistoryScope> {
        self.state().scopes.clone()
    }

    fn state(&self) -> MutexGuard<'_, FakeState> {
        // The state stays consistent even if a holder panicked; keep using it.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl TipPlatform for FakePlatform {
    async fn login(&self) -> Result<(), PlatformError> {
        if self.fail_login {
            return Err(PlatformError::Auth("fake login rejected".into()));
        }
        Ok(())
    }

    async fn open_matches(&self) -> Result<Vec<OpenMatch>, PlatformError> {
        Ok(self
            .state()
            .data
            .matches
            .iter()
            .map(|m| OpenMatch {
                fixture: m.fixture(),
                tip: m.tip,
            })
            .collect())
    }

    async fn placed_tips(&self, scope: &HistoryScope) -> Result<Vec<PlacedTip>, PlatformError> {
        let mut state = self.state();
        state.scopes.push(scope.clone());
        let current = state.data.matches.iter().filter_map(|m| {
            m.tip.map(|tip| PlacedTip {
                matchday: m.matchday,
                match_id: m.id.clone(),
                tip,
            })
        });
        Ok(state
            .data
            .history
            .iter()
            .cloned()
            .chain(current)
            .filter(|t| match scope {
                HistoryScope::Season => true,
                HistoryScope::Matchdays(v) => v.contains(&t.matchday),
            })
            .collect())
    }

    async fn odds(&self, matches: &[Match]) -> Result<OddsMap, PlatformError> {
        let state = self.state();
        Ok(matches
            .iter()
            .filter_map(|wanted| {
                state
                    .data
                    .matches
                    .iter()
                    .find(|m| m.id == wanted.id)
                    .and_then(|m| m.odds.clone())
                    .map(|o| (wanted.id.clone(), o))
            })
            .collect())
    }

    async fn submit_tips(&self, tips: &[TipSubmission]) -> Result<(), PlatformError> {
        let mut state = self.state();
        if let Some(unknown) = tips
            .iter()
            .find(|t| !state.data.matches.iter().any(|m| m.id == t.match_id))
        {
            return Err(PlatformError::Rejected(format!(
                "unknown match {}",
                unknown.match_id
            )));
        }
        for t in tips {
            if let Some(m) = state.data.matches.iter_mut().find(|m| m.id == t.match_id) {
                m.tip = Some(t.tip);
            }
        }
        state.submitted.push(tips.to_vec());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn loads_example_fixture() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/fake-data.yaml");
        let p = FakePlatform::from_file(&path).await.unwrap();
        let open = p.open_matches().await.unwrap();
        assert_eq!(open.len(), 3);
        assert!(open.iter().all(|m| m.fixture.matchday == 12));
        assert_eq!(open.iter().filter(|m| m.tip.is_some()).count(), 1);
        let fixtures: Vec<Match> = open.into_iter().map(|m| m.fixture).collect();
        let odds = p.odds(&fixtures).await.unwrap();
        assert_eq!(odds.len(), 3);
        assert!(odds.values().any(|o| o.over_under_2_5.is_some()));
        let tips = p.placed_tips(&HistoryScope::Season).await.unwrap();
        assert!(tips.iter().any(|t| t.matchday == 11));
        assert!(tips.iter().any(|t| t.matchday == 12));
    }

    #[tokio::test]
    async fn placed_tips_filters_matchdays() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/fake-data.yaml");
        let p = FakePlatform::from_file(&path).await.unwrap();
        let md12 = HistoryScope::Matchdays(vec![12]);
        let tips = p.placed_tips(&md12).await.unwrap();
        assert!(!tips.is_empty());
        assert!(tips.iter().all(|t| t.matchday == 12));
        let md11 = HistoryScope::Matchdays(vec![11]);
        let tips = p.placed_tips(&md11).await.unwrap();
        assert!(!tips.is_empty());
        assert!(tips.iter().all(|t| t.matchday == 11));
        assert_eq!(p.requested_scopes(), [md12, md11]);
    }

    #[tokio::test]
    async fn submit_updates_state_and_rejects_unknown() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/fake-data.yaml");
        let p = FakePlatform::from_file(&path).await.unwrap();
        let open = p.open_matches().await.unwrap();
        let id = open[0].fixture.id.clone();
        let tip = Tip { home: 3, away: 0 };
        p.submit_tips(&[TipSubmission {
            match_id: id.clone(),
            tip,
        }])
        .await
        .unwrap();
        let open = p.open_matches().await.unwrap();
        assert_eq!(
            open.iter().find(|m| m.fixture.id == id).unwrap().tip,
            Some(tip)
        );
        assert_eq!(p.submitted_calls().len(), 1);

        let err = p
            .submit_tips(&[TipSubmission {
                match_id: MatchId::new("nope"),
                tip,
            }])
            .await
            .unwrap_err();
        assert!(matches!(err, PlatformError::Rejected(_)));
        assert_eq!(p.submitted_calls().len(), 1);
    }
}
