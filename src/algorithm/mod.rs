//! Tip algorithms: turn odds into a ranking of candidate tips.

pub mod poisson;

use serde::Deserialize;
use thiserror::Error;

use crate::domain::{Candidate, InvalidOdds, Match, Odds, ScoringScheme};

pub use poisson::PoissonConfig;

pub trait TipAlgorithm: Send + Sync {
    fn name(&self) -> &'static str;

    /// Postcondition: non-empty, unique tips, sorted best-first (expected points
    /// descending, then fewer total goals, then more home goals).
    fn rank(
        &self,
        fixture: &Match,
        odds: &Odds,
        scoring: &ScoringScheme,
    ) -> Result<Vec<Candidate>, AlgorithmError>;
}

#[derive(Debug, Error)]
pub enum AlgorithmError {
    #[error(transparent)]
    InvalidOdds(#[from] InvalidOdds),
    #[error("could not fit goal expectations (rmse {rmse:.4})")]
    NoConvergence { rmse: f64 },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AlgorithmConfig {
    Poisson(PoissonConfig),
}

impl Default for AlgorithmConfig {
    fn default() -> Self {
        Self::Poisson(PoissonConfig::default())
    }
}

pub fn build(cfg: &AlgorithmConfig) -> Box<dyn TipAlgorithm> {
    match cfg {
        AlgorithmConfig::Poisson(p) => Box::new(poisson::PoissonAlgorithm::new(p.max_tip_goals)),
    }
}
