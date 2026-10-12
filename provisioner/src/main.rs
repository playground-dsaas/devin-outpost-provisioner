//! `org-provisioner`: one Devin Outpost, one namespace and a fixed set of
//! manifests per Devin organization. Reconciles forever; see `reconcile.rs`.

mod config;
mod devin;
mod manifests;
mod naming;
mod reconcile;

use std::time::Duration;

use anyhow::Context;
use tokio::signal::unix::{SignalKind, signal};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("org-provisioner {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("no other crypto provider is installed before this");

    let config = config::Config::from_env()?;
    let token = std::fs::read_to_string(&config.token_file)
        .with_context(|| format!("reading {}", config.token_file.display()))?
        .trim()
        .to_string();
    anyhow::ensure!(!token.is_empty(), "token file is empty");
    let initial = manifests::load(&config.manifests_path)?;
    tracing::info!(manifests = initial.len(), "manifests parsed");
    let devin = devin::DevinClient::new(&config.api_url, &token)?;
    let kube = kube::Client::try_default().await?;
    tracing::info!(?config, "starting org-provisioner");

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut interval = tokio::time::interval(config.poll_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = sigterm.recv() => return Ok(()),
        }
        // The manifests are a mounted ConfigMap: re-read so edits apply without a restart.
        let pass = async {
            let manifests = manifests::load(&config.manifests_path)?;
            reconcile::run_pass(&config, &devin, &kube, &manifests, &token).await
        };
        match tokio::time::timeout(Duration::from_secs(600), pass).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!(error = format!("{e:#}"), "reconcile pass failed"),
            Err(_) => tracing::error!("reconcile pass timed out"),
        }
        if config.once {
            return Ok(());
        }
    }
}
