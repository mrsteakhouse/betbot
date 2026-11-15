//! Independent-Poisson model: fit goal expectations to the odds, then rank
//! every tip by its expected points under the scoring scheme.

use serde::Deserialize;

use super::{AlgorithmError, TipAlgorithm};
use crate::domain::{Candidate, Match, Odds, OverUnder, ScoringScheme, Tip};

/// Highest number of goals per team covered by the score matrix.
pub const MAX_GOALS: usize = 10;
const N: usize = MAX_GOALS + 1;

const LAMBDA_MIN: f64 = 0.02;
const LAMBDA_MAX: f64 = 6.0;
const START_TOTAL: f64 = 2.7;
const MAX_ITER: usize = 400;
const LOSS_TOL: f64 = 1e-14;
const MAX_RMSE: f64 = 0.02;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct PoissonConfig {
    pub max_tip_goals: u8,
}

impl Default for PoissonConfig {
    fn default() -> Self {
        Self { max_tip_goals: 5 }
    }
}

/// Probability of each exact result, indexed `[home][away]`, summing to 1.
#[derive(Debug, Clone)]
pub struct ScoreMatrix {
    p: [[f64; N]; N],
}

fn pmf_row(lambda: f64) -> [f64; N] {
    let mut row = [0.0; N];
    let mut p = (-lambda).exp();
    for (k, slot) in row.iter_mut().enumerate() {
        if k > 0 {
            p *= lambda / k as f64;
        }
        *slot = p;
    }
    row
}

impl ScoreMatrix {
    /// Outer product of two Poisson pmfs (goals 0..=MAX_GOALS), renormalized.
    pub fn poisson(lambda_home: f64, lambda_away: f64) -> Self {
        let rh = pmf_row(lambda_home);
        let ra = pmf_row(lambda_away);
        let mut p = [[0.0; N]; N];
        let mut total = 0.0;
        for (row, ph) in p.iter_mut().zip(rh) {
            for (cell, pa) in row.iter_mut().zip(ra) {
                *cell = ph * pa;
                total += *cell;
            }
        }
        if total > 0.0 {
            for cell in p.iter_mut().flatten() {
                *cell /= total;
            }
        }
        Self { p }
    }

    #[cfg(test)]
    pub fn point_mass(home: usize, away: usize) -> Self {
        let mut p = [[0.0; N]; N];
        p[home][away] = 1.0;
        Self { p }
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used by the kicktipp client in the next step")
    )]
    pub fn prob(&self, home: usize, away: usize) -> f64 {
        self.p
            .get(home)
            .and_then(|row| row.get(away))
            .copied()
            .unwrap_or(0.0)
    }

    /// All cells as `(home goals, away goals, probability)`.
    fn cells(&self) -> impl Iterator<Item = (usize, usize, f64)> + '_ {
        self.p
            .iter()
            .enumerate()
            .flat_map(|(h, row)| row.iter().enumerate().map(move |(a, &p)| (h, a, p)))
    }

    /// `[P(home win), P(draw), P(away win)]`.
    pub fn outcome_probs(&self) -> [f64; 3] {
        let mut out = [0.0; 3];
        for (h, a, p) in self.cells() {
            let slot = match h.cmp(&a) {
                std::cmp::Ordering::Greater => &mut out[0],
                std::cmp::Ordering::Equal => &mut out[1],
                std::cmp::Ordering::Less => &mut out[2],
            };
            *slot += p;
        }
        out
    }

    pub fn prob_total_at_least(&self, goals: usize) -> f64 {
        self.cells()
            .filter(|&(h, a, _)| h + a >= goals)
            .map(|(_, _, p)| p)
            .sum()
    }
}

/// Removes the bookmaker margin proportionally: `p_i = (1/o_i) / Σ(1/o_j)`.
pub fn remove_margin(odds: &Odds) -> [f64; 3] {
    let inv = [1.0 / odds.home, 1.0 / odds.draw, 1.0 / odds.away];
    let sum: f64 = inv.iter().sum();
    inv.map(|x| x / sum)
}

/// Margin-free probability of over 2.5 goals.
pub fn two_way_over(ou: &OverUnder) -> f64 {
    ou.under / (ou.over + ou.under)
}

fn lambdas(x: [f64; 2]) -> (f64, f64) {
    let [lh, la] = x;
    (
        lh.exp().clamp(LAMBDA_MIN, LAMBDA_MAX),
        la.exp().clamp(LAMBDA_MIN, LAMBDA_MAX),
    )
}

/// Fits `(λ_home, λ_away)` of independent Poisson goal counts to the odds.
pub fn fit(odds: &Odds) -> Result<(f64, f64), AlgorithmError> {
    odds.validate()?;
    let target = remove_margin(odds);
    let over = odds.over_under_2_5.as_ref().map(two_way_over);
    let n_targets = if over.is_some() { 4.0 } else { 3.0 };

    let loss = |x: [f64; 2]| {
        let (lh, la) = lambdas(x);
        let m = ScoreMatrix::poisson(lh, la);
        let mut l: f64 = m
            .outcome_probs()
            .iter()
            .zip(target)
            .map(|(model, t)| (model - t).powi(2))
            .sum();
        if let Some(p_over) = over {
            l += (m.prob_total_at_least(3) - p_over).powi(2);
        }
        l
    };

    let [p_home, _, p_away] = target;
    let share = (p_home / (p_home + p_away)).clamp(0.05, 0.95);
    let start = [
        (START_TOTAL * share).ln(),
        (START_TOTAL * (1.0 - share)).ln(),
    ];
    let (x, _) = nelder_mead_2d(loss, start, 0.3, MAX_ITER, LOSS_TOL);
    let (x, best) = nelder_mead_2d(loss, x, 0.03, MAX_ITER, LOSS_TOL);

    let rmse = (best / n_targets).sqrt();
    if rmse.is_nan() || rmse > MAX_RMSE {
        return Err(AlgorithmError::NoConvergence { rmse });
    }
    Ok(lambdas(x))
}

fn add(a: [f64; 2], b: [f64; 2]) -> [f64; 2] {
    [a[0] + b[0], a[1] + b[1]]
}

fn sub(a: [f64; 2], b: [f64; 2]) -> [f64; 2] {
    [a[0] - b[0], a[1] - b[1]]
}

fn scale(a: [f64; 2], s: f64) -> [f64; 2] {
    [a[0] * s, a[1] * s]
}

/// Minimal Nelder–Mead for two parameters. Returns the best point and its value.
/// Stops after `max_iter` iterations or once the best value is below `f_tol`.
fn nelder_mead_2d(
    f: impl Fn([f64; 2]) -> f64,
    start: [f64; 2],
    step: f64,
    max_iter: usize,
    f_tol: f64,
) -> ([f64; 2], f64) {
    const REFLECT: f64 = 1.0;
    const EXPAND: f64 = 2.0;
    const CONTRACT: f64 = 0.5;
    const SHRINK: f64 = 0.5;

    let eval = |x: [f64; 2]| (x, f(x));
    let mut simplex = [
        eval(start),
        eval(add(start, [step, 0.0])),
        eval(add(start, [0.0, step])),
    ];

    for _ in 0..max_iter {
        simplex.sort_by(|a, b| a.1.total_cmp(&b.1));
        let [best, mid, worst] = simplex;
        if best.1 < f_tol {
            break;
        }
        let centroid = scale(add(best.0, mid.0), 0.5);
        let reflected = eval(add(centroid, scale(sub(centroid, worst.0), REFLECT)));

        if reflected.1 < best.1 {
            let expanded = eval(add(centroid, scale(sub(reflected.0, centroid), EXPAND)));
            simplex = if expanded.1 < reflected.1 {
                [best, mid, expanded]
            } else {
                [best, mid, reflected]
            };
            continue;
        }
        if reflected.1 < mid.1 {
            simplex = [best, mid, reflected];
            continue;
        }
        let (contracted, reference) = if reflected.1 < worst.1 {
            // Outside contraction.
            (
                eval(add(centroid, scale(sub(reflected.0, centroid), CONTRACT))),
                reflected.1,
            )
        } else {
            // Inside contraction.
            (
                eval(add(centroid, scale(sub(worst.0, centroid), CONTRACT))),
                worst.1,
            )
        };
        if contracted.1 < reference {
            simplex = [best, mid, contracted];
            continue;
        }
        // Shrink towards the best point.
        let shrink = |p: [f64; 2]| eval(add(best.0, scale(sub(p, best.0), SHRINK)));
        simplex = [best, shrink(mid.0), shrink(worst.0)];
    }
    simplex.sort_by(|a, b| a.1.total_cmp(&b.1));
    let [best, _, _] = simplex;
    best
}

/// Ranks all tips with `0..=max_tip_goals` goals per side by expected points,
/// tie-broken by fewer total goals, then more home goals.
pub fn rank_tips(m: &ScoreMatrix, scoring: &ScoringScheme, max_tip_goals: u8) -> Vec<Candidate> {
    // MAX_GOALS is 10, so the conversion never fails.
    let max_tip_goals = max_tip_goals.min(u8::try_from(MAX_GOALS).unwrap_or(u8::MAX));
    let mut candidates: Vec<Candidate> = (0..=max_tip_goals)
        .flat_map(|home| (0..=max_tip_goals).map(move |away| Tip { home, away }))
        .map(|tip| {
            let expected_points = m
                .cells()
                .filter(|&(_, _, p)| p > 0.0)
                .map(|(h, a, p)| {
                    // Cell indices are at most MAX_GOALS (10) and fit into u8.
                    let result = Tip {
                        home: h as u8,
                        away: a as u8,
                    };
                    p * f64::from(scoring.points(tip, result))
                })
                .sum();
            Candidate {
                tip,
                expected_points,
            }
        })
        .collect();
    candidates.sort_by(|a, b| {
        let ra = (a.expected_points * 1e9).round();
        let rb = (b.expected_points * 1e9).round();
        rb.total_cmp(&ra)
            .then_with(|| {
                (u16::from(a.tip.home) + u16::from(a.tip.away))
                    .cmp(&(u16::from(b.tip.home) + u16::from(b.tip.away)))
            })
            .then_with(|| b.tip.home.cmp(&a.tip.home))
    });
    candidates
}

pub struct PoissonAlgorithm {
    max_tip_goals: u8,
}

impl PoissonAlgorithm {
    pub fn new(max_tip_goals: u8) -> Self {
        Self { max_tip_goals }
    }
}

impl TipAlgorithm for PoissonAlgorithm {
    fn name(&self) -> &'static str {
        "poisson"
    }

    fn rank(
        &self,
        _fixture: &Match,
        odds: &Odds,
        scoring: &ScoringScheme,
    ) -> Result<Vec<Candidate>, AlgorithmError> {
        let (lh, la) = fit(odds)?;
        let m = ScoreMatrix::poisson(lh, la);
        Ok(rank_tips(&m, scoring, self.max_tip_goals))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn odds(home: f64, draw: f64, away: f64) -> Odds {
        Odds {
            home,
            draw,
            away,
            over_under_2_5: None,
        }
    }

    fn ep(c: &[Candidate], home: u8, away: u8) -> f64 {
        c.iter()
            .find(|c| c.tip == Tip { home, away })
            .unwrap()
            .expected_points
    }

    fn fair_odds(lh: f64, la: f64, with_ou: bool) -> Odds {
        let m = ScoreMatrix::poisson(lh, la);
        let [h, d, a] = m.outcome_probs();
        let over = m.prob_total_at_least(3);
        Odds {
            home: 1.0 / h,
            draw: 1.0 / d,
            away: 1.0 / a,
            over_under_2_5: with_ou.then(|| OverUnder {
                over: 1.0 / over,
                under: 1.0 / (1.0 - over),
            }),
        }
    }

    #[test]
    fn matrix_basics() {
        let m = ScoreMatrix::poisson(1.0, 1.0);
        let total: f64 = m.cells().map(|(_, _, p)| p).sum();
        assert!((total - 1.0).abs() < 1e-12);
        assert!((m.prob(0, 0) - 0.135335).abs() < 1e-4);
        let [h, _, a] = m.outcome_probs();
        assert!((h - a).abs() < 1e-12);
    }

    #[test]
    fn margin_removal() {
        let p = remove_margin(&odds(2.0, 4.0, 4.0));
        assert!((p[0] - 0.5).abs() < 1e-12);
        assert!((p[1] - 0.25).abs() < 1e-12);
        assert!((p[2] - 0.25).abs() < 1e-12);

        let p = remove_margin(&odds(1.9, 3.5, 4.2));
        assert!((p[0] - 0.50119).abs() < 1e-4);
        assert!((p[1] - 0.27208).abs() < 1e-4);
        assert!((p[2] - 0.22673).abs() < 1e-4);
        assert!((p.iter().sum::<f64>() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn two_way_over_probability() {
        let p = two_way_over(&OverUnder {
            over: 1.8,
            under: 2.0,
        });
        assert!((p - 0.526316).abs() < 1e-6);
    }

    #[test]
    fn fit_round_trip() {
        for (lh, la) in [(1.6, 1.1), (0.8, 1.9)] {
            for with_ou in [false, true] {
                let (fh, fa) = fit(&fair_odds(lh, la, with_ou)).unwrap();
                assert!((fh - lh).abs() < 2e-3, "{lh} {la} {with_ou}: {fh}");
                assert!((fa - la).abs() < 2e-3, "{lh} {la} {with_ou}: {fa}");
            }
        }
    }

    #[test]
    fn symmetric_odds_give_symmetric_fit() {
        let o = odds(2.6, 3.4, 2.6);
        let (lh, la) = fit(&o).unwrap();
        assert!((lh - la).abs() < 1e-3);
        let alg = PoissonAlgorithm::new(5);
        let c = rank_tips(
            &ScoreMatrix::poisson(lh, la),
            &ScoringScheme::default(),
            alg.max_tip_goals,
        );
        for a in 0..=5 {
            for b in 0..=5 {
                assert!((ep(&c, a, b) - ep(&c, b, a)).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn strong_favourite() {
        let o = odds(1.20, 7.0, 15.0);
        let (lh, la) = fit(&o).unwrap();
        assert!(lh > la);
        let fixture = Match {
            id: crate::domain::MatchId::new("m"),
            home: "A".into(),
            away: "B".into(),
            kickoff: jiff::Timestamp::UNIX_EPOCH,
            matchday: 1,
            competition: "X".into(),
        };
        let alg = PoissonAlgorithm::new(5);
        assert_eq!(alg.name(), "poisson");
        let c = alg.rank(&fixture, &o, &ScoringScheme::default()).unwrap();
        let best = c[0].tip;
        assert!(best.home >= 2 && best.away <= 1, "{best}");
    }

    #[test]
    fn over_under_pins_total() {
        let mut high = odds(2.4, 3.6, 3.0);
        high.over_under_2_5 = Some(OverUnder {
            over: 1.6,
            under: 2.3,
        });
        let mut low = odds(2.4, 3.6, 3.0);
        low.over_under_2_5 = Some(OverUnder {
            over: 2.4,
            under: 1.6,
        });
        let (h1, a1) = fit(&high).unwrap();
        let (h2, a2) = fit(&low).unwrap();
        assert!((h1 + a1) - (h2 + a2) > 0.5, "{} vs {}", h1 + a1, h2 + a2);
    }

    #[test]
    fn rank_point_mass() {
        let s = ScoringScheme::default();
        let c = rank_tips(&ScoreMatrix::point_mass(2, 1), &s, 5);
        assert_eq!(ep(&c, 2, 1), 4.0);
        assert_eq!(ep(&c, 3, 2), 3.0);
        assert_eq!(ep(&c, 1, 0), 3.0);
        assert_eq!(ep(&c, 2, 0), 2.0);
        assert_eq!(ep(&c, 1, 1), 0.0);
        assert_eq!(c[0].tip, Tip { home: 2, away: 1 });

        let c = rank_tips(&ScoreMatrix::point_mass(1, 1), &s, 5);
        assert_eq!(ep(&c, 1, 1), 4.0);
        assert_eq!(ep(&c, 0, 0), 2.0);
        assert_eq!(ep(&c, 2, 1), 0.0);
    }

    #[test]
    fn ranking_shape_and_tie_break() {
        let s = ScoringScheme::default();
        let c = rank_tips(&ScoreMatrix::poisson(1.4, 1.1), &s, 5);
        assert_eq!(c.len(), 36);
        assert!(
            c.windows(2)
                .all(|w| w[0].expected_points >= w[1].expected_points - 1e-9)
        );
        let unique: HashSet<Tip> = c.iter().map(|c| c.tip).collect();
        assert_eq!(unique.len(), 36);

        // point_mass(2,1): 3:2 and 1:0 both score 3 -> fewer goals first.
        let c = rank_tips(&ScoreMatrix::point_mass(2, 1), &s, 5);
        let pos = |h, a| {
            c.iter()
                .position(|c| c.tip == Tip { home: h, away: a })
                .unwrap()
        };
        assert!(pos(1, 0) < pos(3, 2));
        // All zero-point tips with equal total: higher home goals first (1:1 before 0:2).
        assert!(pos(1, 1) < pos(0, 2));
    }

    #[test]
    fn invalid_odds_are_rejected() {
        for o in [
            odds(1.0, 3.0, 3.0),
            odds(f64::NAN, 3.0, 3.0),
            odds(3.0, 4.0, 4.0),
        ] {
            assert!(matches!(fit(&o), Err(AlgorithmError::InvalidOdds(_))));
        }
    }

    #[test]
    fn nelder_mead_quadratic() {
        let (x, _) = nelder_mead_2d(
            |[x, y]| (x - 1.0).powi(2) + (y + 2.0).powi(2),
            [0.0, 0.0],
            0.3,
            400,
            1e-14,
        );
        assert!((x[0] - 1.0).abs() < 1e-6, "{x:?}");
        assert!((x[1] + 2.0).abs() < 1e-6, "{x:?}");
    }
}
