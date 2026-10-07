//! Reconcile-pass behaviour against an in-memory Devin account and cluster.

use std::time::Duration;

use kube::ResourceExt;
use org_provisioner::Error;
use org_provisioner::cluster::MemCluster;
use org_provisioner::devin::{CreateOutpost, DevinApi};
use org_provisioner::images::WorkerImages;
use org_provisioner::metrics::Metrics;
use org_provisioner::naming::{TOKEN_SECRET_KEY, TOKEN_SECRET_NAME};
use org_provisioner::reconcile::{PassReport, Reconciler};
use org_provisioner::render::{
    ANNOTATION_DEFAULT_PLATFORM, ANNOTATION_ORPHANED_SINCE, ANNOTATION_OUTPOST_ID,
    ANNOTATION_OUTPOST_RESTRICTED, LABEL_ORG_SLUG,
};
mod common;
use common::*;

fn reconciler_on(devin: MemDevin, cluster: MemCluster) -> Reconciler<MemDevin, MemCluster> {
    Reconciler::new(
        devin,
        cluster,
        template(),
        settings(),
        "cog_tok".into(),
        Metrics::new(),
    )
}

fn reconciler(devin: MemDevin) -> Reconciler<MemDevin, MemCluster> {
    reconciler_on(devin, golden_cluster())
}

#[tokio::test]
async fn provisions_new_org_end_to_end() {
    let devin = MemDevin::default();
    devin.add_org(ORG_A, "Alpha");
    let r = reconciler(devin);

    let report = r.run_pass(t0()).await.unwrap();
    assert_eq!(
        report,
        PassReport {
            organizations: 1,
            provisioned: 1,
            outposts_created: 1,
            pending_default_platform: 1,
            ..Default::default()
        }
    );

    let created = r.devin().created();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].name, "eks-alpha");
    assert_eq!(
        created[0].allowed_org_ids.as_deref(),
        Some(&[ORG_A.to_string()][..])
    );

    let c = r.cluster();
    let ns = c.namespace(NS_A).expect("namespace created");
    assert_eq!(
        ns.labels()["pod-security.kubernetes.io/enforce"],
        "restricted"
    );
    assert_eq!(ns.annotations()[ANNOTATION_OUTPOST_ID], "outpost_1");
    assert_eq!(ns.annotations()[ANNOTATION_OUTPOST_RESTRICTED], "true");
    assert!(c.has_quota(NS_A));
    assert!(c.has_network_policy(NS_A));
    let scc = c.role_bindings(NS_A);
    assert_eq!(scc.len(), 1);
    assert_eq!(scc[0].role_ref.name, "system:openshift:scc:nonroot-v2");
    assert_eq!(
        scc[0].subjects.as_ref().unwrap()[0].name,
        format!("system:serviceaccounts:{NS_A}")
    );
    let secret = c.secret(NS_A, TOKEN_SECRET_NAME).expect("token secret");
    assert_eq!(secret.string_data.unwrap()[TOKEN_SECRET_KEY], "cog_tok");
    let pool = c.pool(NS_A).expect("pool created");
    assert_eq!(pool.spec.pool_id, "outpost_1");
    assert_eq!(pool.spec.token_secret_ref.name, TOKEN_SECRET_NAME);
    assert_eq!(pool.annotations()[ANNOTATION_DEFAULT_PLATFORM], "pending");
}

#[tokio::test]
async fn second_pass_is_idempotent() {
    let devin = MemDevin::default();
    devin.add_org(ORG_A, "Alpha");
    devin.add_org(ORG_B, "Beta");
    let r = reconciler(devin);

    let first = r.run_pass(t0()).await.unwrap();
    assert_eq!(first.outposts_created, 2);
    let second = r.run_pass(t0()).await.unwrap();
    assert_eq!(second.outposts_created, 0);
    assert_eq!(second.provisioned, 2);
    assert_eq!(second.errors, 0);
    assert_eq!(r.devin().outposts().len(), 2);
    assert_eq!(r.cluster().namespace_names(), vec![NS_A, NS_B]);
    assert!(r.cluster().log().iter().all(|l| !l.starts_with("delete")));
}

#[tokio::test]
async fn rename_keeps_namespace_and_outpost() {
    let devin = MemDevin::default();
    devin.add_org(ORG_A, "Alpha");
    let r = reconciler(devin);
    r.run_pass(t0()).await.unwrap();

    r.devin().remove_org(ORG_A);
    r.devin().add_org(ORG_A, "Alpha Renamed");
    let report = r.run_pass(t0()).await.unwrap();

    assert_eq!(report.outposts_created, 0);
    assert_eq!(report.orphaned, 0);
    assert_eq!(r.cluster().namespace_names(), vec![NS_A]);
    let ns = r.cluster().namespace(NS_A).unwrap();
    assert_eq!(ns.labels()[LABEL_ORG_SLUG], "alpha-renamed");
    assert_eq!(r.cluster().pool(NS_A).unwrap().spec.pool_id, "outpost_1");
    assert_eq!(r.devin().outposts().len(), 1);
    assert_eq!(r.devin().outposts()[0].spec.name, "eks-alpha");
}

#[tokio::test]
async fn rebinds_existing_outpost_without_creating() {
    let devin = MemDevin::default();
    devin.add_org(ORG_A, "Alpha");
    devin
        .create_outpost(&CreateOutpost {
            name: "eks-alpha".into(),
            description: None,
            allowed_org_ids: Some(vec![ORG_A.into()]),
        })
        .await
        .unwrap();
    let r = reconciler(devin);
    let report = r.run_pass(t0()).await.unwrap();
    assert_eq!(report.outposts_created, 0);
    assert_eq!(r.cluster().pool(NS_A).unwrap().spec.pool_id, "outpost_1");
}

#[tokio::test]
async fn outpost_restricted_to_org_under_another_prefix_is_not_adopted() {
    let devin = MemDevin::default();
    devin.add_org(ORG_A, "Alpha");
    devin
        .create_outpost(&CreateOutpost {
            name: "other-cluster-alpha".into(),
            description: None,
            allowed_org_ids: Some(vec![ORG_A.into()]),
        })
        .await
        .unwrap();
    let r = reconciler(devin);
    let report = r.run_pass(t0()).await.unwrap();
    assert_eq!(report.outposts_created, 1);
    assert_eq!(r.cluster().pool(NS_A).unwrap().spec.pool_id, "outpost_2");
    assert_eq!(r.devin().outposts()[1].spec.name, "eks-alpha");
}

#[tokio::test]
async fn recorded_outpost_under_another_prefix_is_rebound() {
    let devin = MemDevin::default();
    devin.add_org(ORG_A, "Alpha");
    let mut other_install = settings();
    other_install.outpost_name_prefix = "ocp-".into();
    let r = Reconciler::new(
        devin,
        golden_cluster(),
        template(),
        other_install,
        "cog_tok".into(),
        Metrics::new(),
    );
    r.run_pass(t0()).await.unwrap();
    let (devin, cluster) = r.into_parts();
    assert_eq!(devin.outposts()[0].spec.name, "ocp-alpha");

    let r = reconciler_on(devin, cluster);
    let report = r.run_pass(t0()).await.unwrap();
    assert_eq!(report.outposts_created, 1);
    assert_eq!(r.cluster().pool(NS_A).unwrap().spec.pool_id, "outpost_2");
    assert_eq!(r.devin().outposts()[1].spec.name, "eks-alpha");
}

#[tokio::test]
async fn same_name_outpost_restricted_to_another_org_is_not_reused() {
    let devin = MemDevin::default();
    devin.add_org(ORG_A, "Alpha");
    devin
        .create_outpost(&CreateOutpost {
            name: "eks-alpha".into(),
            description: None,
            allowed_org_ids: Some(vec![ORG_B.into()]),
        })
        .await
        .unwrap();
    let r = reconciler(devin);
    let report = r.run_pass(t0()).await.unwrap();
    assert_eq!(report.outposts_created, 1);
    assert_eq!(r.cluster().pool(NS_A).unwrap().spec.pool_id, "outpost_2");
}

#[tokio::test]
async fn skips_org_devin_will_not_restrict_to() {
    let devin = MemDevin::default();
    devin.add_org(ORG_A, "Alpha");
    devin.add_org(ORG_B, "Beta");
    devin.foreign_orgs.lock().unwrap().insert(ORG_A.into());
    let r = reconciler(devin);

    let report = r.run_pass(t0()).await.unwrap();
    assert_eq!(
        report,
        PassReport {
            organizations: 2,
            provisioned: 1,
            skipped: 1,
            outposts_created: 1,
            pending_default_platform: 1,
            ..Default::default()
        }
    );
    let created = r.devin().created();
    assert_eq!(created.len(), 2);
    assert!(created.iter().all(|c| c.allowed_org_ids.is_some()));
    assert_eq!(r.devin().outposts().len(), 1);
    assert_eq!(r.devin().outposts()[0].spec.name, "eks-beta");
    assert_eq!(r.cluster().namespace_names(), vec![NS_B]);
    assert!(r.cluster().namespace(NS_A).is_none());
}

#[tokio::test]
async fn excluded_orgs_are_not_provisioned() {
    let devin = MemDevin::default();
    devin.add_org(ORG_A, "Alpha");
    devin.add_org(ORG_B, "Beta");
    let mut s = settings();
    s.exclude_org_ids.insert(ORG_B.into());
    let r = Reconciler::new(
        devin,
        golden_cluster(),
        template(),
        s,
        "t".into(),
        Metrics::new(),
    );
    let report = r.run_pass(t0()).await.unwrap();
    assert_eq!(report.organizations, 1);
    assert_eq!(r.cluster().namespace_names(), vec![NS_A]);
}

#[tokio::test]
async fn org_list_failure_aborts_pass_without_changes() {
    let devin = MemDevin::default();
    devin.add_org(ORG_A, "Alpha");
    let r = reconciler(devin);
    r.run_pass(t0()).await.unwrap();

    *r.devin().fail_org_list.lock().unwrap() = true;
    let before = r.cluster().log().len();
    assert!(r.run_pass(t0()).await.is_err());
    assert_eq!(r.cluster().log().len(), before);
    assert!(r.cluster().namespace(NS_A).is_some());
}

#[tokio::test]
async fn removed_org_is_marked_then_deleted_after_grace() {
    let devin = MemDevin::default();
    devin.add_org(ORG_A, "Alpha");
    devin.add_org(ORG_B, "Beta");
    let r = reconciler(devin);
    r.run_pass(t0()).await.unwrap();

    r.devin().remove_org(ORG_B);

    // Pass 1: marked, nothing deleted.
    let report = r.run_pass(t0()).await.unwrap();
    assert_eq!(report.orphaned, 1);
    assert_eq!(report.deleted, 0);
    let ns = r.cluster().namespace(NS_B).unwrap();
    assert_eq!(
        ns.annotations()[ANNOTATION_ORPHANED_SINCE],
        t0().to_rfc3339()
    );
    assert!(r.cluster().pool(NS_B).is_some());

    // Inside the grace window: still nothing.
    let report = r.run_pass(t0() + GRACE / 2).await.unwrap();
    assert_eq!(report.orphaned, 1);
    assert!(r.cluster().pool(NS_B).is_some());

    // Past the grace window: the pool goes first (operator finalizer).
    r.cluster().hold_pool_deletion(NS_B);
    let report = r.run_pass(t0() + GRACE).await.unwrap();
    assert_eq!(report.deleting, 1);
    assert_eq!(report.deleted, 0);
    assert!(
        r.cluster()
            .pool(NS_B)
            .unwrap()
            .metadata
            .deletion_timestamp
            .is_some()
    );
    assert!(r.cluster().namespace(NS_B).is_some());
    assert!(r.devin().deleted().is_empty());

    // Pool still finalizing: wait.
    let report = r
        .run_pass(t0() + GRACE + Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(report.deleting, 1);
    assert_eq!(
        r.cluster()
            .log()
            .iter()
            .filter(|l| l.starts_with("delete OutpostPool"))
            .count(),
        1
    );

    // Pool gone: Outpost + namespace go.
    r.cluster().release_pool_deletion(NS_B);
    let report = r
        .run_pass(t0() + GRACE + Duration::from_secs(120))
        .await
        .unwrap();
    assert_eq!(report.deleted, 1);
    assert_eq!(r.devin().deleted(), vec!["outpost_2".to_string()]);
    assert!(r.cluster().namespace(NS_B).is_none());
    assert_eq!(r.cluster().namespace_names(), vec![NS_A]);
    assert!(r.cluster().pool(NS_A).is_some());
}

#[tokio::test]
async fn org_returning_during_grace_clears_marker() {
    let devin = MemDevin::default();
    devin.add_org(ORG_A, "Alpha");
    devin.add_org(ORG_B, "Beta");
    let r = reconciler(devin);
    r.run_pass(t0()).await.unwrap();

    r.devin().remove_org(ORG_A);
    r.run_pass(t0()).await.unwrap();
    assert!(
        r.cluster()
            .namespace(NS_A)
            .unwrap()
            .annotations()
            .contains_key(ANNOTATION_ORPHANED_SINCE)
    );

    r.devin().add_org(ORG_A, "Alpha");
    let report = r.run_pass(t0() + GRACE / 2).await.unwrap();
    assert_eq!(report.orphaned, 0);
    assert_eq!(report.provisioned, 2);
    assert!(
        !r.cluster()
            .namespace(NS_A)
            .unwrap()
            .annotations()
            .contains_key(ANNOTATION_ORPHANED_SINCE)
    );

    // Well past the original grace: nothing is deleted because the marker is gone.
    let report = r.run_pass(t0() + GRACE * 3).await.unwrap();
    assert_eq!(report.deleted + report.deleting, 0);
    assert!(r.cluster().pool(NS_A).is_some());
}

#[tokio::test]
async fn empty_org_list_never_deprovisions() {
    let devin = MemDevin::default();
    devin.add_org(ORG_A, "Alpha");
    let r = reconciler(devin);
    r.run_pass(t0()).await.unwrap();

    r.devin().remove_org(ORG_A);
    let report = r.run_pass(t0()).await.unwrap();
    assert_eq!(report.organizations, 0);
    assert_eq!(report.orphaned + report.deleting + report.deleted, 0);
    assert!(
        !r.cluster()
            .namespace(NS_A)
            .unwrap()
            .annotations()
            .contains_key(ANNOTATION_ORPHANED_SINCE)
    );
    let report = r.run_pass(t0() + GRACE * 2).await.unwrap();
    assert_eq!(report.deleted, 0);
    assert!(r.cluster().pool(NS_A).is_some());
}

#[tokio::test]
async fn manual_default_platform_flag_survives_reconcile() {
    let devin = MemDevin::default();
    devin.add_org(ORG_A, "Alpha");
    let r = reconciler(devin);
    r.run_pass(t0()).await.unwrap();

    let mut pool = r.cluster().pool(NS_A).unwrap();
    pool.annotations_mut()
        .insert(ANNOTATION_DEFAULT_PLATFORM.into(), "set".into());
    org_provisioner::cluster::Cluster::apply_pool(r.cluster(), &pool)
        .await
        .unwrap();

    let report = r.run_pass(t0()).await.unwrap();
    assert_eq!(report.pending_default_platform, 0);
    assert_eq!(
        r.cluster().pool(NS_A).unwrap().annotations()[ANNOTATION_DEFAULT_PLATFORM],
        "set"
    );
}

#[tokio::test]
async fn binds_golden_snapshot_into_each_org() {
    let devin = MemDevin::default();
    devin.add_org(ORG_A, "Alpha");
    let r = reconciler(devin);
    let c = r.cluster();

    let report = r.run_pass(t0()).await.unwrap();
    assert_eq!(report.errors, 0);

    let content = c
        .volume_snapshot_content(&format!("{GOLDEN}-{NS_A}"))
        .expect("per-org content");
    assert_eq!(
        content.spec.source.snapshot_handle.as_deref(),
        Some("snap-golden")
    );
    assert_eq!(content.spec.deletion_policy, "Retain");
    let snapshot = c.volume_snapshot(NS_A, GOLDEN).expect("per-org snapshot");
    assert_eq!(
        snapshot.spec.source.volume_snapshot_content_name.as_deref(),
        Some(format!("{GOLDEN}-{NS_A}").as_str())
    );
    let source = c
        .pool(NS_A)
        .unwrap()
        .spec
        .resume
        .volume_data_source
        .unwrap();
    assert_eq!(
        (source.kind.as_str(), source.name.as_str()),
        ("VolumeSnapshot", GOLDEN)
    );

    // The bindings are applied before the pool that references them.
    let log = c.log();
    let pos = |s: &str| log.iter().rposition(|l| l == s).unwrap();
    assert!(pos(&format!("apply VolumeSnapshot/{GOLDEN}")) < pos("apply OutpostPool/org"));

    // The content is immutable once bound, so later passes leave it alone.
    r.run_pass(t0()).await.unwrap();
    let content_applies = c
        .log()
        .iter()
        .filter(|l| l.starts_with("apply VolumeSnapshotContent/"))
        .count();
    assert_eq!(content_applies, 1);
}

#[tokio::test]
async fn pass_aborts_until_the_golden_snapshot_for_the_image_is_ready() {
    let devin = MemDevin::default();
    devin.add_org(ORG_A, "Alpha");
    let cluster = MemCluster::default();
    // Not ready yet, and a ready one for a different image.
    cluster.add_golden_snapshot("devin-system", GOLDEN, &worker_image(), "snap-new", false);
    cluster.add_golden_snapshot(
        "devin-system",
        "worker-home-other",
        "other:1",
        "snap-x",
        true,
    );
    let r = reconciler_on(devin, cluster);
    let c = r.cluster();

    let err = r.run_pass(t0()).await.unwrap_err();
    assert!(matches!(err, Error::Config(_)), "{err}");
    assert!(c.pool(NS_A).is_none());
    assert!(c.volume_snapshot(NS_A, GOLDEN).is_none());
    assert!(c.log().iter().all(|l| !l.starts_with("apply")));
}

const DS_IMAGE: &str = "registry.example/devin-outpost-ds:2026.10";
const DS_GOLDEN: &str = "worker-home-ds";

fn ds_rules(org_pattern: &str) -> WorkerImages {
    WorkerImages::parse(&format!(
        "images: {{data-science: {DS_IMAGE}}}\nrules: [{{org: '{org_pattern}', image: data-science}}]\n"
    ))
    .unwrap()
}

fn pool_image(c: &MemCluster, ns: &str) -> Option<String> {
    c.pool(ns)?.spec.worker.overrides.image
}

fn pool_source(c: &MemCluster, ns: &str) -> Option<String> {
    Some(c.pool(ns)?.spec.resume.volume_data_source?.name)
}

#[tokio::test]
async fn per_org_image_rule_moves_only_that_org() {
    let devin = MemDevin::default();
    devin.add_org(ORG_A, "Alpha Data");
    devin.add_org(ORG_B, "Beta");
    let cluster = golden_cluster();
    cluster.add_golden_snapshot("devin-system", DS_GOLDEN, DS_IMAGE, "snap-ds", true);
    let mut r = reconciler_on(devin, cluster);

    // Both on the template image first.
    let report = r.run_pass(t0()).await.unwrap();
    assert_eq!((report.provisioned, report.errors), (2, 0));
    assert_eq!(pool_image(r.cluster(), NS_A), Some(worker_image()));

    // The slug of "Alpha Data" is alpha-data; a glob on it moves only org A.
    r.set_worker_images(ds_rules("alpha-*"));
    let report = r.run_pass(t0()).await.unwrap();
    assert_eq!((report.provisioned, report.errors), (2, 0));
    let c = r.cluster();
    assert_eq!(pool_image(c, NS_A).as_deref(), Some(DS_IMAGE));
    assert_eq!(pool_source(c, NS_A).as_deref(), Some(DS_GOLDEN));
    assert_eq!(pool_image(c, NS_B), Some(worker_image()));
    assert_eq!(pool_source(c, NS_B).as_deref(), Some(GOLDEN));
    // The new image's golden snapshot is bound into A's namespace, with the
    // handle of its own snapshot, and the old binding is left in place.
    let content = c
        .volume_snapshot_content(&format!("{DS_GOLDEN}-{NS_A}"))
        .unwrap();
    assert_eq!(
        content.spec.source.snapshot_handle.as_deref(),
        Some("snap-ds")
    );
    assert!(c.volume_snapshot(NS_A, DS_GOLDEN).is_some());
    assert!(c.volume_snapshot(NS_A, GOLDEN).is_some());
    assert!(c.volume_snapshot(NS_B, DS_GOLDEN).is_none());
    // Nothing else about A changed: same namespace, Outpost and pool name.
    assert_eq!(r.devin().created().len(), 2);

    // Removing the rule moves A back.
    r.set_worker_images(WorkerImages::default());
    r.run_pass(t0()).await.unwrap();
    assert_eq!(pool_image(r.cluster(), NS_A), Some(worker_image()));
    assert_eq!(pool_source(r.cluster(), NS_A).as_deref(), Some(GOLDEN));
}

#[tokio::test]
async fn rules_match_display_name_and_org_id_too() {
    for pattern in ["Alpha Data", ORG_A, "org-aaaa*"] {
        let devin = MemDevin::default();
        devin.add_org(ORG_A, "Alpha Data");
        devin.add_org(ORG_B, "Beta");
        let cluster = golden_cluster();
        cluster.add_golden_snapshot("devin-system", DS_GOLDEN, DS_IMAGE, "snap-ds", true);
        let mut r = reconciler_on(devin, cluster);
        r.set_worker_images(ds_rules(pattern));
        let report = r.run_pass(t0()).await.unwrap();
        assert_eq!(report.errors, 0, "{pattern}");
        assert_eq!(
            pool_image(r.cluster(), NS_A).as_deref(),
            Some(DS_IMAGE),
            "{pattern}"
        );
        assert_eq!(
            pool_image(r.cluster(), NS_B),
            Some(worker_image()),
            "{pattern}"
        );
    }
}

#[tokio::test]
async fn org_whose_image_has_no_golden_snapshot_fails_alone_and_keeps_its_pool() {
    let devin = MemDevin::default();
    devin.add_org(ORG_A, "Alpha Data");
    devin.add_org(ORG_B, "Beta");
    let mut r = reconciler(devin);
    r.run_pass(t0()).await.unwrap();
    assert_eq!(pool_image(r.cluster(), NS_A), Some(worker_image()));

    // Rule points A at an image with no golden snapshot yet (not even unready).
    r.set_worker_images(ds_rules("alpha-data"));
    let report = r.run_pass(t0()).await.unwrap();
    assert_eq!((report.provisioned, report.errors), (1, 1));
    let c = r.cluster();
    assert_eq!(pool_image(c, NS_A), Some(worker_image()));
    assert_eq!(pool_source(c, NS_A).as_deref(), Some(GOLDEN));
    assert!(c.volume_snapshot(NS_A, DS_GOLDEN).is_none());
    assert_eq!(pool_image(c, NS_B), Some(worker_image()));

    // A snapshot that exists but is not ready yet is the same.
    c.add_golden_snapshot("devin-system", DS_GOLDEN, DS_IMAGE, "snap-ds", false);
    let report = r.run_pass(t0()).await.unwrap();
    assert_eq!((report.provisioned, report.errors), (1, 1));
    assert_eq!(pool_image(r.cluster(), NS_A), Some(worker_image()));

    // Once ready, the next pass moves A.
    r.cluster()
        .add_golden_snapshot("devin-system", DS_GOLDEN, DS_IMAGE, "snap-ds", true);
    let report = r.run_pass(t0()).await.unwrap();
    assert_eq!((report.provisioned, report.errors), (2, 0));
    assert_eq!(pool_image(r.cluster(), NS_A).as_deref(), Some(DS_IMAGE));
}

#[tokio::test]
async fn new_org_matched_by_a_rule_is_not_provisioned_until_its_golden_is_ready() {
    let devin = MemDevin::default();
    devin.add_org(ORG_A, "Alpha Data");
    let mut r = reconciler(devin);
    r.set_worker_images(ds_rules("alpha-*"));
    let report = r.run_pass(t0()).await.unwrap();
    assert_eq!((report.provisioned, report.errors), (0, 1));
    assert!(r.cluster().pool(NS_A).is_none());
    // No Outpost is created for an org whose pool cannot be written.
    assert!(r.devin().created().is_empty());
}
