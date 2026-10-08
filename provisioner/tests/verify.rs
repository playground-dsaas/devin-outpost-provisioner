//! The acceptance checklist against an in-memory Devin account and cluster.

use std::time::Duration;

use devin_outposts_k8s::crd::{Condition, OutpostPoolStatus};
use org_provisioner::cluster::MemCluster;
use org_provisioner::images::WorkerImages;
use org_provisioner::metrics::Metrics;
use org_provisioner::reconcile::Reconciler;
use org_provisioner::template::PoolTemplate;
use org_provisioner::verify::{Outcome, Report, Verifier};

mod common;
use common::*;

fn verifier(devin: MemDevin, cluster: MemCluster) -> Verifier<MemDevin, MemCluster> {
    Verifier::new(
        devin,
        cluster,
        template(),
        WorkerImages::default(),
        settings(),
    )
}

/// One org provisioned by a reconcile pass with `template`, before the
/// operator and the snapshot controller have reacted.
async fn provisioned(template: PoolTemplate) -> (MemDevin, MemCluster) {
    let devin = MemDevin::default();
    devin.add_org(ORG_A, "Alpha");
    let cluster = MemCluster::default();
    let image = template.pool.worker.overrides.image.clone().unwrap();
    cluster.add_golden_snapshot("devin-system", GOLDEN, &image, "snap-golden", true);
    let r = Reconciler::new(
        devin,
        cluster,
        template,
        settings(),
        "cog_tok".into(),
        Metrics::new(),
    );
    r.run_pass(t0()).await.unwrap();
    r.into_parts()
}

fn status(phase: &str, message: Option<&str>) -> OutpostPoolStatus {
    OutpostPoolStatus {
        phase: Some(phase.to_string()),
        claimed_sessions: 0,
        last_synced: Some("2026-01-01T00:00:00Z".to_string()),
        watch_cursor: None,
        conditions: vec![Condition {
            type_: "Ready".to_string(),
            status: (phase == "Ready").to_string(),
            reason: None,
            message: message.map(str::to_string),
            last_transition_time: None,
        }],
    }
}

fn failure<'a>(report: &'a Report, name: &str) -> &'a str {
    match report.outcome(name) {
        Some(Outcome::Fail(detail)) => detail,
        other => panic!("{name}: expected a failure, got {other:?}"),
    }
}

fn passes(report: &Report, name: &str) -> bool {
    matches!(report.outcome(name), Some(Outcome::Pass(_)))
}

#[tokio::test]
async fn unprovisioned_org_fails_but_golden_snapshot_passes() {
    let devin = MemDevin::default();
    devin.add_org(ORG_A, "Alpha");
    let report = verifier(devin, golden_cluster()).run().await.unwrap();

    assert!(!report.ok());
    assert!(passes(&report, "devin-api/organizations"));
    assert!(passes(
        &report,
        &format!("golden-snapshot {}", worker_image())
    ));
    assert!(failure(&report, &format!("{NS_A}/namespace")).contains("not provisioned"));
    assert!(report.outcome(&format!("{NS_A}/pool")).is_none());
}

#[tokio::test]
async fn passes_once_operator_and_snapshot_controller_have_caught_up() {
    let (devin, cluster) = provisioned(template()).await;
    let v = verifier(devin, cluster);

    let report = v.run().await.unwrap();
    assert!(!report.ok());
    assert!(passes(&report, &format!("{NS_A}/namespace")));
    assert!(passes(&report, &format!("{NS_A}/pool")));
    assert!(passes(&report, &format!("{NS_A}/pool/image")));
    assert!(passes(&report, &format!("{NS_A}/token-secret")));
    assert!(passes(
        &report,
        &format!("{NS_A}/rolebinding/scc-nonroot-v2")
    ));
    assert!(failure(&report, &format!("{NS_A}/operator")).contains("no status"));
    assert!(failure(&report, &format!("{NS_A}/golden-binding")).contains("not readyToUse"));

    v.cluster().set_pool_status(NS_A, status("Ready", None));
    v.cluster().mark_volume_snapshot_ready(NS_A, GOLDEN);

    let report = v.run().await.unwrap();
    assert!(report.ok(), "{report}");
    assert!(passes(&report, &format!("{NS_A}/operator")));
    assert!(passes(&report, &format!("{NS_A}/golden-binding")));
    assert!(passes(&report, &format!("{NS_A}/default-platform")));
    assert!(report.to_string().ends_with("0 failed"));
}

#[tokio::test]
async fn degraded_pool_and_missing_scc_binding_fail() {
    // Provisioned with the generic template, verified against the OpenShift one.
    let (devin, cluster) = provisioned(template_default()).await;
    cluster.set_pool_status(NS_A, status("Unauthorized", Some("401 from the queue API")));
    cluster.mark_volume_snapshot_ready(NS_A, GOLDEN);
    let report = verifier(devin, cluster).run().await.unwrap();

    let operator = failure(&report, &format!("{NS_A}/operator"));
    assert!(operator.contains("Unauthorized") && operator.contains("401"));
    assert!(
        failure(&report, &format!("{NS_A}/rolebinding/scc-nonroot-v2"))
            .contains("system:openshift:scc:nonroot-v2")
    );
}

#[tokio::test]
async fn unready_golden_snapshot_and_api_failure_are_reported() {
    let devin = MemDevin::default();
    devin.add_org(ORG_A, "Alpha");
    let cluster = MemCluster::default();
    cluster.add_golden_snapshot("devin-system", GOLDEN, &worker_image(), "snap-1", false);
    let v = verifier(devin, cluster);
    let report = v.run().await.unwrap();
    assert!(
        failure(&report, &format!("golden-snapshot {}", worker_image()))
            .contains("1 exist but are not ready")
    );
    assert!(failure(&report, &format!("{NS_A}/namespace")).contains("not provisioned"));

    *v.devin().fail_org_list.lock().unwrap() = true;
    let report = v
        .run_until_ok(Duration::ZERO, Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(report.checks.len(), 1);
    assert!(failure(&report, "devin-api/organizations").contains("503"));
}

#[tokio::test]
async fn pool_bound_to_another_installs_outpost_fails() {
    let (devin, cluster) = provisioned(template()).await;
    let mut other_install = settings();
    other_install.outpost_name_prefix = "ocp-".into();
    let v = Verifier::new(
        devin,
        cluster,
        template(),
        WorkerImages::default(),
        other_install,
    );

    let report = v.run().await.unwrap();
    let detail = failure(&report, &format!("{NS_A}/pool"));
    assert!(
        detail.contains("eks-alpha") && detail.contains("\"ocp-\""),
        "{detail}"
    );
}
