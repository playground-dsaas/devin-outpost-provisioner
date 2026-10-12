//! Process configuration, read from the environment.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;

#[derive(Debug, Clone)]
pub struct Config {
    /// Devin API base URL (enterprise hosts: `https://<host>/api`).
    pub api_url: String,
    /// File holding the Devin service-user token (a mounted Secret).
    pub token_file: PathBuf,
    /// Multi-document YAML applied into every org namespace (a mounted ConfigMap).
    pub manifests_path: PathBuf,
    pub poll_interval: Duration,
    /// Org namespace = prefix + 12 hex chars of the org id.
    pub namespace_prefix: String,
    /// Outpost name = prefix + org slug. Unique per cluster.
    pub outpost_name_prefix: String,
    /// Organizations never provisioned (e.g. the enterprise-level org).
    pub exclude_org_ids: BTreeSet<String>,
    /// Run one pass and exit.
    pub once: bool,
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let poll_interval = env_or("POLL_INTERVAL_SECONDS", "60")
            .parse::<u64>()
            .map(Duration::from_secs)
            .context("POLL_INTERVAL_SECONDS")?;
        Ok(Self {
            api_url: env_or("DEVIN_API_URL", "https://api.devin.ai")
                .trim_end_matches('/')
                .to_string(),
            token_file: PathBuf::from(
                std::env::var("DEVIN_TOKEN_FILE").context("DEVIN_TOKEN_FILE is required")?,
            ),
            manifests_path: PathBuf::from(env_or(
                "MANIFESTS_PATH",
                "/etc/org-provisioner/manifests.yaml",
            )),
            poll_interval,
            namespace_prefix: env_or("NAMESPACE_PREFIX", "devin-org-"),
            outpost_name_prefix: env_or("OUTPOST_NAME_PREFIX", "outpost-"),
            exclude_org_ids: env_or("EXCLUDE_ORG_IDS", "")
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            once: matches!(env_or("RUN_ONCE", "").as_str(), "1" | "true"),
        })
    }
}
