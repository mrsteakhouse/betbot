//! Core domain types shared by platforms, odds providers, algorithms and rules.

use std::collections::HashMap;
use std::fmt;

use serde::Deserialize;
use thiserror::Error;

/// Platform-specific identifier of a match.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize)]
#[serde(transparent)]
pub struct MatchId(String);

impl MatchId {
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used by the kicktipp client in the next step")
    )]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    #[expect(dead_code, reason = "used by the kicktipp client in the next step")]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MatchId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A fixture (scheduled match).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Match {
    pub id: MatchId,
    pub home: String,
    pub away: String,
    #[expect(dead_code, reason = "used by the kicktipp client in the next step")]
    pub kickoff: jiff::Timestamp,
    pub matchday: u32,
    #[expect(dead_code, reason = "used by the kicktipp client in the next step")]
    pub competition: String,
}

/// A score line, used both for tips and for match results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tip {
    pub home: u8,
    pub away: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    HomeWin,
    Draw,
    AwayWin,
}

impl Tip {
    pub fn outcome(self) -> Outcome {
        match self.home.cmp(&self.away) {
            std::cmp::Ordering::Greater => Outcome::HomeWin,
            std::cmp::Ordering::Equal => Outcome::Draw,
            std::cmp::Ordering::Less => Outcome::AwayWin,
        }
    }

    /// Goal difference home minus away.
    pub fn diff(self) -> i16 {
        i16::from(self.home) - i16::from(self.away)
    }
}

impl fmt::Display for Tip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.home, self.away)
    }
}

/// Decimal (European) odds for a match.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Odds {
    pub home: f64,
    pub draw: f64,
    pub away: f64,
    #[serde(default, rename = "overUnder25")]
    pub over_under_2_5: Option<OverUnder>,
}

/// Decimal odds for over/under 2.5 goals.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverUnder {
    pub over: f64,
    pub under: f64,
}

#[derive(Debug, Clone, Error)]
#[error("invalid odds: {0}")]
pub struct InvalidOdds(pub &'static str);

/// Accepted range of the inverse odds sum (1 + bookmaker margin).
const INVERSE_SUM_RANGE: std::ops::RangeInclusive<f64> = 0.99..=1.5;

fn valid_odd(o: f64) -> bool {
    o.is_finite() && o > 1.0
}

impl Odds {
    pub fn validate(&self) -> Result<(), InvalidOdds> {
        if ![self.home, self.draw, self.away].into_iter().all(valid_odd) {
            return Err(InvalidOdds("1X2 odds must be finite and greater than 1"));
        }
        let inverse_sum = 1.0 / self.home + 1.0 / self.draw + 1.0 / self.away;
        if !INVERSE_SUM_RANGE.contains(&inverse_sum) {
            return Err(InvalidOdds("1X2 margin out of range"));
        }
        if let Some(ou) = &self.over_under_2_5 {
            if !valid_odd(ou.over) || !valid_odd(ou.under) {
                return Err(InvalidOdds(
                    "over/under odds must be finite and greater than 1",
                ));
            }
            let inverse_sum = 1.0 / ou.over + 1.0 / ou.under;
            if !INVERSE_SUM_RANGE.contains(&inverse_sum) {
                return Err(InvalidOdds("over/under margin out of range"));
            }
        }
        Ok(())
    }
}

/// A match still open for tipping, with the user's current tip if any.
#[derive(Debug, Clone)]
pub struct OpenMatch {
    pub fixture: Match,
    pub tip: Option<Tip>,
}

/// A tip the user has placed (any matchday of the season).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlacedTip {
    pub matchday: u32,
    pub match_id: MatchId,
    pub tip: Tip,
}

/// A tip to be sent to the platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TipSubmission {
    pub match_id: MatchId,
    pub tip: Tip,
}

/// A ranked tip with its expected points.
#[derive(Debug, Clone, Copy)]
pub struct Candidate {
    pub tip: Tip,
    pub expected_points: f64,
}

pub type OddsMap = HashMap<MatchId, Odds>;

/// Points awarded for a correct tendency, goal difference or exact result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutcomePoints {
    pub tendency: u32,
    #[serde(default)]
    pub goal_difference: Option<u32>,
    pub exact: u32,
}

/// Points scheme per result outcome. Default is the kicktipp standard scheme.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct ScoringScheme {
    pub home_win: OutcomePoints,
    pub draw: OutcomePoints,
    pub away_win: OutcomePoints,
}

impl Default for ScoringScheme {
    fn default() -> Self {
        let win = OutcomePoints {
            tendency: 2,
            goal_difference: Some(3),
            exact: 4,
        };
        Self {
            home_win: win,
            draw: OutcomePoints {
                tendency: 2,
                goal_difference: None,
                exact: 4,
            },
            away_win: win,
        }
    }
}

impl ScoringScheme {
    pub fn points(&self, tip: Tip, result: Tip) -> u32 {
        let ro = result.outcome();
        if tip.outcome() != ro {
            return 0;
        }
        let p = match ro {
            Outcome::HomeWin => self.home_win,
            Outcome::Draw => self.draw,
            Outcome::AwayWin => self.away_win,
        };
        if tip == result {
            p.exact
        } else if tip.diff() == result.diff() {
            p.goal_difference.unwrap_or(p.tendency)
        } else {
            p.tendency
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(home: u8, away: u8) -> Tip {
        Tip { home, away }
    }

    #[test]
    fn default_scheme_points() {
        let s = ScoringScheme::default();
        let cases = [
            (t(2, 1), t(2, 1), 4),
            (t(2, 1), t(3, 2), 3),
            (t(2, 1), t(1, 0), 3),
            (t(2, 1), t(2, 0), 2),
            (t(2, 1), t(1, 1), 0),
            (t(1, 1), t(1, 1), 4),
            (t(1, 1), t(2, 2), 2),
            (t(0, 1), t(1, 3), 2),
            (t(0, 1), t(1, 2), 3),
            (t(0, 0), t(2, 1), 0),
        ];
        for (tip, result, expected) in cases {
            assert_eq!(s.points(tip, result), expected, "tip {tip} result {result}");
        }
    }

    #[test]
    fn community_scheme_3_2_1() {
        let pts = |tendency, gd, exact| OutcomePoints {
            tendency,
            goal_difference: Some(gd),
            exact,
        };
        let p = pts(1, 2, 3);
        let s = ScoringScheme {
            home_win: p,
            draw: p,
            away_win: p,
        };
        let cases = [
            (t(2, 1), t(3, 1), 1),
            (t(1, 0), t(2, 1), 2),
            (t(1, 1), t(2, 2), 2),
            (t(2, 1), t(2, 1), 3),
            (t(0, 0), t(1, 0), 0),
            (t(0, 1), t(1, 3), 1),
        ];
        for (tip, result, expected) in cases {
            assert_eq!(s.points(tip, result), expected, "tip {tip} result {result}");
        }
    }

    #[test]
    fn custom_draw_goal_difference() {
        let mut s = ScoringScheme::default();
        s.draw.goal_difference = Some(3);
        assert_eq!(s.points(t(1, 1), t(2, 2)), 3);
    }

    #[test]
    fn yaml_override_of_draw_keeps_other_defaults() {
        let s: ScoringScheme =
            serde_saphyr::from_str("draw: { tendency: 1, goalDifference: 2, exact: 5 }\n").unwrap();
        let d = ScoringScheme::default();
        assert_eq!(s.home_win, d.home_win);
        assert_eq!(s.away_win, d.away_win);
        assert_eq!(
            s.draw,
            OutcomePoints {
                tendency: 1,
                goal_difference: Some(2),
                exact: 5
            }
        );
    }

    #[test]
    fn tip_display() {
        assert_eq!(t(2, 1).to_string(), "2:1");
    }

    #[test]
    fn odds_validation() {
        let ok = Odds {
            home: 2.0,
            draw: 3.4,
            away: 3.6,
            over_under_2_5: Some(OverUnder {
                over: 1.9,
                under: 1.9,
            }),
        };
        assert!(ok.validate().is_ok());
        let mut bad = ok.clone();
        bad.over_under_2_5 = Some(OverUnder {
            over: 1.0,
            under: 1.9,
        });
        assert!(bad.validate().is_err());
    }
}
