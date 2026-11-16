//! Tipping platforms (where matches come from and tips go to).

pub mod fake;
pub mod kicktipp;

use serde::Deserialize;
use thiserror::Error;

use crate::credentials::Credentials;
use crate::domain::{Match, OddsMap, OpenMatch, PlacedTip, TipSubmission};

pub use fake::{FakeConfig, FakePlatform};
pub use kicktipp::{KicktippConfig, KicktippPlatform};

pub trait TipPlatform: Send + Sync {
    /// Establish a session (login cookie). Idempotent. Interior state (cookie jar) lives behind &self.
    fn login(&self) -> impl Future<Output = Result<(), PlatformError>> + Send;
    /// Matches still open for tipping, each with the user's current tip if any.
    /// Platforms should return only the next matchday (kicktipp's tip page does).
    /// This is not relied upon: the runner applies the tip window (next matchday
    /// plus postponed stragglers, never later) to whatever is returned.
    fn open_matches(&self) -> impl Future<Output = Result<Vec<OpenMatch>, PlatformError>> + Send;
    /// Tips of the user (incl. open matches that already have a tip and started
    /// matches of a mixed matchday) within `scope`. Platforms may return more
    /// than asked, never less.
    fn placed_tips(
        &self,
        scope: &HistoryScope,
    ) -> impl Future<Output = Result<Vec<PlacedTip>, PlatformError>> + Send;
    /// Platform-native odds for the given matches. Missing key = none. Platforms without odds return an empty map.
    fn odds(
        &self,
        matches: &[Match],
    ) -> impl Future<Output = Result<OddsMap, PlatformError>> + Send;
    /// Set exactly these tips, leave every other tip untouched (form-based platforms must re-send existing values).
    fn submit_tips(
        &self,
        tips: &[TipSubmission],
    ) -> impl Future<Output = Result<(), PlatformError>> + Send;
}

/// Which already placed tips a platform has to return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryScope {
    /// All tips of the season.
    Season,
    /// Only tips of these matchdays.
    Matchdays(Vec<u32>),
}

#[derive(Debug, Error)]
pub enum PlatformError {
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("unexpected page: {0}")]
    UnexpectedPage(String),
    #[error("tips rejected: {0}")]
    Rejected(String),
    #[error("fake platform fixture: {0}")]
    Fixture(String),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PlatformConfig {
    Kicktipp(KicktippConfig),
    Fake(FakeConfig),
}

impl PlatformConfig {
    pub fn requires_credentials(&self) -> bool {
        match self {
            Self::Kicktipp(_) => true,
            Self::Fake(_) => false,
        }
    }
}

/// Runtime-selected platform.
pub enum Platform {
    Fake(FakePlatform),
    Kicktipp(KicktippPlatform),
}

impl Platform {
    pub async fn build(
        cfg: &PlatformConfig,
        creds: Option<Credentials>,
    ) -> Result<Platform, PlatformError> {
        match cfg {
            PlatformConfig::Fake(c) => Ok(Self::Fake(FakePlatform::from_file(&c.fixture).await?)),
            PlatformConfig::Kicktipp(c) => {
                let creds = creds.ok_or_else(|| {
                    PlatformError::Auth("kicktipp requires credentials (credentialsDir)".into())
                })?;
                Ok(Self::Kicktipp(KicktippPlatform::new(c, creds)?))
            }
        }
    }
}

impl TipPlatform for Platform {
    async fn login(&self) -> Result<(), PlatformError> {
        match self {
            Self::Fake(p) => p.login().await,
            Self::Kicktipp(p) => p.login().await,
        }
    }

    async fn open_matches(&self) -> Result<Vec<OpenMatch>, PlatformError> {
        match self {
            Self::Fake(p) => p.open_matches().await,
            Self::Kicktipp(p) => p.open_matches().await,
        }
    }

    async fn placed_tips(&self, scope: &HistoryScope) -> Result<Vec<PlacedTip>, PlatformError> {
        match self {
            Self::Fake(p) => p.placed_tips(scope).await,
            Self::Kicktipp(p) => p.placed_tips(scope).await,
        }
    }

    async fn odds(&self, matches: &[Match]) -> Result<OddsMap, PlatformError> {
        match self {
            Self::Fake(p) => p.odds(matches).await,
            Self::Kicktipp(p) => p.odds(matches).await,
        }
    }

    async fn submit_tips(&self, tips: &[TipSubmission]) -> Result<(), PlatformError> {
        match self {
            Self::Fake(p) => p.submit_tips(tips).await,
            Self::Kicktipp(p) => p.submit_tips(tips).await,
        }
    }
}
