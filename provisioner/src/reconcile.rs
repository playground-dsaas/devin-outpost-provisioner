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
    ANNOTATION_OUTPOST_RESTRICTED, ANNOTATION_WORKER_IMAGE, BoundOutpost, DEFAULT_PLATFORM_OTHER,
    DEFAULT_PLATFORM_PENDING, DEFAULT_PLATFORM_SET, GoldenSnapshot, RenderInput,
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

    pub fn into_parts(self) -> (D, C) {
        (self.devin, self.cluster)
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

        let recorded_default = existing_pool
            .and_then(|p| p.annotations().get(ANNOTATION_DEFAULT_PLATFORM))
            .map(String::as_str);
        let default_platform = self
            .ensure_default_platform(org, &outpost.outpost_id, recorded_default)
            .await?;
        let bundle = render::render(&RenderInput {
            org,
            namespace: ns_name,
            outpost: &outpost,
            api_url: &self.settings.api_url,
            template: &self.template,
            image,
            default_platform: Some(default_platform),
            golden: &golden,
        });

        self.cluster.apply_namespace(&bundle.namespace).await?;
        // First: they grant the provisioner (and the operator) its write
        // access to everything else in the namespace.
        for b in &bundle.role_bindings {
            self.cluster.apply_role_binding(b).await?;
        }
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
        self.cluster
            .apply_secret(&render::render_token_secret(org, ns_name, &self.token))
            .await?;
        // Created once, never re-applied: `spec.volumeSnapshotRef` is atomic
        // and the snapshot controller stamps the bound snapshot's uid into it;
        // re-applying would strip the uid and the CSI provisioner would refuse
        // to restore from it ("bound to a different snapshot"). The content is
        // cluster-scoped, so it outlives a deleted namespace: one bound to a
        // VolumeSnapshot uid that no longer exists is deleted here and created
        // afresh on the next pass, once its finalizers have run.
        let content = &bundle.volume_snapshot_content;
        let content_name = content.name_any();
        let existing = self
            .cluster
            .get_volume_snapshot_content(&content_name)
            .await?;
        let bound_uid = existing
            .as_ref()
            .and_then(|c| c.spec.volume_snapshot_ref.uid.as_deref());
        match (existing.is_some(), bound_uid) {
            (false, _) => self.cluster.apply_volume_snapshot_content(content).await?,
            (true, None) => {}
            (true, Some(bound_uid)) => {
                let current = self
                    .cluster
                    .get_volume_snapshot(ns_name, &bundle.volume_snapshot.name_any())
                    .await?;
                if current.as_ref().and_then(|v| v.metadata.uid.as_deref()) != Some(bound_uid) {
                    tracing::warn!(namespace = %ns_name, content = %content_name, "VolumeSnapshotContent is bound to a VolumeSnapshot that no longer exists; replacing");
                    self.cluster
                        .delete_volume_snapshot_content(&content_name)
                        .await?;
                }
            }
        }
        self.cluster
            .apply_volume_snapshot(&bundle.volume_snapshot)
            .await?;
        self.cluster.apply_pool(&bundle.pool).await?;

        if existing_pool.is_none() {
            tracing::info!(
                org_id = %org.org_id,
                org = %org.name,
                namespace = %ns_name,
                outpost_id = %outpost.outpost_id,
                default_platform,
                "provisioned new organization"
            );
        }
        Ok(Some(OrgOutcome {
            created_outpost: created,
            default_platform_pending: default_platform == DEFAULT_PLATFORM_PENDING,
        }))
    }

    /// Point the org's default session placement at its Outpost when none is
    /// set. A default that already points elsewhere (another cluster's
    /// Outpost, a hosted platform) is a choice and is left alone. Returns the
    /// [`ANNOTATION_DEFAULT_PLATFORM`] value to record.
    async fn ensure_default_platform(
        &self,
        org: &Organization,
        outpost_id: &str,
        recorded: Option<&str>,
    ) -> Result<&'static str> {
        let current = match self.devin.get_default_platform(&org.org_id).await {
            Ok(current) => current,
            Err(Error::Api { status, body }) => {
                tracing::warn!(org_id = %org.org_id, org = %org.name, status, body = %body, "cannot read the org default platform; set it to the Outpost in the Devin UI");
                return Ok(DEFAULT_PLATFORM_PENDING);
            }
            Err(e) => return Err(e),
        };
        if current.outpost_pool_id.as_deref() == Some(outpost_id) {
            return Ok(DEFAULT_PLATFORM_SET);
        }
        if !current.is_unset() {
            if recorded != Some(DEFAULT_PLATFORM_OTHER) {
                tracing::warn!(org_id = %org.org_id, org = %org.name, current = %current.describe(), "org default platform is not this Outpost; leaving it");
            }
            return Ok(DEFAULT_PLATFORM_OTHER);
        }
        match self
            .devin
            .set_default_platform(&org.org_id, outpost_id)
            .await
        {
            Ok(()) => {
                tracing::info!(org_id = %org.org_id, org = %org.name, outpost_id, "org default platform set to its Outpost");
                Ok(DEFAULT_PLATFORM_SET)
            }
            Err(Error::Api { status, body }) => {
                tracing::warn!(org_id = %org.org_id, org = %org.name, status, body = %body, "cannot set the org default platform (token lacks ManageOrgSettings?); set it in the Devin UI");
                Ok(DEFAULT_PLATFORM_PENDING)
            }
            Err(e) => Err(e),
        }
    }

    /// Find the Outpost bound to `org`, or create it. Resolution order:
    /// the ID recorded on the existing pool/namespace, an Outpost with this
    /// install's derived name (`outpost_name_prefix` + org slug) that is
    /// restricted to this org or unrestricted, then create. Only Outposts
    /// named with this install's prefix qualify, recorded ones included: an
    /// Outpost restricted to this org under another prefix belongs to another
    /// cluster, and two operators on one Outpost would both claim its
    /// sessions. Changing the prefix therefore rebinds every org to new
    /// Outposts. Returns `None` when Devin rejects
    /// `allowed_org_ids` for this org: that is how the API reports the
    /// enterprise-level org (and orgs of other accounts), which get no Outpost
    /// or pool.
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
            match outposts.iter().find(|o| o.metadata.outpost_id == id) {
                Some(o) if o.spec.name.starts_with(&self.settings.outpost_name_prefix) => {
                    return Ok(Some((bind(o, org), false)));
                }
                Some(o) => tracing::warn!(
                    org_id = %org.org_id, outpost_id = %id, outpost = %o.spec.name,
                    prefix = %self.settings.outpost_name_prefix,
                    "recorded Outpost does not carry this install's prefix; rebinding"
                ),
                None => {
                    tracing::warn!(org_id = %org.org_id, outpost_id = %id, "recorded Outpost no longer exists; rebinding")
                }
            }
        }
        let name = naming::outpost_name(&self.settings.outpost_name_prefix, &org.name);
        if let Some(o) = outposts.iter().find(|o| {
            o.spec.name == name
                && match o.spec.allowed_org_ids.as_deref() {
                    None | Some([]) => true,
                    Some(ids) => ids == [org.org_id.clone()],
                }
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
        // Cluster-scoped, so not swept by the namespace deletion.
        if let Some(org_id) = render::org_id_of(ns) {
            self.cluster
                .delete_volume_snapshot_contents_of(org_id)
                .await?;
        }
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
