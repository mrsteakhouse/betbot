#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod algorithm;
mod config;
mod credentials;
mod domain;
mod notify;
mod odds;
mod platform;
mod rules;
mod runner;
#[cfg(test)]
mod testutil;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

/// Tipping bot (Tippspiel) running as a Kubernetes CronJob.
#[derive(Debug, Parser)]
#[command(version)]
struct Cli {
    /// Path to the YAML config file.
    #[arg(long, env = "BETBOT_CONFIG")]
    config: PathBuf,
    /// Compute and log tips without submitting them.
    #[arg(long, env = "BETBOT_DRY_RUN")]
    dry_run: bool,
    /// Parse and validate the config, then exit (0 valid, 2 invalid). No network access.
    #[arg(long)]
    check_config: bool,
    /// Log output format.
    #[arg(long, env = "BETBOT_LOG_FORMAT", value_enum, default_value_t = LogFormat::Text)]
    log_format: LogFormat,
}

fn init_logging(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false);
    // Called exactly once at startup, so no global subscriber is set yet.
    match format {
        LogFormat::Text => builder.init(),
        LogFormat::Json => builder.json().init(),
    }
}

/// Resolves on Ctrl-C or (on unix) SIGTERM. A failing signal handler never resolves.
async fn shutdown_signal() -> &'static str {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            warn!(error = %e, "cannot listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let terminate = async {
            match signal(SignalKind::terminate()) {
                Ok(mut s) => {
                    s.recv().await;
                }
                Err(e) => {
                    warn!(error = %e, "cannot listen for SIGTERM");
                    std::future::pending::<()>().await;
                }
            }
        };
        tokio::select! {
            () = ctrl_c => "SIGINT",
            () = terminate => "SIGTERM",
        }
    }

    #[cfg(not(unix))]
    {
        ctrl_c.await;
        "Ctrl-C"
    }
}

/// `--check-config`: parses the notifier URL files that exist (no network).
/// Missing files are skipped, since the Secrets are usually not mounted
/// where the config is checked. Returns false if any existing file is
/// unreadable or holds an invalid URL.
async fn check_notifier_urls(config: &config::Config) -> bool {
    let mut ok = true;
    for n in &config.notifiers {
        let result = match credentials::load_secret(&n.url_file, "notifier url").await {
            Err(credentials::CredentialsError::Read { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                debug!(notifier = %n.name, "notifier url file not found, not checked");
                continue;
            }
            Err(e) => Err(notify::NotifyError::from(e)),
            Ok(url) => notify::validate_url(&url),
        };
        if let Err(e) = result {
            error!(notifier = %n.name, error = %e, "invalid notifier");
            ok = false;
        }
    }
    ok
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.log_format);

    let config = match config::load(&cli.config) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "invalid configuration");
            return ExitCode::from(2);
        }
    };
    let tenants: Vec<&str> = config.tenants.iter().map(|t| t.name.as_str()).collect();
    if cli.check_config {
        if !check_notifier_urls(&config).await {
            return ExitCode::from(2);
        }
        let notifiers: Vec<&str> = config.notifiers.iter().map(|n| n.name.as_str()).collect();
        info!(tenants = ?tenants, notifiers = ?notifiers, "configuration is valid");
        return ExitCode::SUCCESS;
    }
    info!(
        version = env!("CARGO_PKG_VERSION"),
        dry_run = cli.dry_run,
        tenants = ?tenants,
        "betbot starting"
    );

    tokio::select! {
        outcomes = runner::run_all(config, cli.dry_run) => {
            let failed_tenants: Vec<&str> = outcomes
                .iter()
                .filter(|o| o.result.is_err())
                .map(|o| o.name.as_str())
                .collect();
            let failed = failed_tenants.len();
            let ok = outcomes.len() - failed;
            if failed == 0 {
                info!(ok, failed, "run finished");
                ExitCode::SUCCESS
            } else {
                error!(ok, failed, failed_tenants = ?failed_tenants, "run finished with failures");
                ExitCode::from(1)
            }
        }
        signal = shutdown_signal() => {
            error!(signal, "received shutdown signal, aborting run");
            ExitCode::from(1)
        }
    }
}
