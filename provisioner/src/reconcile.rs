//! One reconcile pass: make the cluster and the Devin account match the
//! enterprise's organization list.
//!
//! The pass is stateless — every decision is recomputed from the org list,
//! the Outpost list and the labelled objects already in the cluster — so it
//! is safe to re-run at any time and after any partial failure.
//!
//! Deprovisioning is deliberately slow: a namespace whose org disappears is
//! first *marked* ([`ANNOTATION_ORPHANED_SINCE`]), and only deleted once the
//! org has stayed absent for the grace period. Deletion is itself staged
//! (pool first so the operator's finalizer can release claims, then the
//! Outpost and namespace) and a pass whose org list is empty never deletes.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use chrono::{DateTime, Utc};
use devin_outposts_k8s::crd::OutpostPool;
use k8s_openapi::api::core::v1::Namespace;
use kube::ResourceExt;

use crate::cluster::Cluster;
use crate::devin::{CreateOutpost, DevinApi, Organization, Outpost};
use crate::error::{Error, Result};
use crate::images::WorkerImages;
use crate::metrics::Metrics;
use crate::naming::{self, POOL_NAME};
use crate::render::{
    self, ANNOTATION_DEFAULT_PLATFORM, ANNOTATION_ORPHANED_SINCE, ANNOTATION_OUTPOST_ID,
    ANNOTATION_OUTPOST_RESTRICTED, ANNOTATION_WORKER_IMAGE, BoundOutpost, GoldenSnapshot,
    RenderInput,
};
use crate::snapshot::VolumeSnapshot;
use crate::template::PoolTemplate;

/// Org-independent reconciler settings.
#[derive(Debug, Clone)]
pub struct Settings {
    pub api_url: String,
    pub namespace_prefix: String,
    pub outpost_name_prefix: String,
    pub exclude_org_ids: BTreeSet<String>,
    pub deprovision_grace: Duration,
    /// Namespace the golden snapshot lives in (the provisioner's own).
    pub system_namespace: String,
}

/// What one pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PassReport {
    /// Organizations in the enterprise list (after exclusions).
    pub organizations: usize,
    /// Organizations whose namespace + pool were applied this pass.
    pub provisioned: usize,
    /// Organizations whose reconcile failed this pass.
    pub errors: usize,
    /// Organizations left unprovisioned because Devin refuses to restrict an
    /// Outpost to them (the enterprise-level org, or one from another account).
    pub skipped: usize,
    /// Outposts created this pass.
    pub outposts_created: usize,
    /// Namespaces marked orphaned and still inside the grace period.
    pub orphaned: usize,
    /// Namespaces past the grace period whose pool deletion was requested.
    pub deleting: usize,
    /// Namespaces (and their Outposts) deleted this pass.
    pub deleted: usize,
    /// Provisioned orgs still awaiting the manual default-platform step.
    pub pending_default_platform: usize,
}

/// Reconciles organizations into namespaces, pools and Outposts.
pub struct Reconciler<D, C> {
    devin: D,
    cluster: C,
    template: PoolTemplate,
    images: WorkerImages,
    settings: Settings,
    token: String,
    metrics: Metrics,
}

impl<D: DevinApi, C: Cluster> Reconciler<D, C> {
    pub fn new(
        devin: D,
        cluster: C,
        template: PoolTemplate,
        settings: Settings,
        token: String,
        metrics: Metrics,
    ) -> Self {
        Self {
            devin,
            cluster,
            template,
            images: WorkerImages::default(),
            settings,
            token,
            metrics,
        }
    }

    /// Replace the per-org worker images (re-read from its ConfigMap every pass).
    pub fn set_worker_images(&mut self, images: WorkerImages) {
        self.images = images;
    }

    /// Replace the pool template (e.g. after the ConfigMap changes).
    pub fn set_template(&mut self, template: PoolTemplate) {
        self.template = template;
    }

    pub fn cluster(&self) -> &C {
        &self.cluster
    }

    pub fn devin(&self) -> &D {
        &self.devin
    }

    /// Run one pass at `now`. Fails only when the inputs (org list, Outpost
    /// list, cluster inventory) cannot be read or no golden snapshot is ready
    /// for the template's worker image; per-org failures, including an org
    /// whose per-org image has no ready golden snapshot yet, are logged,
    /// counted and skipped, leaving that org's pool as it is.
    pub async fn run_pass(&self, now: DateTime<Utc>) -> Result<PassReport> {
        let mut report = PassReport::default();

        let orgs: Vec<Organization> = self
            .devin
            .list_organizations()
            .await?
            .into_iter()
            .filter(|o| !self.settings.exclude_org_ids.contains(&o.org_id))
            .collect();
        let mut outposts = self.devin.list_outposts().await?;
        let namespaces = self.cluster.list_managed_namespaces().await?;
        let pools = self.cluster.list_managed_pools().await?;
        let snapshots = self
            .cluster
            .list_golden_snapshots(&self.settings.system_namespace)
            .await?;
        let default_image = self
            .template
            .pool
            .worker
            .overrides
            .image
            .clone()
            .ok_or_else(|| Error::Template("pool.worker.overrides.image is required".into()))?;
        let mut goldens: BTreeMap<String, GoldenSnapshot> = BTreeMap::new();
        goldens.insert(
            default_image.clone(),
            self.find_golden_snapshot(&snapshots, &default_image)
                .await?,
        );
        report.organizations = orgs.len();

        let pools_by_ns: BTreeMap<String, &OutpostPool> = pools
            .iter()
            .filter(|p| p.name_any() == POOL_NAME)
            .map(|p| (p.namespace().unwrap_or_default(), p))
            .collect();
        let ns_by_name: BTreeMap<String, &Namespace> =
            namespaces.iter().map(|n| (n.name_any(), n)).collect();

        for org in &orgs {
            let ns_name = naming::namespace(&self.settings.namespace_prefix, &org.org_id);
            let existing_ns = ns_by_name.get(&ns_name).copied();
            let existing_pool = pools_by_ns.get(&ns_name).copied();
            match self
                .reconcile_org(
                    org,
                    &ns_name,
                    existing_ns,
                    existing_pool,
                    &default_image,
                    &snapshots,
                    &mut goldens,
                    &mut outposts,
                )
                .await
            {
                Ok(Some(outcome)) => {
                    report.provisioned += 1;
                    report.outposts_created += usize::from(outcome.created_outpost);
                    report.pending_default_platform +=
                        usize::from(outcome.default_platform_pending);
                }
                Ok(None) => report.skipped += 1,
                Err(err) => {
                    report.errors += 1;
                    self.metrics.reconcile_errors.inc();
                    tracing::error!(org_id = %org.org_id, org = %org.name, namespace = %ns_name, error = %err, "failed to reconcile organization");
                }
            }
        }

        let present: BTreeSet<&str> = orgs.iter().map(|o| o.org_id.as_str()).collect();
        let orphans: Vec<&Namespace> = namespaces
            .iter()
            .filter(|ns| render::org_id_of(*ns).is_none_or(|id| !present.contains(id)))
            .collect();
        if orgs.is_empty() && !orphans.is_empty() {
            tracing::warn!(
                namespaces = orphans.len(),
                "organization list is empty; refusing to deprovision anything this pass"
            );
        } else {
            for ns in orphans {
                let pool = pools_by_ns.get(&ns.name_any()).copied();
                match self.deprovision(ns, pool, now).await {
                    Ok(Deprovision::Grace) => report.orphaned += 1,
                    Ok(Deprovision::PoolDeleting) => report.deleting += 1,
                    Ok(Deprovision::Deleted) => report.deleted += 1,
                    Err(err) => {
                        report.errors += 1;
                        self.metrics.reconcile_errors.inc();
                        tracing::error!(namespace = %ns.name_any(), error = %err, "failed to deprovision namespace");
                    }
                }
            }
        }

        self.metrics.observe_pass(&report);
        tracing::info!(?report, "reconcile pass complete");
        Ok(report)
    }

    /// The ready golden snapshot holding `image`'s home, produced by
    /// `make golden-snapshot`. The operator requires a source for new session
    /// volumes, so without one no pool is written for that image and pools
    /// stay as they are: an image change never reaches a pool before its
    /// golden home.
    async fn find_golden_snapshot(
        &self,
        snapshots: &[VolumeSnapshot],
        image: &str,
    ) -> Result<GoldenSnapshot> {
        let Some(snapshot) = snapshots.iter().find(|s| {
            s.is_ready()
                && s.annotations()
                    .get(ANNOTATION_WORKER_IMAGE)
                    .map(String::as_str)
                    == Some(image)
        }) else {
            return Err(Error::Config(format!(
                "no ready golden snapshot for worker image {image:?} among {} in {} (run `make golden-snapshot`)",
                snapshots.len(),
                self.settings.system_namespace
            )));
        };
        let content_name = snapshot
            .status
            .as_ref()
            .and_then(|s| s.bound_volume_snapshot_content_name.clone())
            .ok_or_else(|| {
                Error::Config(format!(
                    "golden VolumeSnapshot {} is ready but bound to no content",
                    snapshot.name_any()
                ))
            })?;
        let content = self
            .cluster
            .get_volume_snapshot_content(&content_name)
            .await?
            .ok_or_else(|| {
                Error::Config(format!("VolumeSnapshotContent {content_name} missing"))
            })?;
        let handle = content.snapshot_handle().ok_or_else(|| {
            Error::Config(format!(
                "VolumeSnapshotContent {content_name} has no snapshotHandle"
            ))
        })?;
        Ok(GoldenSnapshot {
            name: snapshot.name_any(),
            driver: content.spec.driver.clone(),
            snapshot_handle: handle.to_string(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn reconcile_org(
        &self,
        org: &Organization,
        ns_name: &str,
        existing_ns: Option<&Namespace>,
        existing_pool: Option<&OutpostPool>,
        default_image: &str,
        snapshots: &[VolumeSnapshot],
        goldens: &mut BTreeMap<String, GoldenSnapshot>,
        outposts: &mut Vec<Outpost>,
    ) -> Result<Option<OrgOutcome>> {
        let image = self.images.resolve(org, default_image);
        let golden = match goldens.get(image) {
            Some(g) => g.clone(),
            None => {
                let g = self.find_golden_snapshot(snapshots, image).await?;
                goldens.insert(image.to_string(), g.clone());
                g
            }
        };
        let current_image = existing_pool.and_then(|p| p.spec.worker.overrides.image.as_deref());
        if existing_pool.is_some() && current_image != Some(image) {
            tracing::info!(org_id = %org.org_id, org = %org.name, namespace = %ns_name, from = ?current_image, to = %image, golden = %golden.name, "switching worker image");
        }

        let Some((outpost, created)) = self
            .ensure_outpost(org, existing_ns, existing_pool, outposts)
            .await?
        else {
            return Ok(None);
        };

        let default_platform = existing_pool
            .and_then(|p| p.annotations().get(ANNOTATION_DEFAULT_PLATFORM))
            .map(String::as_str);
        let bundle = render::render(&RenderInput {
            org,
            namespace: ns_name,
            outpost: &outpost,
            api_url: &self.settings.api_url,
            template: &self.template,
            image,
            default_platform,
            golden: &golden,
        });

        self.cluster.apply_namespace(&bundle.namespace).await?;
        if existing_ns.is_some_and(|ns| ns.annotations().contains_key(ANNOTATION_ORPHANED_SINCE)) {
            tracing::info!(org_id = %org.org_id, namespace = %ns_name, "organization is back; clearing orphan marker");
            self.cluster
                .annotate_namespace(ns_name, ANNOTATION_ORPHANED_SINCE, None)
                .await?;
        }
        if let Some(q) = &bundle.resource_quota {
            self.cluster.apply_resource_quota(q).await?;
        }
        if let Some(l) = &bundle.limit_range {
            self.cluster.apply_limit_range(l).await?;
        }
        if let Some(n) = &bundle.network_policy {
            self.cluster.apply_network_policy(n).await?;
        }
        for b in &bundle.role_bindings {
            self.cluster.apply_role_binding(b).await?;
        }
        self.cluster
            .apply_secret(&render::render_token_secret(org, ns_name, &self.token))
            .await?;
        // Created once, never re-applied: `spec.volumeSnapshotRef` is atomic
        // and the snapshot controller stamps the bound snapshot's uid into it;
        // re-applying would strip the uid and the CSI provisioner would refuse
        // to restore from it ("bound to a different snapshot").
        let content = &bundle.volume_snapshot_content;
        if self
            .cluster
            .get_volume_snapshot_content(&content.name_any())
            .await?
            .is_none()
        {
            self.cluster.apply_volume_snapshot_content(content).await?;
        }
        self.cluster
            .apply_volume_snapshot(&bundle.volume_snapshot)
            .await?;
        self.cluster.apply_pool(&bundle.pool).await?;

        let pending = default_platform.is_none_or(|v| v == "pending");
        if existing_pool.is_none() {
            tracing::warn!(
                org_id = %org.org_id,
                org = %org.name,
                namespace = %ns_name,
                outpost_id = %outpost.outpost_id,
                "provisioned new organization; set its default platform to this Outpost in the Devin UI (no API yet)"
            );
        }
        Ok(Some(OrgOutcome {
            created_outpost: created,
            default_platform_pending: pending,
        }))
    }

    /// Find the Outpost bound to `org`, or create it. Resolution order:
    /// the ID recorded on the existing pool/namespace, an Outpost restricted to
    /// exactly this org, an unrestricted Outpost with the derived name, then
    /// create. Returns `None` when Devin rejects `allowed_org_ids` for this
    /// org: that is how the API reports the enterprise-level org (and orgs of
    /// other accounts), which get no Outpost or pool.
    async fn ensure_outpost(
        &self,
        org: &Organization,
        existing_ns: Option<&Namespace>,
        existing_pool: Option<&OutpostPool>,
        outposts: &mut Vec<Outpost>,
    ) -> Result<Option<(BoundOutpost, bool)>> {
        let recorded_id = existing_pool
            .and_then(|p| p.annotations().get(ANNOTATION_OUTPOST_ID).cloned())
            .or_else(|| {
                existing_ns.and_then(|n| n.annotations().get(ANNOTATION_OUTPOST_ID).cloned())
            });
        if let Some(id) = recorded_id {
            if let Some(o) = outposts.iter().find(|o| o.metadata.outpost_id == id) {
                return Ok(Some((bind(o, org), false)));
            }
            tracing::warn!(org_id = %org.org_id, outpost_id = %id, "recorded Outpost no longer exists; rebinding");
        }
        if let Some(o) = outposts
            .iter()
            .find(|o| o.spec.allowed_org_ids.as_deref() == Some(std::slice::from_ref(&org.org_id)))
        {
            return Ok(Some((bind(o, org), false)));
        }
        let name = naming::outpost_name(&self.settings.outpost_name_prefix, &org.name);
        if let Some(o) = outposts.iter().find(|o| {
            o.spec.name == name && o.spec.allowed_org_ids.as_ref().is_none_or(Vec::is_empty)
        }) {
            return Ok(Some((bind(o, org), false)));
        }

        let created = match self
            .devin
            .create_outpost(&CreateOutpost {
                name,
                description: Some(format!("org-provisioner: {} ({})", org.name, org.org_id)),
                allowed_org_ids: Some(vec![org.org_id.clone()]),
            })
            .await
        {
            Ok(o) => o,
            Err(Error::InvalidOrgIds(detail)) => {
                tracing::warn!(
                    org_id = %org.org_id,
                    org = %org.name,
                    %detail,
                    "Devin rejected allowed_org_ids (enterprise-level org or another account's); skipping. Add it to EXCLUDE_ORG_IDS to silence this"
                );
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        self.metrics.outposts_created.inc();
        tracing::info!(org_id = %org.org_id, outpost_id = %created.metadata.outpost_id, outpost = %created.spec.name, "created Outpost");
        let bound = bind(&created, org);
        outposts.push(created);
        Ok(Some((bound, true)))
    }

    async fn deprovision(
        &self,
        ns: &Namespace,
        pool: Option<&OutpostPool>,
        now: DateTime<Utc>,
    ) -> Result<Deprovision> {
        let name = ns.name_any();
        let since = ns
            .annotations()
            .get(ANNOTATION_ORPHANED_SINCE)
            .and_then(|v| DateTime::parse_from_rfc3339(v).ok())
            .map(|t| t.with_timezone(&Utc));
        let Some(since) = since else {
            tracing::warn!(namespace = %name, org_id = ?render::org_id_of(ns), grace_secs = self.settings.deprovision_grace.as_secs(), "organization missing from enterprise list; marking namespace orphaned");
            self.cluster
                .annotate_namespace(&name, ANNOTATION_ORPHANED_SINCE, Some(&now.to_rfc3339()))
                .await?;
            return Ok(Deprovision::Grace);
        };
        let elapsed = (now - since).to_std().unwrap_or_default();
        if elapsed < self.settings.deprovision_grace {
            return Ok(Deprovision::Grace);
        }

        if let Some(pool) = pool {
            if pool.metadata.deletion_timestamp.is_none() {
                tracing::warn!(namespace = %name, "grace period elapsed; deleting OutpostPool (operator releases its claims first)");
                self.cluster.delete_pool(&name, &pool.name_any()).await?;
            }
            return Ok(Deprovision::PoolDeleting);
        }

        if let Some(outpost_id) = ns.annotations().get(ANNOTATION_OUTPOST_ID) {
            tracing::warn!(namespace = %name, %outpost_id, "deleting Outpost");
            self.devin.delete_outpost(outpost_id).await?;
            self.metrics
                .deletions
                .get_or_create(&crate::metrics::KindLabel::outpost())
                .inc();
        }
        tracing::warn!(namespace = %name, "deleting namespace");
        self.cluster.delete_namespace(&name).await?;
        self.metrics
            .deletions
            .get_or_create(&crate::metrics::KindLabel::namespace())
            .inc();
        Ok(Deprovision::Deleted)
    }
}

fn bind(o: &Outpost, org: &Organization) -> BoundOutpost {
    BoundOutpost {
        outpost_id: o.metadata.outpost_id.clone(),
        name: o.spec.name.clone(),
        org_restricted: o
            .spec
            .allowed_org_ids
            .as_ref()
            .is_some_and(|ids| ids.iter().any(|id| id == &org.org_id)),
    }
}

struct OrgOutcome {
    created_outpost: bool,
    default_platform_pending: bool,
}

enum Deprovision {
    Grace,
    PoolDeleting,
    Deleted,
}

/// Restricted-ness recorded on an existing object, for tests and status.
pub fn recorded_restricted<K: ResourceExt>(obj: &K) -> Option<bool> {
    obj.annotations()
        .get(ANNOTATION_OUTPOST_RESTRICTED)
        .and_then(|v| v.parse().ok())
}
