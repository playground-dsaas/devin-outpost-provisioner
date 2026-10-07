//! Process configuration, read from the environment.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::error::{Error, Result};

/// Where the Outposts service-user token is read from.
#[derive(Debug, Clone)]
pub enum TokenSource {
    /// AWS Secrets Manager secret ARN or name, read through Pod Identity.
    SecretsManager(String),
    /// A file on disk (for local runs and tests).
    File(PathBuf),
}

/// Runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Devin API base URL. Also written to every `OutpostPool.spec.apiUrl`.
    pub api_url: String,
    /// Where the service-user token comes from.
    pub token_source: TokenSource,
    /// Time between reconcile passes.
    pub poll_interval: Duration,
    /// How long an organization must be absent from the enterprise list before
    /// its namespace and Outpost are deleted.
    pub deprovision_grace: Duration,
    /// Path to the pool template YAML.
    pub pool_template_path: PathBuf,
    /// Path to the per-org worker images YAML.
    pub worker_images_path: PathBuf,
    /// Prefix for per-org namespaces.
    pub namespace_prefix: String,
    /// Prefix for per-org Devin Outpost names.
    pub outpost_name_prefix: String,
    /// Organization IDs never provisioned (e.g. orgs owned by another account).
    pub exclude_org_ids: BTreeSet<String>,
    /// Namespace holding the golden session-volume snapshot
    /// (`make golden-snapshot`); the provisioner's own.
    pub system_namespace: String,
    /// Bind address for `/metrics` and `/healthz`.
    pub metrics_addr: SocketAddr,
    /// Run one pass and exit.
    pub once: bool,
    /// `verify`: how long to keep re-checking until everything passes.
    pub verify_timeout: Duration,
    /// `verify`: time between attempts.
    pub verify_interval: Duration,
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn env_secs(name: &str, default: u64) -> Result<Duration> {
    match std::env::var(name) {
        Ok(v) => v
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|e| Error::Config(format!("{name}={v:?} is not a number of seconds: {e}"))),
        Err(_) => Ok(Duration::from_secs(default)),
    }
}

impl Config {
    /// Read configuration from the environment.
    pub fn from_env() -> Result<Self> {
        let token_source = match (
            std::env::var("OUTPOSTS_TOKEN_SECRET_ID"),
            std::env::var("OUTPOSTS_TOKEN_FILE"),
        ) {
            (Ok(id), _) if !id.is_empty() => TokenSource::SecretsManager(id),
            (_, Ok(path)) if !path.is_empty() => TokenSource::File(PathBuf::from(path)),
            _ => {
                return Err(Error::Config(
                    "set OUTPOSTS_TOKEN_SECRET_ID (Secrets Manager) or OUTPOSTS_TOKEN_FILE".into(),
                ));
            }
        };
        let metrics_addr = env_or("METRICS_ADDR", "0.0.0.0:8080")
            .parse()
            .map_err(|e| Error::Config(format!("METRICS_ADDR: {e}")))?;
        let exclude_org_ids = env_or("EXCLUDE_ORG_IDS", "")
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        Ok(Self {
            api_url: env_or("DEVIN_API_URL", "https://api.devin.ai")
                .trim_end_matches('/')
                .to_string(),
            token_source,
            poll_interval: env_secs("POLL_INTERVAL_SECONDS", 60)?,
            deprovision_grace: env_secs("DEPROVISION_GRACE_SECONDS", 3600)?,
            pool_template_path: PathBuf::from(env_or(
                "POOL_TEMPLATE_PATH",
                "/etc/org-provisioner/pool-template.yaml",
            )),
            worker_images_path: PathBuf::from(env_or(
                "WORKER_IMAGES_PATH",
                "/etc/org-provisioner/worker-images.yaml",
            )),
            namespace_prefix: env_or("NAMESPACE_PREFIX", "devin-org-"),
            outpost_name_prefix: env_or("OUTPOST_NAME_PREFIX", "eks-"),
            exclude_org_ids,
            system_namespace: env_or("SYSTEM_NAMESPACE", "devin-system"),
            metrics_addr,
            once: std::env::var("RUN_ONCE").is_ok_and(|v| v == "1" || v == "true"),
            verify_timeout: env_secs("VERIFY_TIMEOUT_SECONDS", 240)?,
            verify_interval: env_secs("VERIFY_INTERVAL_SECONDS", 15)?,
        })
    }
}
