//! YAML configuration (mounted from a ConfigMap). Contains no secrets:
//! credentials are referenced by directory only.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use crate::algorithm::AlgorithmConfig;
use crate::domain::ScoringScheme;
use crate::odds::OddsConfig;
use crate::platform::PlatformConfig;
use crate::rules::Rules;

fn default_tenant_timeout() -> u64 {
    300
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_tenant_timeout")]
    pub tenant_timeout_seconds: u64,
    pub tenants: Vec<TenantConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TenantConfig {
    pub name: String,
    /// Directory with the files `username` and `password` (Secret volume mount).
    #[serde(default)]
    pub credentials_dir: Option<PathBuf>,
    pub platform: PlatformConfig,
    #[serde(default)]
    pub odds: OddsConfig,
    #[serde(default)]
    pub algorithm: AlgorithmConfig,
    #[serde(default)]
    pub scoring: ScoringScheme,
    #[serde(default)]
    pub rules: Rules,
    #[serde(default)]
    pub overwrite_existing_tips: bool,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read config file {}: {source}", path.display())]
    Read { path: PathBuf, source: io::Error },
    #[error("could not parse config: {0}")]
    Parse(String),
    #[error("invalid config: {0}")]
    Invalid(String),
}

/// Reads, parses and validates the config file.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    parse(&text)
}

fn parse(text: &str) -> Result<Config, ConfigError> {
    let config: Config =
        serde_saphyr::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
    config.validate()?;
    Ok(config)
}

impl Config {
    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |msg: String| Err(ConfigError::Invalid(msg));
        if self.tenants.is_empty() {
            return invalid("no tenants configured".into());
        }
        if self.tenant_timeout_seconds == 0 {
            return invalid("tenantTimeoutSeconds must be greater than 0".into());
        }
        let mut names = HashSet::new();
        for t in &self.tenants {
            if t.name.is_empty() || t.name.trim() != t.name {
                return invalid(format!(
                    "tenant name {:?} must be non-empty without surrounding whitespace",
                    t.name
                ));
            }
            if !names.insert(t.name.as_str()) {
                return invalid(format!("duplicate tenant name {:?}", t.name));
            }
            if t.platform.requires_credentials() && t.credentials_dir.is_none() {
                return invalid(format!("tenant {:?}: credentialsDir is required", t.name));
            }
            if let Some(r) = &t.rules.max_same_result
                && r.max < 1
            {
                return invalid(format!(
                    "tenant {:?}: rules.maxSameResult.max must be at least 1",
                    t.name
                ));
            }
            match &t.algorithm {
                AlgorithmConfig::Poisson(p) if !(1..=10).contains(&p.max_tip_goals) => {
                    return invalid(format!(
                        "tenant {:?}: algorithm.maxTipGoals must be in 1..=10",
                        t.name
                    ));
                }
                AlgorithmConfig::Poisson(_) => {}
            }
            if let PlatformConfig::Kicktipp(k) = &t.platform
                && (k.community.is_empty()
                    || !k
                        .community
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
            {
                // The slug is interpolated into URL paths.
                return invalid(format!(
                    "tenant {:?}: platform.community must be non-empty and contain only [A-Za-z0-9_-]",
                    t.name
                ));
            }
            if let PlatformConfig::Fake(f) = &t.platform
                && f.fixture.as_os_str().is_empty()
            {
                return invalid(format!("tenant {:?}: platform.fixture is empty", t.name));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{MaxSameResult, Scope};

    fn minimal(extra_tenant: &str) -> String {
        format!(
            "tenants:\n  - name: a\n    platform: {{ type: fake, fixture: f.yaml }}\n{extra_tenant}"
        )
    }

    fn assert_parse_err(text: &str) {
        match parse(text) {
            Err(ConfigError::Parse(_)) => {}
            other => panic!("expected parse error, got {other:?}"),
        }
    }

    fn assert_invalid(text: &str) {
        match parse(text) {
            Err(ConfigError::Invalid(_)) => {}
            other => panic!("expected invalid error, got {other:?}"),
        }
    }

    #[test]
    fn parses_example_config() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/config.yaml");
        let c = load(&path).unwrap();
        assert_eq!(c.tenants.len(), 1);
        let t = &c.tenants[0];
        assert!(matches!(t.platform, PlatformConfig::Fake(_)));
        assert_eq!(
            t.rules.max_same_result,
            Some(MaxSameResult {
                max: 6,
                scope: Scope::Matchday,
                mirrored: true
            })
        );
    }

    #[test]
    fn defaults() {
        let c = parse(&minimal("")).unwrap();
        assert_eq!(c.tenant_timeout_seconds, 300);
        let t = &c.tenants[0];
        assert_eq!(t.scoring, ScoringScheme::default());
        assert!(matches!(&t.algorithm, AlgorithmConfig::Poisson(p) if p.max_tip_goals == 5));
        assert!(matches!(t.odds, OddsConfig::Platform));
        assert!(!t.overwrite_existing_tips);
        assert!(t.credentials_dir.is_none());
        assert!(t.rules.max_same_result.is_none());
    }

    #[test]
    fn full_tenant() {
        let c = parse(
            "tenantTimeoutSeconds: 60
tenants:
  - name: alice
    credentialsDir: /secrets/alice
    platform: { type: kicktipp, community: my-round-slug }
    odds: { type: platform }
    algorithm: { type: poisson, maxTipGoals: 4 }
    scoring:
      homeWin: { tendency: 2, goalDifference: 3, exact: 4 }
      draw: { tendency: 2, exact: 4 }
      awayWin: { tendency: 2, goalDifference: 3, exact: 4 }
    rules:
      maxSameResult: { max: 6, scope: matchday }
    overwriteExistingTips: true
",
        )
        .unwrap();
        assert_eq!(c.tenant_timeout_seconds, 60);
        let t = &c.tenants[0];
        assert!(
            matches!(&t.platform, PlatformConfig::Kicktipp(k) if k.community == "my-round-slug")
        );
        assert!(matches!(&t.algorithm, AlgorithmConfig::Poisson(p) if p.max_tip_goals == 4));
        assert_eq!(t.scoring, ScoringScheme::default());
        assert_eq!(
            t.rules.max_same_result,
            Some(MaxSameResult {
                max: 6,
                scope: Scope::Matchday,
                mirrored: true
            })
        );
        assert!(t.overwrite_existing_tips);
        assert_eq!(
            t.credentials_dir.as_deref(),
            Some(Path::new("/secrets/alice"))
        );
    }

    #[test]
    fn unknown_fields_rejected() {
        assert_parse_err(&format!("foo: 1\n{}", minimal("")));
        assert_parse_err(&minimal("    foo: 1\n"));
        assert_parse_err(&minimal(
            "    rules:\n      maxSameResult: { maxx: 6, max: 6, scope: season }\n",
        ));
        assert_parse_err(&minimal("    algorithm: { type: poisson, foo: 1 }\n"));
        assert_parse_err(&minimal("    platform2: 1\n"));
    }

    #[test]
    fn unknown_platform_type_rejected() {
        assert_parse_err("tenants:\n  - name: a\n    platform: { type: tippspiel }\n");
    }

    #[test]
    fn invalid_configs_rejected() {
        assert_invalid("tenants: []\n");
        assert_invalid(&format!(
            "{}  - name: a\n    platform: {{ type: fake, fixture: g.yaml }}\n",
            minimal("")
        ));
        assert_invalid("tenants:\n  - name: k\n    platform: { type: kicktipp, community: x }\n");
        assert_invalid(&minimal(
            "    rules:\n      maxSameResult: { max: 0, scope: season }\n",
        ));
        assert_invalid(&minimal(
            "    algorithm: { type: poisson, maxTipGoals: 0 }\n",
        ));
        assert_invalid(&minimal(
            "    algorithm: { type: poisson, maxTipGoals: 11 }\n",
        ));
        assert_invalid(&format!("tenantTimeoutSeconds: 0\n{}", minimal("")));
        assert_invalid("tenants:\n  - name: ' a'\n    platform: { type: fake, fixture: f.yaml }\n");
        assert_invalid("tenants:\n  - name: a\n    platform: { type: fake, fixture: '' }\n");
        for community in ["a/../b", "", "a b", "a?b"] {
            assert_invalid(&format!(
                "tenants:\n  - name: k\n    credentialsDir: /s\n    platform: {{ type: kicktipp, community: '{community}' }}\n"
            ));
        }
        parse("tenants:\n  - name: k\n    credentialsDir: /s\n    platform: { type: kicktipp, community: Test_runde-1 }\n").unwrap();
    }
}
