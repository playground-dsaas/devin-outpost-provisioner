//! Prometheus metrics and the `/metrics` + `/healthz` server.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::reconcile::PassReport;

/// Label set for deletion counters.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct KindLabel {
    kind: &'static str,
}

impl KindLabel {
    pub fn outpost() -> Self {
        Self { kind: "outpost" }
    }
    pub fn namespace() -> Self {
        Self { kind: "namespace" }
    }
}

/// All metrics. Cheap to clone; every clone shares the same registry.
#[derive(Clone)]
pub struct Metrics {
    registry: Arc<Mutex<Registry>>,
    pub organizations: Gauge,
    pub provisioned_pools: Gauge,
    pub skipped_organizations: Gauge,
    pub orphaned_namespaces: Gauge,
    pub passes: Counter,
    pub pass_failures: Counter,
    pub reconcile_errors: Counter,
    pub outposts_created: Counter,
    pub deletions: Family<KindLabel, Counter>,
    pub last_pass_unix: Gauge,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        let mut registry = Registry::with_prefix("org_provisioner");
        let organizations = Gauge::default();
        let provisioned_pools = Gauge::default();
        let skipped_organizations = Gauge::default();
        let orphaned_namespaces = Gauge::default();
        let passes = Counter::default();
        let pass_failures = Counter::default();
        let reconcile_errors = Counter::default();
        let outposts_created = Counter::default();
        let deletions = Family::<KindLabel, Counter>::default();
        let last_pass_unix = Gauge::default();
        registry.register(
            "organizations",
            "Organizations in the enterprise list",
            organizations.clone(),
        );
        registry.register(
            "provisioned_pools",
            "Organizations whose namespace and pool were applied in the last pass",
            provisioned_pools.clone(),
        );
        registry.register(
            "skipped_organizations",
            "Organizations Devin refuses to restrict an Outpost to (enterprise-level or foreign); not provisioned",
            skipped_organizations.clone(),
        );
        registry.register(
            "orphaned_namespaces",
            "Managed namespaces whose organization is gone, awaiting deletion",
            orphaned_namespaces.clone(),
        );
        registry.register("passes", "Completed reconcile passes", passes.clone());
        registry.register(
            "pass_failures",
            "Reconcile passes aborted because inputs could not be read",
            pass_failures.clone(),
        );
        registry.register(
            "reconcile_errors",
            "Per-organization reconcile failures",
            reconcile_errors.clone(),
        );
        registry.register(
            "outposts_created",
            "Devin Outposts created",
            outposts_created.clone(),
        );
        registry.register(
            "deletions",
            "Namespaces and Outposts deleted",
            deletions.clone(),
        );
        registry.register(
            "last_pass_unix_seconds",
            "Completion time of the last successful pass",
            last_pass_unix.clone(),
        );
        Self {
            registry: Arc::new(Mutex::new(registry)),
            organizations,
            provisioned_pools,
            skipped_organizations,
            orphaned_namespaces,
            passes,
            pass_failures,
            reconcile_errors,
            outposts_created,
            deletions,
            last_pass_unix,
        }
    }

    /// Record the gauges from a completed pass.
    pub fn observe_pass(&self, report: &PassReport) {
        self.organizations.set(report.organizations as i64);
        self.provisioned_pools.set(report.provisioned as i64);
        self.skipped_organizations.set(report.skipped as i64);
        self.orphaned_namespaces
            .set((report.orphaned + report.deleting) as i64);
        self.passes.inc();
        self.last_pass_unix.set(chrono::Utc::now().timestamp());
    }

    /// Text exposition of every metric.
    pub fn encode(&self) -> Result<String, std::fmt::Error> {
        let mut out = String::new();
        encode(
            &mut out,
            &self.registry.lock().expect("metrics registry poisoned"),
        )?;
        Ok(out)
    }
}

async fn metrics_handler(State(m): State<Metrics>) -> impl IntoResponse {
    match m.encode() {
        Ok(body) => (
            StatusCode::OK,
            [(
                header::CONTENT_TYPE,
                "application/openmetrics-text; version=1.0.0; charset=utf-8",
            )],
            body,
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Serve `/metrics` and `/healthz` until the process exits.
pub async fn serve(addr: SocketAddr, metrics: Metrics) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(metrics);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "metrics server listening");
    axum::serve(listener, app).await?;
    Ok(())
}
