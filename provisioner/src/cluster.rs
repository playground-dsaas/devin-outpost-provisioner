//! Kubernetes access behind a small trait so the reconciler can be tested
//! against an in-memory cluster.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use devin_outposts_k8s::crd::{OutpostPool, OutpostPoolStatus};
use k8s_openapi::api::core::v1::{LimitRange, Namespace, ResourceQuota, Secret};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::api::rbac::v1::RoleBinding;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams};
use kube::{Api, Client, Resource, ResourceExt};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::Result;
use crate::render::{FIELD_MANAGER, GOLDEN_VOLUME_NAME, LABEL_MANAGED_BY, LABEL_ORG_ID, org_id_of};
use crate::snapshot::{VolumeSnapshot, VolumeSnapshotContent};

/// The cluster operations the reconciler needs.
#[async_trait]
pub trait Cluster: Send + Sync {
    /// Namespaces carrying the managed-by label.
    async fn list_managed_namespaces(&self) -> Result<Vec<Namespace>>;
    /// `OutpostPool`s carrying the managed-by label, across all namespaces.
    async fn list_managed_pools(&self) -> Result<Vec<OutpostPool>>;

    async fn apply_namespace(&self, obj: &Namespace) -> Result<()>;
    async fn apply_resource_quota(&self, obj: &ResourceQuota) -> Result<()>;
    async fn apply_limit_range(&self, obj: &LimitRange) -> Result<()>;
    async fn apply_network_policy(&self, obj: &NetworkPolicy) -> Result<()>;
    async fn apply_role_binding(&self, obj: &RoleBinding) -> Result<()>;
    async fn apply_secret(&self, obj: &Secret) -> Result<()>;
    async fn apply_volume_snapshot_content(&self, obj: &VolumeSnapshotContent) -> Result<()>;
    async fn apply_volume_snapshot(&self, obj: &VolumeSnapshot) -> Result<()>;
    async fn apply_pool(&self, obj: &OutpostPool) -> Result<()>;

    /// Golden `VolumeSnapshot`s (`app.kubernetes.io/name=worker-home`) in the
    /// system namespace. Empty when the snapshot CRDs are not installed.
    async fn list_golden_snapshots(&self, namespace: &str) -> Result<Vec<VolumeSnapshot>>;
    async fn get_volume_snapshot_content(
        &self,
        name: &str,
    ) -> Result<Option<VolumeSnapshotContent>>;

    // Read-only lookups for `verify`.
    async fn get_volume_snapshot(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<Option<VolumeSnapshot>>;
    async fn get_secret(&self, namespace: &str, name: &str) -> Result<Option<Secret>>;
    /// Managed `RoleBinding`s in one namespace.
    async fn list_role_bindings(&self, namespace: &str) -> Result<Vec<RoleBinding>>;

    /// Set (or with `None`, remove) one annotation on a namespace.
    async fn annotate_namespace(&self, name: &str, key: &str, value: Option<&str>) -> Result<()>;
    /// Delete a pool; missing is not an error.
    async fn delete_pool(&self, namespace: &str, name: &str) -> Result<()>;
    async fn delete_volume_snapshot_content(&self, name: &str) -> Result<()>;
    /// Delete every managed `VolumeSnapshotContent` labelled with `org_id`.
    async fn delete_volume_snapshot_contents_of(&self, org_id: &str) -> Result<()>;
    /// Delete a namespace; missing is not an error.
    async fn delete_namespace(&self, name: &str) -> Result<()>;
}

/// [`Cluster`] backed by a real API server, using server-side apply.
#[derive(Clone)]
pub struct KubeCluster {
    client: Client,
}

impl KubeCluster {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    fn managed_selector() -> ListParams {
        ListParams::default().labels(&format!("{LABEL_MANAGED_BY}={FIELD_MANAGER}"))
    }

    async fn apply<K>(&self, obj: &K) -> Result<()>
    where
        K: Resource<Scope = kube::core::NamespaceResourceScope>
            + Clone
            + std::fmt::Debug
            + Serialize
            + DeserializeOwned,
        K::DynamicType: Default,
    {
        let ns = obj
            .meta()
            .namespace
            .as_deref()
            .expect("namespaced object rendered without namespace");
        let api: Api<K> = Api::namespaced(self.client.clone(), ns);
        api.patch(&obj.name_any(), &Self::apply_params(), &Patch::Apply(obj))
            .await?;
        Ok(())
    }

    fn apply_params() -> PatchParams {
        PatchParams::apply(FIELD_MANAGER).force()
    }

    fn ignore_not_found(res: kube::Result<impl Sized>) -> Result<()> {
        match res {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[async_trait]
impl Cluster for KubeCluster {
    async fn list_managed_namespaces(&self) -> Result<Vec<Namespace>> {
        let api: Api<Namespace> = Api::all(self.client.clone());
        Ok(api.list(&Self::managed_selector()).await?.items)
    }

    async fn list_managed_pools(&self) -> Result<Vec<OutpostPool>> {
        let api: Api<OutpostPool> = Api::all(self.client.clone());
        Ok(api.list(&Self::managed_selector()).await?.items)
    }

    async fn apply_namespace(&self, obj: &Namespace) -> Result<()> {
        let api: Api<Namespace> = Api::all(self.client.clone());
        api.patch(&obj.name_any(), &Self::apply_params(), &Patch::Apply(obj))
            .await?;
        Ok(())
    }

    async fn apply_resource_quota(&self, obj: &ResourceQuota) -> Result<()> {
        self.apply(obj).await
    }

    async fn apply_limit_range(&self, obj: &LimitRange) -> Result<()> {
        self.apply(obj).await
    }

    async fn apply_network_policy(&self, obj: &NetworkPolicy) -> Result<()> {
        self.apply(obj).await
    }

    async fn apply_role_binding(&self, obj: &RoleBinding) -> Result<()> {
        self.apply(obj).await
    }

    async fn apply_secret(&self, obj: &Secret) -> Result<()> {
        self.apply(obj).await
    }

    async fn apply_volume_snapshot_content(&self, obj: &VolumeSnapshotContent) -> Result<()> {
        let api: Api<VolumeSnapshotContent> = Api::all(self.client.clone());
        api.patch(&obj.name_any(), &Self::apply_params(), &Patch::Apply(obj))
            .await?;
        Ok(())
    }

    async fn apply_volume_snapshot(&self, obj: &VolumeSnapshot) -> Result<()> {
        self.apply(obj).await
    }

    async fn apply_pool(&self, obj: &OutpostPool) -> Result<()> {
        self.apply(obj).await
    }

    async fn list_golden_snapshots(&self, namespace: &str) -> Result<Vec<VolumeSnapshot>> {
        let api: Api<VolumeSnapshot> = Api::namespaced(self.client.clone(), namespace);
        let lp =
            ListParams::default().labels(&format!("app.kubernetes.io/name={GOLDEN_VOLUME_NAME}"));
        match api.list(&lp).await {
            Ok(list) => Ok(list.items),
            Err(kube::Error::Api(e)) if e.code == 404 => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    async fn get_volume_snapshot_content(
        &self,
        name: &str,
    ) -> Result<Option<VolumeSnapshotContent>> {
        let api: Api<VolumeSnapshotContent> = Api::all(self.client.clone());
        Ok(api.get_opt(name).await?)
    }

    async fn get_volume_snapshot(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<Option<VolumeSnapshot>> {
        let api: Api<VolumeSnapshot> = Api::namespaced(self.client.clone(), namespace);
        Ok(api.get_opt(name).await?)
    }

    async fn get_secret(&self, namespace: &str, name: &str) -> Result<Option<Secret>> {
        let api: Api<Secret> = Api::namespaced(self.client.clone(), namespace);
        Ok(api.get_opt(name).await?)
    }

    async fn list_role_bindings(&self, namespace: &str) -> Result<Vec<RoleBinding>> {
        let api: Api<RoleBinding> = Api::namespaced(self.client.clone(), namespace);
        Ok(api.list(&Self::managed_selector()).await?.items)
    }

    async fn annotate_namespace(&self, name: &str, key: &str, value: Option<&str>) -> Result<()> {
        let api: Api<Namespace> = Api::all(self.client.clone());
        let patch = serde_json::json!({
            "metadata": { "annotations": { key: value } }
        });
        api.patch(name, &PatchParams::default(), &Patch::Merge(&patch))
            .await?;
        Ok(())
    }

    async fn delete_pool(&self, namespace: &str, name: &str) -> Result<()> {
        let api: Api<OutpostPool> = Api::namespaced(self.client.clone(), namespace);
        Self::ignore_not_found(api.delete(name, &DeleteParams::default()).await)
    }

    async fn delete_namespace(&self, name: &str) -> Result<()> {
        let api: Api<Namespace> = Api::all(self.client.clone());
        Self::ignore_not_found(api.delete(name, &DeleteParams::default()).await)
    }

    async fn delete_volume_snapshot_content(&self, name: &str) -> Result<()> {
        let api: Api<VolumeSnapshotContent> = Api::all(self.client.clone());
        Self::ignore_not_found(api.delete(name, &DeleteParams::default()).await)
    }

    async fn delete_volume_snapshot_contents_of(&self, org_id: &str) -> Result<()> {
        let api: Api<VolumeSnapshotContent> = Api::all(self.client.clone());
        let lp = ListParams::default().labels(&format!(
            "{LABEL_MANAGED_BY}={FIELD_MANAGER},{LABEL_ORG_ID}={org_id}"
        ));
        api.delete_collection(&DeleteParams::default(), &lp).await?;
        Ok(())
    }
}

/// In-memory [`Cluster`] for tests. Namespaces own their pools: deleting a
/// namespace drops its pool, quota, limits, policy and secret.
#[derive(Default)]
pub struct MemCluster {
    inner: Mutex<MemState>,
}

#[derive(Default)]
struct MemState {
    namespaces: BTreeMap<String, Namespace>,
    pools: BTreeMap<(String, String), OutpostPool>,
    quotas: BTreeMap<(String, String), ResourceQuota>,
    limits: BTreeMap<(String, String), LimitRange>,
    policies: BTreeMap<(String, String), NetworkPolicy>,
    role_bindings: BTreeMap<(String, String), RoleBinding>,
    secrets: BTreeMap<(String, String), Secret>,
    snapshots: BTreeMap<(String, String), VolumeSnapshot>,
    contents: BTreeMap<String, VolumeSnapshotContent>,
    /// Pools whose deletion is held back, emulating an operator finalizer.
    finalized_pools: Vec<(String, String)>,
    /// Every mutating call, for assertions.
    log: Vec<String>,
}

fn key<K: ResourceExt>(obj: &K) -> (String, String) {
    (obj.namespace().unwrap_or_default(), obj.name_any())
}

impl MemCluster {
    pub fn namespace(&self, name: &str) -> Option<Namespace> {
        self.inner.lock().unwrap().namespaces.get(name).cloned()
    }

    pub fn pool(&self, namespace: &str) -> Option<OutpostPool> {
        self.inner
            .lock()
            .unwrap()
            .pools
            .get(&(namespace.to_string(), crate::naming::POOL_NAME.to_string()))
            .cloned()
    }

    pub fn secret(&self, namespace: &str, name: &str) -> Option<Secret> {
        self.inner
            .lock()
            .unwrap()
            .secrets
            .get(&(namespace.to_string(), name.to_string()))
            .cloned()
    }

    pub fn volume_snapshot(&self, namespace: &str, name: &str) -> Option<VolumeSnapshot> {
        self.inner
            .lock()
            .unwrap()
            .snapshots
            .get(&(namespace.to_string(), name.to_string()))
            .cloned()
    }

    pub fn volume_snapshot_content(&self, name: &str) -> Option<VolumeSnapshotContent> {
        self.inner.lock().unwrap().contents.get(name).cloned()
    }

    /// A golden snapshot as `make golden-snapshot` leaves it: bound to a
    /// content whose status carries the storage handle, ready or not.
    pub fn add_golden_snapshot(
        &self,
        namespace: &str,
        name: &str,
        worker_image: &str,
        snapshot_handle: &str,
        ready: bool,
    ) {
        use crate::snapshot::{
            VolumeSnapshotContentSource, VolumeSnapshotContentSpec, VolumeSnapshotContentStatus,
            VolumeSnapshotSource, VolumeSnapshotSpec, VolumeSnapshotStatus,
        };
        let content_name = format!("snapcontent-{name}");
        let mut snapshot = VolumeSnapshot::new(
            name,
            VolumeSnapshotSpec {
                source: VolumeSnapshotSource {
                    persistent_volume_claim_name: Some(name.to_string()),
                    volume_snapshot_content_name: None,
                },
                volume_snapshot_class_name: Some("ebs".to_string()),
            },
        );
        snapshot.metadata.namespace = Some(namespace.to_string());
        snapshot.labels_mut().insert(
            "app.kubernetes.io/name".to_string(),
            GOLDEN_VOLUME_NAME.to_string(),
        );
        snapshot.annotations_mut().insert(
            crate::render::ANNOTATION_WORKER_IMAGE.to_string(),
            worker_image.to_string(),
        );
        snapshot.status = Some(VolumeSnapshotStatus {
            bound_volume_snapshot_content_name: Some(content_name.clone()),
            ready_to_use: Some(ready),
        });
        let mut content = VolumeSnapshotContent::new(
            &content_name,
            VolumeSnapshotContentSpec {
                deletion_policy: "Delete".to_string(),
                driver: "ebs.csi.aws.com".to_string(),
                source: VolumeSnapshotContentSource {
                    snapshot_handle: None,
                    volume_handle: Some("vol-1".to_string()),
                },
                volume_snapshot_ref: Default::default(),
                volume_snapshot_class_name: Some("ebs".to_string()),
                source_volume_mode: Some("Filesystem".to_string()),
            },
        );
        content.status = Some(VolumeSnapshotContentStatus {
            snapshot_handle: Some(snapshot_handle.to_string()),
            ready_to_use: Some(ready),
        });
        let mut s = self.inner.lock().unwrap();
        s.snapshots
            .insert((namespace.to_string(), name.to_string()), snapshot);
        s.contents.insert(content_name, content);
    }

    /// What the operator writes once it has reconciled the pool.
    pub fn set_pool_status(&self, namespace: &str, status: OutpostPoolStatus) {
        let mut s = self.inner.lock().unwrap();
        if let Some(p) = s
            .pools
            .get_mut(&(namespace.to_string(), crate::naming::POOL_NAME.to_string()))
        {
            p.status = Some(status);
        }
    }

    /// What the snapshot controller does to a pre-provisioned binding.
    /// Bind like the snapshot controller: mark the snapshot ready and stamp
    /// its uid into the content's `volumeSnapshotRef`.
    pub fn mark_volume_snapshot_ready(&self, namespace: &str, name: &str) {
        let mut s = self.inner.lock().unwrap();
        let uid = format!("uid-{namespace}-{name}");
        let Some(v) = s
            .snapshots
            .get_mut(&(namespace.to_string(), name.to_string()))
        else {
            return;
        };
        v.metadata.uid = Some(uid.clone());
        let content_name = v.spec.source.volume_snapshot_content_name.clone();
        v.status = Some(crate::snapshot::VolumeSnapshotStatus {
            bound_volume_snapshot_content_name: content_name.clone(),
            ready_to_use: Some(true),
        });
        if let Some(c) = content_name.and_then(|n| s.contents.get_mut(&n)) {
            c.spec.volume_snapshot_ref.uid = Some(uid);
        }
    }

    pub fn has_quota(&self, namespace: &str) -> bool {
        let s = self.inner.lock().unwrap();
        s.quotas.keys().any(|(ns, _)| ns == namespace)
    }

    pub fn has_network_policy(&self, namespace: &str) -> bool {
        let s = self.inner.lock().unwrap();
        s.policies.keys().any(|(ns, _)| ns == namespace)
    }

    pub fn role_bindings(&self, namespace: &str) -> Vec<RoleBinding> {
        let s = self.inner.lock().unwrap();
        s.role_bindings
            .iter()
            .filter(|((ns, _), _)| ns == namespace)
            .map(|(_, b)| b.clone())
            .collect()
    }

    pub fn namespace_names(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .namespaces
            .keys()
            .cloned()
            .collect()
    }

    /// Hold the next deletion of this pool, as the operator's finalizer would
    /// while it releases claims.
    pub fn hold_pool_deletion(&self, namespace: &str) {
        self.inner
            .lock()
            .unwrap()
            .finalized_pools
            .push((namespace.to_string(), crate::naming::POOL_NAME.to_string()));
    }

    pub fn release_pool_deletion(&self, namespace: &str) {
        let mut s = self.inner.lock().unwrap();
        s.finalized_pools.retain(|(ns, _)| ns != namespace);
        s.pools
            .retain(|(ns, _), p| ns != namespace || p.metadata.deletion_timestamp.is_none());
    }

    pub fn log(&self) -> Vec<String> {
        self.inner.lock().unwrap().log.clone()
    }

    fn record(s: &mut MemState, entry: String) {
        s.log.push(entry);
    }
}

#[async_trait]
impl Cluster for MemCluster {
    async fn list_managed_namespaces(&self) -> Result<Vec<Namespace>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .namespaces
            .values()
            .filter(|n| crate::render::is_managed(*n))
            .cloned()
            .collect())
    }

    async fn list_managed_pools(&self) -> Result<Vec<OutpostPool>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .pools
            .values()
            .filter(|p| crate::render::is_managed(*p))
            .cloned()
            .collect())
    }

    async fn apply_namespace(&self, obj: &Namespace) -> Result<()> {
        let mut s = self.inner.lock().unwrap();
        let name = obj.name_any();
        // Server-side apply keeps annotations owned by other managers; emulate
        // that for the orphaned-since marker.
        let mut obj = obj.clone();
        if let Some(existing) = s.namespaces.get(&name)
            && let Some(v) = existing
                .annotations()
                .get(crate::render::ANNOTATION_ORPHANED_SINCE)
        {
            obj.annotations_mut()
                .insert(crate::render::ANNOTATION_ORPHANED_SINCE.into(), v.clone());
        }
        Self::record(&mut s, format!("apply Namespace/{name}"));
        s.namespaces.insert(name, obj);
        Ok(())
    }

    async fn apply_resource_quota(&self, obj: &ResourceQuota) -> Result<()> {
        let mut s = self.inner.lock().unwrap();
        Self::record(&mut s, format!("apply ResourceQuota/{}", obj.name_any()));
        s.quotas.insert(key(obj), obj.clone());
        Ok(())
    }

    async fn apply_limit_range(&self, obj: &LimitRange) -> Result<()> {
        let mut s = self.inner.lock().unwrap();
        Self::record(&mut s, format!("apply LimitRange/{}", obj.name_any()));
        s.limits.insert(key(obj), obj.clone());
        Ok(())
    }

    async fn apply_network_policy(&self, obj: &NetworkPolicy) -> Result<()> {
        let mut s = self.inner.lock().unwrap();
        Self::record(&mut s, format!("apply NetworkPolicy/{}", obj.name_any()));
        s.policies.insert(key(obj), obj.clone());
        Ok(())
    }

    async fn apply_role_binding(&self, obj: &RoleBinding) -> Result<()> {
        let mut s = self.inner.lock().unwrap();
        Self::record(&mut s, format!("apply RoleBinding/{}", obj.name_any()));
        s.role_bindings.insert(key(obj), obj.clone());
        Ok(())
    }

    async fn apply_secret(&self, obj: &Secret) -> Result<()> {
        let mut s = self.inner.lock().unwrap();
        Self::record(&mut s, format!("apply Secret/{}", obj.name_any()));
        s.secrets.insert(key(obj), obj.clone());
        Ok(())
    }

    async fn apply_volume_snapshot_content(&self, obj: &VolumeSnapshotContent) -> Result<()> {
        let mut s = self.inner.lock().unwrap();
        Self::record(
            &mut s,
            format!("apply VolumeSnapshotContent/{}", obj.name_any()),
        );
        s.contents.insert(obj.name_any(), obj.clone());
        Ok(())
    }

    async fn apply_volume_snapshot(&self, obj: &VolumeSnapshot) -> Result<()> {
        let mut s = self.inner.lock().unwrap();
        Self::record(&mut s, format!("apply VolumeSnapshot/{}", obj.name_any()));
        s.snapshots.insert(key(obj), obj.clone());
        Ok(())
    }

    async fn apply_pool(&self, obj: &OutpostPool) -> Result<()> {
        let mut s = self.inner.lock().unwrap();
        Self::record(&mut s, format!("apply OutpostPool/{}", obj.name_any()));
        // `status` is a subresource: applying the spec leaves it as the operator set it.
        let mut obj = obj.clone();
        obj.status = s.pools.get(&key(&obj)).and_then(|p| p.status.clone());
        s.pools.insert(key(&obj), obj);
        Ok(())
    }

    async fn list_golden_snapshots(&self, namespace: &str) -> Result<Vec<VolumeSnapshot>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .snapshots
            .values()
            .filter(|v| {
                v.namespace().as_deref() == Some(namespace)
                    && v.labels().get("app.kubernetes.io/name").map(String::as_str)
                        == Some(GOLDEN_VOLUME_NAME)
            })
            .cloned()
            .collect())
    }

    async fn get_volume_snapshot_content(
        &self,
        name: &str,
    ) -> Result<Option<VolumeSnapshotContent>> {
        Ok(self.inner.lock().unwrap().contents.get(name).cloned())
    }

    async fn get_volume_snapshot(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<Option<VolumeSnapshot>> {
        Ok(self.volume_snapshot(namespace, name))
    }

    async fn get_secret(&self, namespace: &str, name: &str) -> Result<Option<Secret>> {
        Ok(self.secret(namespace, name))
    }

    async fn list_role_bindings(&self, namespace: &str) -> Result<Vec<RoleBinding>> {
        Ok(self.role_bindings(namespace))
    }

    async fn annotate_namespace(&self, name: &str, key: &str, value: Option<&str>) -> Result<()> {
        let mut s = self.inner.lock().unwrap();
        Self::record(&mut s, format!("annotate Namespace/{name} {key}={value:?}"));
        if let Some(ns) = s.namespaces.get_mut(name) {
            match value {
                Some(v) => {
                    ns.annotations_mut().insert(key.to_string(), v.to_string());
                }
                None => {
                    ns.annotations_mut().remove(key);
                }
            }
        }
        Ok(())
    }

    async fn delete_pool(&self, namespace: &str, name: &str) -> Result<()> {
        let mut s = self.inner.lock().unwrap();
        Self::record(&mut s, format!("delete OutpostPool/{namespace}/{name}"));
        let k = (namespace.to_string(), name.to_string());
        if s.finalized_pools.contains(&k) {
            if let Some(p) = s.pools.get_mut(&k) {
                p.metadata.deletion_timestamp =
                    Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                        k8s_openapi::jiff::Timestamp::now(),
                    ));
            }
        } else {
            s.pools.remove(&k);
        }
        Ok(())
    }

    async fn delete_namespace(&self, name: &str) -> Result<()> {
        let mut s = self.inner.lock().unwrap();
        Self::record(&mut s, format!("delete Namespace/{name}"));
        s.namespaces.remove(name);
        s.pools.retain(|(ns, _), _| ns != name);
        s.quotas.retain(|(ns, _), _| ns != name);
        s.limits.retain(|(ns, _), _| ns != name);
        s.policies.retain(|(ns, _), _| ns != name);
        s.snapshots.retain(|(ns, _), _| ns != name);
        s.secrets.retain(|(ns, _), _| ns != name);
        Ok(())
    }

    async fn delete_volume_snapshot_content(&self, name: &str) -> Result<()> {
        let mut s = self.inner.lock().unwrap();
        Self::record(&mut s, format!("delete VolumeSnapshotContent/{name}"));
        s.contents.remove(name);
        Ok(())
    }

    async fn delete_volume_snapshot_contents_of(&self, org_id: &str) -> Result<()> {
        let mut s = self.inner.lock().unwrap();
        Self::record(&mut s, format!("delete VolumeSnapshotContents of {org_id}"));
        s.contents.retain(|_, c| org_id_of(c) != Some(org_id));
        Ok(())
    }
}
