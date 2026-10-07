//! Entry point for the `org-provisioner` binary.
//!
//! No arguments: reconcile forever (the Deployment). `verify`: run the
//! acceptance checklist once the cluster has caught up and exit non-zero on
//! any failure (the `helm test` Pod). Both read the same environment.

use std::sync::Arc;

use devin_outposts_k8s::telemetry;
use org_provisioner::cluster::KubeCluster;
use org_provisioner::config::Config;
use org_provisioner::devin::DevinClient;
use org_provisioner::images::WorkerImages;
use org_provisioner::metrics::{self, Metrics};
use org_provisioner::reconcile::{Reconciler, Settings};
use org_provisioner::template::PoolTemplate;
use org_provisioner::token;
use org_provisioner::verify::Verifier;
use tokio::signal::unix::{SignalKind, signal};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let subcommand = std::env::args().nth(1);
    match subcommand.as_deref() {
        Some("--version") => {
            println!("org-provisioner {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        None | Some("verify") => {}
        Some(other) => {
            anyhow::bail!("unknown argument {other:?}; expected no arguments or `verify`")
        }
    }
    telemetry::init();
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("no other crypto provider is installed before this");

    let config = Config::from_env()?;
    let template = PoolTemplate::load(&config.pool_template_path)?;
    let images = WorkerImages::load(&config.worker_images_path)?;
    let token = token::load(&config.token_source).await?;
    let devin = DevinClient::new(&config.api_url, token.clone())?;
    let cluster = KubeCluster::new(kube::Client::try_default().await?);

    if subcommand.is_some() {
        return verify(config, devin, cluster, template, images).await;
    }
    tracing::info!(?config, "starting org-provisioner");
    let metrics = Metrics::new();
    let reconciler = Arc::new(tokio::sync::Mutex::new(Reconciler::new(
        devin,
        cluster,
        template,
        settings(&config),
        token,
        metrics.clone(),
    )));
    reconciler.lock().await.set_worker_images(images);

    let metrics_server = tokio::spawn(metrics::serve(config.metrics_addr, metrics.clone()));

    let poll = {
        let reconciler = reconciler.clone();
        let config = config.clone();
        let metrics = metrics.clone();
        async move {
            let mut interval = tokio::time::interval(config.poll_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                let mut r = reconciler.lock().await;
                // Both files are mounted ConfigMaps; pick up edits without a restart.
                match PoolTemplate::load(&config.pool_template_path) {
                    Ok(t) => r.set_template(t),
                    Err(e) => {
                        tracing::error!(error = %e, "pool template invalid; keeping the previous one")
                    }
                }
                match WorkerImages::load(&config.worker_images_path) {
                    Ok(i) => r.set_worker_images(i),
                    Err(e) => {
                        tracing::error!(error = %e, "worker images invalid; keeping the previous ones")
                    }
                }
                if let Err(e) = r.run_pass(chrono::Utc::now()).await {
                    metrics.pass_failures.inc();
                    tracing::error!(error = %e, "reconcile pass aborted");
                }
                if config.once {
                    return anyhow::Ok(());
                }
            }
        }
    };

    // As PID 1 in the container the default SIGTERM disposition is ignored, so
    // handle it explicitly or a pod being rolled keeps reconciling until SIGKILL.
    let mut sigterm = signal(SignalKind::terminate())?;
    tokio::select! {
        res = poll => res?,
        res = metrics_server => res??,
        _ = tokio::signal::ctrl_c() => tracing::info!("received SIGINT, shutting down"),
        _ = sigterm.recv() => tracing::info!("received SIGTERM, shutting down"),
    }
    Ok(())
}

fn settings(config: &Config) -> Settings {
    Settings {
        api_url: config.api_url.clone(),
        namespace_prefix: config.namespace_prefix.clone(),
        outpost_name_prefix: config.outpost_name_prefix.clone(),
        exclude_org_ids: config.exclude_org_ids.clone(),
        deprovision_grace: config.deprovision_grace,
        system_namespace: config.system_namespace.clone(),
    }
}

async fn verify(
    config: Config,
    devin: DevinClient,
    cluster: KubeCluster,
    template: PoolTemplate,
    images: WorkerImages,
) -> anyhow::Result<()> {
    tracing::info!(
        timeout_secs = config.verify_timeout.as_secs(),
        "verifying the installation"
    );
    let verifier = Verifier::new(devin, cluster, template, images, settings(&config));
    let report = verifier
        .run_until_ok(config.verify_timeout, config.verify_interval)
        .await?;
    println!("{report}");
    let failed = report.failures().count();
    if failed > 0 {
        anyhow::bail!("{failed} checks failed");
    }
    Ok(())
}
