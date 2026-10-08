//! `org-provisioner verify`: the acceptance checklist `helm test` runs against
//! an installed release.
//!
//! Every check only reads the cluster and the Devin account. Together they
//! cover what a session needs before a worker pod can start for an org: the
//! namespace, an `OutpostPool` the operator has synced (`status.phase:
//! Ready`), the token Secret, a ready binding of the golden snapshot and each
//! RoleBinding the template asks for (on OpenShift, the SCC that admits
//! uid 1000). A clean report does not prove a worker pod starts and reaches
//! Devin; only a real session does.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::{Duration, Instant};

use devin_outposts_k8s::crd::OutpostPool;
use k8s_openapi::api::core::v1::Namespace;
use kube::ResourceExt;

use crate::cluster::Cluster;
use crate::devin::{DevinApi, Organization, Outpost};
use crate::error::{Error, Result};
use crate::images::WorkerImages;
use crate::naming::{self, POOL_NAME, TOKEN_SECRET_KEY, TOKEN_SECRET_NAME};
use crate::reconcile::Settings;
use crate::render::{self, ANNOTATION_ORPHANED_SINCE, ANNOTATION_WORKER_IMAGE};
use crate::snapshot::VolumeSnapshot;
use crate::template::PoolTemplate;

/// Result of one check. `Warn` is a condition a human must resolve that does
/// not stop sessions from starting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Pass(String),
    Warn(String),
    Fail(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub outcome: Outcome,
}

/// The checklist, in the order the checks ran.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    fn push(&mut self, name: impl Into<String>, outcome: Outcome) {
        self.checks.push(Check {
            name: name.into(),
            outcome,
        });
    }

    fn pass(&mut self, name: impl Into<String>, detail: impl Into<String>) {
        self.push(name, Outcome::Pass(detail.into()));
    }

    fn warn(&mut self, name: impl Into<String>, detail: impl Into<String>) {
        self.push(name, Outcome::Warn(detail.into()));
    }

    fn fail(&mut self, name: impl Into<String>, detail: impl Into<String>) {
        self.push(name, Outcome::Fail(detail.into()));
    }

    pub fn outcome(&self, name: &str) -> Option<&Outcome> {
        self.checks
            .iter()
            .find(|c| c.name == name)
            .map(|c| &c.outcome)
    }

    pub fn failures(&self) -> impl Iterator<Item = &Check> {
        self.checks
            .iter()
            .filter(|c| matches!(c.outcome, Outcome::Fail(_)))
    }

    /// No check failed (warnings allowed).
    pub fn ok(&self) -> bool {
        self.failures().next().is_none()
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (mut passed, mut warned, mut failed) = (0, 0, 0);
        for c in &self.checks {
            let (tag, detail) = match &c.outcome {
                Outcome::Pass(d) => {
                    passed += 1;
                    ("ok  ", d)
                }
                Outcome::Warn(d) => {
                    warned += 1;
                    ("WARN", d)
                }
                Outcome::Fail(d) => {
                    failed += 1;
                    ("FAIL", d)
                }
            };
            writeln!(f, "{tag}  {}: {detail}", c.name)?;
        }
        write!(
            f,
            "{} checks: {passed} passed, {warned} warnings, {failed} failed",
            self.checks.len()
        )
    }
}

/// Runs the checklist with the provisioner's own configuration, so what it
/// expects is exactly what the running provisioner would write.
pub struct Verifier<D, C> {
    devin: D,
    cluster: C,
    template: PoolTemplate,
    images: WorkerImages,
    settings: Settings,
}

impl<D: DevinApi, C: Cluster> Verifier<D, C> {
    pub fn new(
        devin: D,
        cluster: C,
        template: PoolTemplate,
        images: WorkerImages,
        settings: Settings,
    ) -> Self {
        Self {
            devin,
            cluster,
            template,
            images,
            settings,
        }
    }

    pub fn cluster(&self) -> &C {
        &self.cluster
    }

    pub fn devin(&self) -> &D {
        &self.devin
    }

    /// Re-run [`Self::run`] every `interval` until it has no failures or
    /// `timeout` elapses; the last report is returned either way. A fresh
    /// install needs a poll interval for the pools and a moment for the
    /// operator and snapshot controller to catch up.
    pub async fn run_until_ok(&self, timeout: Duration, interval: Duration) -> Result<Report> {
        let deadline = Instant::now() + timeout;
        loop {
            let report = self.run().await?;
            if report.ok() || Instant::now() >= deadline {
                return Ok(report);
            }
            tracing::info!(
                failing = report.failures().count(),
                retry_secs = interval.as_secs(),
                "checks failing; retrying"
            );
            tokio::time::sleep(interval).await;
        }
    }

    /// One pass over the checklist. Fails only when the cluster cannot be
    /// read (RBAC, API server); a Devin API error is reported as a failed
    /// check instead, since that is exactly what the test is for.
    pub async fn run(&self) -> Result<Report> {
        let mut report = Report::default();
        let api_url = &self.settings.api_url;

        let orgs: Vec<Organization> = match self.devin.list_organizations().await {
            Ok(all) => {
                let total = all.len();
                let orgs: Vec<_> = all
                    .into_iter()
                    .filter(|o| !self.settings.exclude_org_ids.contains(&o.org_id))
                    .collect();
                report.pass(
                    "devin-api/organizations",
                    format!(
                        "{} organizations listed at {api_url} ({} excluded)",
                        orgs.len(),
                        total - orgs.len()
                    ),
                );
                orgs
            }
            Err(e) => {
                report.fail(
                    "devin-api/organizations",
                    format!("cannot list organizations at {api_url}: {e}"),
                );
                return Ok(report);
            }
        };
        let outposts: Vec<Outpost> = match self.devin.list_outposts().await {
            Ok(o) => {
                report.pass(
                    "devin-api/outposts",
                    format!("{} Outposts in the account", o.len()),
                );
                o
            }
            Err(e) => {
                report.fail("devin-api/outposts", format!("cannot list Outposts: {e}"));
                return Ok(report);
            }
        };
        if orgs.is_empty() {
            report.warn(
                "organizations",
                "the enterprise lists no organizations (after excludeOrgIds); nothing to provision",
            );
        }

        let system_ns = &self.settings.system_namespace;
        let snapshots = self.cluster.list_golden_snapshots(system_ns).await?;
        let namespaces = self.cluster.list_managed_namespaces().await?;
        let pools = self.cluster.list_managed_pools().await?;
        let ns_by_name: BTreeMap<String, &Namespace> =
            namespaces.iter().map(|n| (n.name_any(), n)).collect();
        let pools_by_ns: BTreeMap<String, &OutpostPool> = pools
            .iter()
            .filter(|p| p.name_any() == POOL_NAME)
            .map(|p| (p.namespace().unwrap_or_default(), p))
            .collect();

        let default_image = self
            .template
            .pool
            .worker
            .overrides
            .image
            .clone()
            .ok_or_else(|| Error::Template("pool.worker.overrides.image is required".into()))?;
        let mut images: BTreeSet<&str> = BTreeSet::from([default_image.as_str()]);
        images.extend(orgs.iter().map(|o| self.images.resolve(o, &default_image)));
        let goldens: BTreeMap<&str, Option<String>> = images
            .into_iter()
            .map(|image| (image, self.check_golden(&mut report, &snapshots, image)))
            .collect();

        for org in &orgs {
            let ns_name = naming::namespace(&self.settings.namespace_prefix, &org.org_id);
            let image = self.images.resolve(org, &default_image);
            self.check_org(
                &mut report,
                org,
                &ns_name,
                ns_by_name.get(&ns_name).copied(),
                pools_by_ns.get(&ns_name).copied(),
                &outposts,
                image,
                goldens[image].as_deref(),
            )
            .await?;
        }

        let present: BTreeSet<&str> = orgs.iter().map(|o| o.org_id.as_str()).collect();
        for ns in &namespaces {
            if render::org_id_of(ns).is_none_or(|id| !present.contains(id)) {
                report.warn(
                    format!("{}/orphaned", ns.name_any()),
                    format!(
                        "organization {:?} is no longer listed; the provisioner deletes the namespace after the grace period (orphaned since {})",
                        render::org_id_of(ns).unwrap_or("unknown"),
                        ns.annotations()
                            .get(ANNOTATION_ORPHANED_SINCE)
                            .map(String::as_str)
                            .unwrap_or("next pass")
                    ),
                );
            }
        }
        Ok(report)
    }

    /// The name of the ready golden snapshot for `image`, if any.
    fn check_golden(
        &self,
        report: &mut Report,
        snapshots: &[VolumeSnapshot],
        image: &str,
    ) -> Option<String> {
        let name = format!("golden-snapshot {image}");
        let for_image: Vec<&VolumeSnapshot> = snapshots
            .iter()
            .filter(|s| {
                s.annotations()
                    .get(ANNOTATION_WORKER_IMAGE)
                    .map(String::as_str)
                    == Some(image)
            })
            .collect();
        match for_image.iter().find(|s| s.is_ready()) {
            Some(s) => {
                report.pass(
                    name,
                    format!(
                        "{} is readyToUse in {}",
                        s.name_any(),
                        self.settings.system_namespace
                    ),
                );
                Some(s.name_any())
            }
            None => {
                report.fail(
                    name,
                    format!(
                        "no readyToUse golden VolumeSnapshot for this image in {} ({} exist but are not ready); install devin-golden-home with image={image} and wait for its Job and snapshot",
                        self.settings.system_namespace,
                        for_image.len()
                    ),
                );
                None
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn check_org(
        &self,
        report: &mut Report,
        org: &Organization,
        ns_name: &str,
        namespace: Option<&Namespace>,
        pool: Option<&OutpostPool>,
        outposts: &[Outpost],
        image: &str,
        golden: Option<&str>,
    ) -> Result<()> {
        let org_label = format!("{} ({})", org.name, org.org_id);
        let Some(ns) = namespace else {
            report.fail(
                format!("{ns_name}/namespace"),
                format!(
                    "{org_label}: not provisioned. Wait one poll interval and read the provisioner logs; if Devin refuses to restrict an Outpost to this org (the enterprise-level org) add it to provisioner.excludeOrgIds"
                ),
            );
            return Ok(());
        };
        match ns.annotations().get(ANNOTATION_ORPHANED_SINCE) {
            None => report.pass(format!("{ns_name}/namespace"), org_label.clone()),
            Some(since) => report.fail(
                format!("{ns_name}/namespace"),
                format!(
                    "{org_label}: marked orphaned since {since} although the org is listed; the provisioner clears the marker on its next pass"
                ),
            ),
        }

        let Some(pool) = pool else {
            report.fail(
                format!("{ns_name}/pool"),
                format!(
                    "no OutpostPool {POOL_NAME:?} in the namespace yet; read the provisioner logs"
                ),
            );
            return Ok(());
        };
        match outposts
            .iter()
            .find(|o| o.metadata.outpost_id == pool.spec.pool_id)
        {
            None => report.fail(
                format!("{ns_name}/pool"),
                format!(
                    "poolId {} is not an Outpost of this account; delete the pool so the provisioner rebinds",
                    pool.spec.pool_id
                ),
            ),
            Some(o) if !o.spec.name.starts_with(&self.settings.outpost_name_prefix) => {
                report.fail(
                    format!("{ns_name}/pool"),
                    format!(
                        "bound to Outpost {} ({}), which does not carry this install's prefix {:?}: another cluster's Outpost, or the prefix changed; the provisioner rebinds on its next pass",
                        o.spec.name, o.metadata.outpost_id, self.settings.outpost_name_prefix
                    ),
                )
            }
            Some(o)
                if o.spec
                    .allowed_org_ids
                    .as_ref()
                    .is_some_and(|ids| ids.contains(&org.org_id)) =>
            {
                report.pass(
                    format!("{ns_name}/pool"),
                    format!(
                        "bound to Outpost {} ({}), restricted to this org",
                        o.spec.name, o.metadata.outpost_id
                    ),
                )
            }
            Some(o) => report.warn(
                format!("{ns_name}/pool"),
                format!(
                    "bound to Outpost {} ({}), which is not restricted to this org (allowed_org_ids unset): any org of the account may use it",
                    o.spec.name, o.metadata.outpost_id
                ),
            ),
        }

        match pool.spec.worker.overrides.image.as_deref() {
            Some(current) if current == image => {
                report.pass(format!("{ns_name}/pool/image"), image.to_string())
            }
            current => report.fail(
                format!("{ns_name}/pool/image"),
                format!(
                    "pool runs {current:?}, the rules resolve {image:?}; the provisioner switches it once that image's golden snapshot is ready"
                ),
            ),
        }

        let operator = format!("{ns_name}/operator");
        match pool.status.as_ref() {
            Some(s) if s.phase.as_deref() == Some("Ready") => report.pass(
                operator,
                format!(
                    "operator synced the pool: phase Ready, {} claimed sessions, last synced {}",
                    s.claimed_sessions,
                    s.last_synced.as_deref().unwrap_or("never")
                ),
            ),
            Some(s) => {
                let message = s
                    .conditions
                    .iter()
                    .find_map(|c| c.message.as_deref())
                    .unwrap_or("no condition message");
                report.fail(
                    operator,
                    format!(
                        "pool phase {}: {message}",
                        s.phase.as_deref().unwrap_or("unknown")
                    ),
                )
            }
            None => report.fail(
                operator,
                "pool has no status: the operator has not reconciled it (is the operator Deployment running and watching all namespaces?)",
            ),
        }

        let secret = format!("{ns_name}/token-secret");
        match self.cluster.get_secret(ns_name, TOKEN_SECRET_NAME).await? {
            Some(s)
                if s.data
                    .as_ref()
                    .is_some_and(|d| d.contains_key(TOKEN_SECRET_KEY))
                    || s.string_data
                        .as_ref()
                        .is_some_and(|d| d.contains_key(TOKEN_SECRET_KEY)) =>
            {
                report.pass(
                    secret,
                    format!("Secret {TOKEN_SECRET_NAME} has key {TOKEN_SECRET_KEY}"),
                )
            }
            Some(_) => report.fail(
                secret,
                format!("Secret {TOKEN_SECRET_NAME} has no key {TOKEN_SECRET_KEY}"),
            ),
            None => report.fail(secret, format!("Secret {TOKEN_SECRET_NAME} missing")),
        }

        let binding = format!("{ns_name}/golden-binding");
        if let Some(golden) = golden {
            let data_source = pool
                .spec
                .resume
                .volume_data_source
                .as_ref()
                .map(|d| d.name.as_str());
            match self.cluster.get_volume_snapshot(ns_name, golden).await? {
                None => report.fail(
                    binding,
                    format!("VolumeSnapshot {golden} missing in the namespace"),
                ),
                Some(s) if !s.is_ready() => report.fail(
                    binding,
                    format!(
                        "VolumeSnapshot {golden} is not readyToUse: the snapshot controller has not bound it to VolumeSnapshotContent {} (is the CSI snapshot controller installed?)",
                        s.spec
                            .source
                            .volume_snapshot_content_name
                            .as_deref()
                            .unwrap_or("?")
                    ),
                ),
                Some(_) if data_source == Some(golden) => report.pass(
                    binding,
                    format!("VolumeSnapshot {golden} is readyToUse and the pool clones session volumes from it"),
                ),
                Some(_) => report.fail(
                    binding,
                    format!(
                        "pool clones from {data_source:?}, expected {golden}; the provisioner rewrites it on its next pass"
                    ),
                ),
            }
        } else {
            report.fail(binding, format!("blocked on golden-snapshot {image}"));
        }

        if !self.template.namespace.role_bindings.is_empty() {
            let bindings = self.cluster.list_role_bindings(ns_name).await?;
            for expected in &self.template.namespace.role_bindings {
                let name = format!("{ns_name}/rolebinding/{}", expected.name);
                let subject = expected.subject(ns_name);
                let group = match &subject.namespace {
                    Some(sa_ns) => format!("ServiceAccount {sa_ns}/{}", subject.name),
                    None => subject.name.clone(),
                };
                let found = bindings.iter().find(|b| {
                    b.name_any() == expected.name
                        && b.role_ref.kind == "ClusterRole"
                        && b.role_ref.name == expected.cluster_role
                        && b.subjects.as_ref().is_some_and(|subjects| {
                            subjects.iter().any(|s| {
                                s.kind == subject.kind
                                    && s.name == subject.name
                                    && s.namespace == subject.namespace
                            })
                        })
                });
                match found {
                    Some(_) => report.pass(
                        name,
                        format!("ClusterRole {} bound to {group}", expected.cluster_role),
                    ),
                    None => report.fail(
                        name,
                        format!(
                            "no RoleBinding {} granting ClusterRole {} to {group}; the provisioner's ClusterRole must be allowed to bind it (helm upgrade re-renders the rule)",
                            expected.name, expected.cluster_role
                        ),
                    ),
                }
            }
        }

        let default_platform = format!("{ns_name}/default-platform");
        match self.devin.get_default_platform(&org.org_id).await {
            Ok(current) if current.outpost_pool_id.as_deref() == Some(pool.spec.pool_id.as_str()) => {
                report.pass(default_platform, format!("org default platform is {}", current.describe()))
            }
            Ok(current) if current.is_unset() => report.warn(
                default_platform,
                "org default platform is unset: the provisioner sets it on its next pass, unless its token lacks ManageOrgSettings (then set it in the Devin UI)".to_string(),
            ),
            Ok(current) => report.warn(
                default_platform,
                format!("org default platform is {}, not this Outpost: its sessions run elsewhere unless changed in the Devin UI", current.describe()),
            ),
            Err(e) => report.warn(default_platform, format!("cannot read the org default platform: {e}")),
        }
        Ok(())
    }
}
