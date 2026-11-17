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
    /// Optional deterministic variation among near-best tips.
    pub variation: Option<VariationConfig>,
}

impl Default for PoissonConfig {
    fn default() -> Self {
        Self {
            max_tip_goals: 5,
            variation: None,
        }
    }
}

/// Picks uniformly (but deterministically, from `seed` and the match
/// identity) among all tips whose expected points are within `tolerance` of
/// the best, so tenants with different seeds get different tips.
/// `tolerance` 0 = no variation.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VariationConfig {
    pub seed: i64,
    /// Maximum expected-points loss against the best tip.
    pub tolerance: f64,
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

/// FNV-1a, 64 bit. Hand-rolled because std hashers are not stable across
/// Rust versions, and the variation pick must not change with the toolchain.
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    bytes
        .iter()
        .fold(OFFSET, |h, &b| (h ^ u64::from(b)).wrapping_mul(PRIME))
}

/// splitmix64 finalizer: spreads FNV's weak low bits over the whole word.
fn splitmix64_mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Stable hash of `(seed, home team, away team)`. The match identity is the
/// pair of team names, not the platform's match id or the matchday, so the
/// pick does not depend on platform ids.
fn variation_hash(seed: i64, home: &str, away: &str) -> u64 {
    let mut bytes = Vec::with_capacity(10 + home.len() + away.len());
    bytes.extend_from_slice(&seed.to_le_bytes());
    bytes.push(0xFF);
    bytes.extend_from_slice(home.as_bytes());
    bytes.push(0xFF);
    bytes.extend_from_slice(away.as_bytes());
    splitmix64_mix(fnv1a64(&bytes))
}

/// Moves the variation pick (uniformly chosen among the candidates within
/// `tolerance` of the best) to the front; the rest keeps its EP order.
///
/// Known limitation: if the pick hits a rule limit, the rules step tries the
/// following candidates in order, which starts with the pure best at zero or
/// negative loss.
fn vary(candidates: &mut Vec<Candidate>, v: VariationConfig, fixture: &Match) {
    if v.tolerance <= 0.0 {
        return;
    }
    let Some(best) = candidates.first().copied() else {
        return;
    };
    let eligible: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| best.expected_points - c.expected_points <= v.tolerance)
        .map(|(i, _)| i)
        .collect();
    let h = variation_hash(v.seed, &fixture.home, &fixture.away);
    // `eligible` is non-empty (it contains index 0) and `h % len < len`.
    let Some(&idx) = usize::try_from(h % eligible.len() as u64)
        .ok()
        .and_then(|i| eligible.get(i))
    else {
        return;
    };
    if idx == 0 {
        return;
    }
    let picked = candidates.remove(idx);
    candidates.insert(0, picked);
    tracing::info!(
        match_id = %fixture.id,
        home = %fixture.home,
        away = %fixture.away,
        best = %best.tip,
        picked = %picked.tip,
        ep_loss = format_args!("{:.3}", best.expected_points - picked.expected_points),
        eligible = eligible.len(),
        "variation changed the pick"
    );
}

pub struct PoissonAlgorithm {
    max_tip_goals: u8,
    variation: Option<VariationConfig>,
}

impl PoissonAlgorithm {
    pub fn new(max_tip_goals: u8) -> Self {
        Self {
            max_tip_goals,
            variation: None,
        }
    }

    pub fn with_variation(mut self, variation: Option<VariationConfig>) -> Self {
        self.variation = variation;
        self
    }
}

impl TipAlgorithm for PoissonAlgorithm {
    fn name(&self) -> &'static str {
        "poisson"
    }

    fn rank(
        &self,
        fixture: &Match,
        odds: &Odds,
        scoring: &ScoringScheme,
    ) -> Result<Vec<Candidate>, AlgorithmError> {
        let (lh, la) = fit(odds)?;
        let m = ScoreMatrix::poisson(lh, la);
        let mut candidates = rank_tips(&m, scoring, self.max_tip_goals);
        if let Some(v) = self.variation {
            vary(&mut candidates, v, fixture);
        }
        Ok(candidates)
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

    fn fixture(home: &str, away: &str) -> Match {
        Match {
            id: crate::domain::MatchId::new("m"),
            home: home.into(),
            away: away.into(),
            kickoff: jiff::Timestamp::UNIX_EPOCH,
            matchday: 1,
            competition: "X".into(),
        }
    }

    fn varied(seed: i64, tolerance: f64) -> PoissonAlgorithm {
        PoissonAlgorithm::new(5).with_variation(Some(VariationConfig { seed, tolerance }))
    }

    #[test]
    fn hash_known_answers() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
        assert_eq!(splitmix64_mix(0x9e37_79b9_7f4a_7c15), 0xe220_a839_7b1d_cdaf);
        // Pinned: changing this changes every tenant's picks.
        assert_eq!(variation_hash(7, "FC A", "FC B"), PINNED_VARIATION_HASH);
    }

    const PINNED_VARIATION_HASH: u64 = 0x6b81_b05f_3415_e444;

    #[test]
    fn variation_is_deterministic() {
        let o = odds(2.6, 3.4, 2.6);
        let s = ScoringScheme::default();
        let f = fixture("FC A", "FC B");
        let a = varied(7, 0.2).rank(&f, &o, &s).unwrap();
        let b = varied(7, 0.2).rank(&f, &o, &s).unwrap();
        let ta: Vec<Tip> = a.iter().map(|c| c.tip).collect();
        let tb: Vec<Tip> = b.iter().map(|c| c.tip).collect();
        assert_eq!(ta, tb);
    }

    #[test]
    fn zero_tolerance_equals_no_variation() {
        let s = ScoringScheme::default();
        let f = fixture("FC A", "FC B");
        for o in [odds(2.6, 3.4, 2.6), odds(1.5, 4.2, 6.5)] {
            let plain: Vec<Tip> = PoissonAlgorithm::new(5)
                .rank(&f, &o, &s)
                .unwrap()
                .iter()
                .map(|c| c.tip)
                .collect();
            for seed in [0, 1, -5, 7, 1000] {
                let v: Vec<Tip> = varied(seed, 0.0)
                    .rank(&f, &o, &s)
                    .unwrap()
                    .iter()
                    .map(|c| c.tip)
                    .collect();
                assert_eq!(v, plain, "seed {seed}");
            }
        }
    }

    #[test]
    fn variation_stays_within_tolerance() {
        let s = ScoringScheme::default();
        let f = fixture("FC A", "FC B");
        let tolerance = 0.1;
        for lh in [0.6, 1.0, 1.4, 1.9, 2.6] {
            for la in [0.5, 0.9, 1.3, 2.0] {
                let o = fair_odds(lh, la, false);
                let plain = PoissonAlgorithm::new(5).rank(&f, &o, &s).unwrap();
                let best = plain[0].expected_points;
                for seed in [0, 1, -5, 7, 1000] {
                    let c = varied(seed, tolerance).rank(&f, &o, &s).unwrap();
                    assert_eq!(c.len(), plain.len());
                    let unique: HashSet<Tip> = c.iter().map(|c| c.tip).collect();
                    assert_eq!(unique.len(), c.len());
                    assert!(best - c[0].expected_points <= tolerance + 1e-12);
                    assert!(
                        c[1..]
                            .windows(2)
                            .all(|w| w[0].expected_points >= w[1].expected_points - 1e-9)
                    );
                }
            }
        }
    }

    #[test]
    fn seeds_produce_different_picks() {
        let s = ScoringScheme::default();
        let o = odds(2.6, 3.4, 2.6);
        let tolerance = 0.2;
        let plain = PoissonAlgorithm::new(5)
            .rank(&fixture("FC A", "FC B"), &o, &s)
            .unwrap();
        let eligible = plain
            .iter()
            .filter(|c| plain[0].expected_points - c.expected_points <= tolerance)
            .count();
        assert!(eligible >= 2, "test premise: {eligible} eligible tips");

        let f = fixture("FC A", "FC B");
        let picks: HashSet<Tip> = (0..20)
            .map(|seed| varied(seed, tolerance).rank(&f, &o, &s).unwrap()[0].tip)
            .collect();
        assert!(picks.len() >= 2, "{picks:?}");

        let picks = |seed| -> Vec<Tip> {
            (0..30)
                .map(|i| {
                    let f = fixture(&format!("T{i}"), &format!("T{}", i + 100));
                    varied(seed, tolerance).rank(&f, &o, &s).unwrap()[0].tip
                })
                .collect()
        };
        assert_ne!(picks(1), picks(2));
    }

    #[test]
    fn strong_favourite_ignores_seed_with_small_tolerance() {
        let s = ScoringScheme::default();
        let o = odds(1.20, 7.0, 15.0);
        let f = fixture("FC A", "FC B");
        let plain = PoissonAlgorithm::new(5).rank(&f, &o, &s).unwrap()[0].tip;
        for seed in 0..20 {
            assert_eq!(varied(seed, 0.001).rank(&f, &o, &s).unwrap()[0].tip, plain);
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
