//! Odds providers.

use serde::Deserialize;
use thiserror::Error;

use crate::domain::Match;
use crate::platform::{Platform, PlatformError, TipPlatform};

pub use crate::domain::OddsMap;

pub trait OddsProvider: Send + Sync {
    /// Odds for the given matches. A missing key means no odds for that match.
    fn odds(&self, matches: &[Match]) -> impl Future<Output = Result<OddsMap, OddsError>> + Send;
}

#[derive(Debug, Error)]
pub enum OddsError {
    #[error(transparent)]
    Platform(#[from] PlatformError),
    #[expect(dead_code, reason = "used by external odds providers later")]
    #[error("odds unavailable: {0}")]
    Unavailable(String),
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum OddsConfig {
    /// Odds shown by the tipping platform itself.
    #[default]
    Platform,
}

/// Odds taken from the tipping platform.
pub struct PlatformOdds<'a, P: TipPlatform>(pub &'a P);

impl<P: TipPlatform> OddsProvider for PlatformOdds<'_, P> {
    async fn odds(&self, matches: &[Match]) -> Result<OddsMap, OddsError> {
        Ok(self.0.odds(matches).await?)
    }
}

/// Runtime-selected odds provider.
pub enum OddsSource<'a> {
    Platform(PlatformOdds<'a, Platform>),
}

impl<'a> OddsSource<'a> {
    pub fn build(cfg: &OddsConfig, platform: &'a Platform) -> Self {
        match cfg {
            OddsConfig::Platform => Self::Platform(PlatformOdds(platform)),
        }
    }
}

impl OddsProvider for OddsSource<'_> {
    async fn odds(&self, matches: &[Match]) -> Result<OddsMap, OddsError> {
        match self {
            Self::Platform(p) => p.odds(matches).await,
        }
    }
}
