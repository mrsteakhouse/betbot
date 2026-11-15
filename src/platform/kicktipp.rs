//! kicktipp.de platform. The HTTP client follows in the next step; for now
//! every operation reports `NotImplemented`.

use serde::Deserialize;
use tracing::info;

use super::{PlatformError, TipPlatform};
use crate::credentials::Credentials;
use crate::domain::{Match, OddsMap, OpenMatch, PlacedTip, TipSubmission};

const NOT_IMPLEMENTED: PlatformError = PlatformError::NotImplemented("kicktipp client");

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KicktippConfig {
    /// Community (Tipprunde) slug as in `https://www.kicktipp.de/<community>/`.
    pub community: String,
}

pub struct KicktippPlatform {
    community: String,
    #[expect(dead_code, reason = "used by the kicktipp client in the next step")]
    credentials: Credentials,
}

impl KicktippPlatform {
    pub fn new(cfg: &KicktippConfig, credentials: Credentials) -> Self {
        Self {
            community: cfg.community.clone(),
            credentials,
        }
    }
}

impl TipPlatform for KicktippPlatform {
    async fn login(&self) -> Result<(), PlatformError> {
        info!(community = %self.community, "logging in to kicktipp");
        Err(NOT_IMPLEMENTED)
    }

    async fn open_matches(&self) -> Result<Vec<OpenMatch>, PlatformError> {
        Err(NOT_IMPLEMENTED)
    }

    async fn season_tips(&self) -> Result<Vec<PlacedTip>, PlatformError> {
        Err(NOT_IMPLEMENTED)
    }

    async fn odds(&self, _matches: &[Match]) -> Result<OddsMap, PlatformError> {
        Err(NOT_IMPLEMENTED)
    }

    async fn submit_tips(&self, _tips: &[TipSubmission]) -> Result<(), PlatformError> {
        Err(NOT_IMPLEMENTED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_not_implemented<T>(r: Result<T, PlatformError>) -> bool {
        matches!(r, Err(PlatformError::NotImplemented(_)))
    }

    #[tokio::test]
    async fn every_method_is_not_implemented() {
        let creds = Credentials::from_parts("test-user".into(), "hunter2".into()).unwrap();
        let p = KicktippPlatform::new(
            &KicktippConfig {
                community: "test-round".into(),
            },
            creds,
        );
        assert!(is_not_implemented(p.login().await));
        assert!(is_not_implemented(p.open_matches().await));
        assert!(is_not_implemented(p.season_tips().await));
        assert!(is_not_implemented(p.odds(&[]).await));
        assert!(is_not_implemented(p.submit_tips(&[]).await));
    }
}
