//! Community rules that constrain the set of tips (e.g. "at most N times the
//! same result per season").

use std::collections::{BTreeMap, HashMap};

use serde::Deserialize;

use crate::domain::{Candidate, MatchId, PlacedTip, Tip};
use crate::platform::HistoryScope;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Season,
    Matchday,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MaxSameResult {
    pub max: u32,
    pub scope: Scope,
    /// Count 2:1 and 1:2 as the same result.
    #[serde(default = "yes")]
    pub mirrored: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Rules {
    pub max_same_result: Option<MaxSameResult>,
}

/// Ranked candidates of one match (non-empty, best first).
#[derive(Debug, Clone)]
pub struct MatchChoice {
    pub match_id: MatchId,
    pub matchday: u32,
    pub candidates: Vec<Candidate>,
}

#[derive(Debug, Clone)]
pub struct Assignment {
    pub match_id: MatchId,
    pub tip: Tip,
    pub expected_points: f64,
    /// Index into the match's candidate list (0 = best).
    pub rank: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub matchday: Option<u32>,
    pub result: Tip,
    pub count: u32,
    pub max: u32,
}

#[derive(Debug, Clone)]
pub struct RuleReport {
    pub assignments: Vec<Assignment>,
    pub violations: Vec<Violation>,
}

/// `(matchday for Scope::Matchday / None for Scope::Season, normalized result)`.
type Key = (Option<u32>, (u8, u8));

impl MaxSameResult {
    fn key(&self, matchday: u32, tip: Tip) -> Key {
        let md = match self.scope {
            Scope::Season => None,
            Scope::Matchday => Some(matchday),
        };
        let result = if self.mirrored {
            (tip.home.max(tip.away), tip.home.min(tip.away))
        } else {
            (tip.home, tip.away)
        };
        (md, result)
    }
}

impl Rules {
    /// Which tip history the rules need, given the matchdays of the tip
    /// window. `None` = no history needed. Any season-scoped rule needs the
    /// whole season; matchday-scoped rules only need the window's matchdays
    /// (history of other matchdays never counts for them).
    pub fn history_scope(&self, window: &[u32]) -> Option<HistoryScope> {
        // Scopes of all history-based rules (add future rules here).
        let mut scopes = self.max_same_result.iter().map(|r| r.scope).peekable();
        scopes.peek()?;
        if scopes.any(|s| s == Scope::Season) {
            return Some(HistoryScope::Season);
        }
        let mut matchdays = window.to_vec();
        matchdays.sort_unstable();
        matchdays.dedup();
        Some(HistoryScope::Matchdays(matchdays))
    }

    /// Picks one candidate per match. Without rules every match gets its best
    /// candidate. With `maxSameResult` the matches over the limit are greedily
    /// switched to the lower-ranked candidate with the smallest expected-points
    /// loss whose result still has room. Limits that cannot be met are reported
    /// as violations (the remaining picks are kept).
    pub fn apply(&self, history: &[PlacedTip], choices: &[MatchChoice]) -> RuleReport {
        let mut picks = vec![0usize; choices.len()];
        let mut violations = Vec::new();

        if let Some(rule) = self.max_same_result {
            let mut counts: HashMap<Key, u32> = HashMap::new();
            for h in history {
                *counts.entry(rule.key(h.matchday, h.tip)).or_default() += 1;
            }
            for c in choices {
                if let Some(first) = c.candidates.first() {
                    *counts.entry(rule.key(c.matchday, first.tip)).or_default() += 1;
                }
            }
            let count = |counts: &HashMap<Key, u32>, k: &Key| counts.get(k).copied().unwrap_or(0);

            loop {
                // (loss, match index, candidate index)
                let mut best: Option<(f64, usize, usize)> = None;
                for (i, (c, &pick)) in choices.iter().zip(&picks).enumerate() {
                    let Some(current) = c.candidates.get(pick) else {
                        continue;
                    };
                    if count(&counts, &rule.key(c.matchday, current.tip)) <= rule.max {
                        continue;
                    }
                    let next = c
                        .candidates
                        .iter()
                        .enumerate()
                        .skip(pick + 1)
                        .find(|(_, cand)| {
                            count(&counts, &rule.key(c.matchday, cand.tip)) < rule.max
                        });
                    if let Some((j, cand)) = next {
                        let loss = current.expected_points - cand.expected_points;
                        if best.is_none_or(|(l, _, _)| loss < l) {
                            best = Some((loss, i, j));
                        }
                    }
                }
                let Some((_, i, j)) = best else { break };
                let (Some(c), Some(pick)) = (choices.get(i), picks.get_mut(i)) else {
                    break;
                };
                if let (Some(old), Some(new)) = (c.candidates.get(*pick), c.candidates.get(j)) {
                    if let Some(n) = counts.get_mut(&rule.key(c.matchday, old.tip)) {
                        *n = n.saturating_sub(1);
                    }
                    *counts.entry(rule.key(c.matchday, new.tip)).or_default() += 1;
                }
                *pick = j;
            }

            let over: BTreeMap<Key, u32> =
                counts.into_iter().filter(|&(_, n)| n > rule.max).collect();
            violations = over
                .into_iter()
                .map(|((matchday, (home, away)), count)| Violation {
                    matchday,
                    result: Tip { home, away },
                    count,
                    max: rule.max,
                })
                .collect();
        }

        let assignments = choices
            .iter()
            .zip(picks)
            .filter_map(|(c, rank)| {
                c.candidates.get(rank).map(|cand| Assignment {
                    match_id: c.match_id.clone(),
                    tip: cand.tip,
                    expected_points: cand.expected_points,
                    rank,
                })
            })
            .collect();
        RuleReport {
            assignments,
            violations,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tip(s: &str) -> Tip {
        let (h, a) = s.split_once(':').unwrap();
        Tip {
            home: h.parse().unwrap(),
            away: a.parse().unwrap(),
        }
    }

    fn choice(id: &str, matchday: u32, cands: &[(&str, f64)]) -> MatchChoice {
        MatchChoice {
            match_id: MatchId::new(id),
            matchday,
            candidates: cands
                .iter()
                .map(|&(t, ep)| Candidate {
                    tip: tip(t),
                    expected_points: ep,
                })
                .collect(),
        }
    }

    fn placed(matchday: u32, id: &str, t: &str) -> PlacedTip {
        PlacedTip {
            matchday,
            match_id: MatchId::new(id),
            tip: tip(t),
        }
    }

    fn rules(max: u32, scope: Scope, mirrored: bool) -> Rules {
        Rules {
            max_same_result: Some(MaxSameResult {
                max,
                scope,
                mirrored,
            }),
        }
    }

    fn tips(r: &RuleReport) -> Vec<String> {
        r.assignments.iter().map(|a| a.tip.to_string()).collect()
    }

    fn total(r: &RuleReport) -> f64 {
        r.assignments.iter().map(|a| a.expected_points).sum()
    }

    fn fixtures_a() -> Vec<MatchChoice> {
        vec![
            choice("m1", 1, &[("2:1", 1.50), ("1:0", 1.40), ("0:0", 0.8)]),
            choice("m2", 1, &[("2:1", 1.50), ("1:0", 1.20), ("0:0", 0.5)]),
            choice("m3", 1, &[("1:2", 1.60), ("0:1", 1.55), ("0:0", 0.9)]),
            choice("m4", 1, &[("1:2", 1.40), ("0:1", 1.10), ("0:0", 0.6)]),
        ]
    }

    #[test]
    fn a_mirrored_season() {
        let r = rules(2, Scope::Season, true).apply(&[], &fixtures_a());
        assert_eq!(tips(&r), ["1:0", "2:1", "0:1", "1:2"]);
        assert!((total(&r) - 5.85).abs() < 1e-9);
        assert!(r.violations.is_empty());
        assert_eq!(r.assignments[0].rank, 1);
    }

    #[test]
    fn a_not_mirrored_unchanged() {
        let r = rules(2, Scope::Season, false).apply(&[], &fixtures_a());
        assert_eq!(tips(&r), ["2:1", "2:1", "1:2", "1:2"]);
    }

    #[test]
    fn a_brute_force_oracle() {
        let rule = rules(2, Scope::Season, true);
        let fx = fixtures_a();
        let m = rule.max_same_result.unwrap();
        let mut best = f64::NEG_INFINITY;
        for code in 0..3usize.pow(4) {
            let picks: Vec<usize> = (0..4).map(|i| (code / 3usize.pow(i)) % 3).collect();
            let mut counts: HashMap<Key, u32> = HashMap::new();
            let mut sum = 0.0;
            for (c, &p) in fx.iter().zip(&picks) {
                let cand = c.candidates[p];
                *counts.entry(m.key(c.matchday, cand.tip)).or_default() += 1;
                sum += cand.expected_points;
            }
            if counts.values().all(|&n| n <= m.max) && sum > best {
                best = sum;
            }
        }
        let greedy = total(&rule.apply(&[], &fx));
        assert!((best - 5.85).abs() < 1e-9);
        assert!((greedy - best).abs() < 1e-9);
    }

    fn history_b(matchdays: [u32; 5]) -> Vec<PlacedTip> {
        let results = ["2:1", "1:2", "2:1", "1:2", "2:1"];
        matchdays
            .iter()
            .zip(results)
            .enumerate()
            .map(|(i, (&md, t))| placed(md, &format!("h{i}"), t))
            .collect()
    }

    #[test]
    fn b_history_counts() {
        let history = history_b([1, 2, 3, 4, 5]);
        let fx = vec![
            choice("m1", 6, &[("2:1", 1.5), ("1:0", 1.4)]),
            choice("m2", 6, &[("2:1", 1.5), ("1:0", 1.2)]),
        ];
        let r = rules(6, Scope::Season, true).apply(&history, &fx);
        assert_eq!(tips(&r), ["1:0", "2:1"]);
        assert!(r.violations.is_empty());
    }

    #[test]
    fn c_scope_matchday() {
        let history = history_b([1; 5]);
        let fx2 = vec![
            choice("m1", 2, &[("2:1", 1.5), ("1:0", 1.4)]),
            choice("m2", 2, &[("2:1", 1.5), ("1:0", 1.2)]),
        ];
        let r = rules(6, Scope::Matchday, true).apply(&history, &fx2);
        assert_eq!(tips(&r), ["2:1", "2:1"]);
        let r = rules(6, Scope::Season, true).apply(&history, &fx2);
        assert_eq!(tips(&r), ["1:0", "2:1"]);

        let fx1 = vec![
            choice("m1", 1, &[("2:1", 1.5), ("1:0", 1.4)]),
            choice("m2", 1, &[("2:1", 1.5), ("1:0", 1.2)]),
        ];
        let r = rules(6, Scope::Matchday, true).apply(&history, &fx1);
        assert_eq!(tips(&r), ["1:0", "2:1"]);
    }

    #[test]
    fn d_unsatisfiable_is_violation() {
        let history = [placed(1, "h", "2:1")];
        let fx = vec![choice("m1", 2, &[("2:1", 1.5)])];
        let r = rules(1, Scope::Season, true).apply(&history, &fx);
        assert_eq!(tips(&r), ["2:1"]);
        assert_eq!(
            r.violations,
            [Violation {
                matchday: None,
                result: tip("2:1"),
                count: 2,
                max: 1
            }]
        );
    }

    #[test]
    fn e_skips_full_candidate() {
        let history = [placed(1, "h", "1:0")];
        let fx = vec![
            choice("m1", 2, &[("2:1", 1.5), ("1:0", 1.4), ("0:0", 1.0)]),
            choice("m2", 2, &[("2:1", 1.5), ("1:0", 1.45), ("0:0", 0.9)]),
        ];
        let r = rules(1, Scope::Season, true).apply(&history, &fx);
        assert_eq!(tips(&r), ["0:0", "2:1"]);
        assert!(r.violations.is_empty());
    }

    #[test]
    fn history_scope_follows_rule_scope() {
        assert_eq!(Rules::default().history_scope(&[12, 10, 10]), None);
        assert_eq!(
            rules(6, Scope::Matchday, true).history_scope(&[12, 10, 10]),
            Some(HistoryScope::Matchdays(vec![10, 12]))
        );
        assert_eq!(
            rules(6, Scope::Season, true).history_scope(&[12, 10, 10]),
            Some(HistoryScope::Season)
        );
    }

    #[test]
    fn f_no_rules_takes_top() {
        let rules = Rules::default();
        assert_eq!(rules.history_scope(&[12]), None);
        let history = history_b([1; 5]);
        let r = rules.apply(&history, &fixtures_a());
        assert_eq!(tips(&r), ["2:1", "2:1", "1:2", "1:2"]);
        assert!(r.assignments.iter().all(|a| a.rank == 0));
    }
}
