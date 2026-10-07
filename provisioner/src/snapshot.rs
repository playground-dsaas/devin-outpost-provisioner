//! The `snapshot.storage.k8s.io/v1` objects used to clone session volumes
//! from the golden snapshot (k8s-openapi does not ship them). Only the
//! fields read or written here.

use k8s_openapi::api::core::v1::ObjectReference;
use kube::CustomResource;
use serde::{Deserialize, Serialize};

#[derive(CustomResource, Clone, Debug, Default, Serialize, Deserialize)]
#[kube(
    group = "snapshot.storage.k8s.io",
    version = "v1",
    kind = "VolumeSnapshot",
    namespaced,
    status = "VolumeSnapshotStatus",
    schema = "disabled"
)]
#[serde(rename_all = "camelCase")]
pub struct VolumeSnapshotSpec {
    pub source: VolumeSnapshotSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_snapshot_class_name: Option<String>,
}

/// Exactly one of the two is set: a PVC for a dynamic snapshot, a content
/// for a pre-provisioned one.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeSnapshotSource {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistent_volume_claim_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_snapshot_content_name: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeSnapshotStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_volume_snapshot_content_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_to_use: Option<bool>,
}

impl VolumeSnapshot {
    pub fn is_ready(&self) -> bool {
        self.status
            .as_ref()
            .is_some_and(|s| s.ready_to_use == Some(true))
    }
}

#[derive(CustomResource, Clone, Debug, Default, Serialize, Deserialize)]
#[kube(
    group = "snapshot.storage.k8s.io",
    version = "v1",
    kind = "VolumeSnapshotContent",
    status = "VolumeSnapshotContentStatus",
    schema = "disabled"
)]
#[serde(rename_all = "camelCase")]
pub struct VolumeSnapshotContentSpec {
    /// `Delete` or `Retain`: what happens to the storage-side snapshot when
    /// this object is deleted.
    pub deletion_policy: String,
    pub driver: String,
    pub source: VolumeSnapshotContentSource,
    pub volume_snapshot_ref: ObjectReference,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_snapshot_class_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_volume_mode: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeSnapshotContentSource {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_handle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_handle: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeSnapshotContentStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_handle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_to_use: Option<bool>,
}

impl VolumeSnapshotContent {
    /// The storage-side snapshot ID (an EBS `snap-...` on AWS).
    pub fn snapshot_handle(&self) -> Option<&str> {
        self.status
            .as_ref()
            .and_then(|s| s.snapshot_handle.as_deref())
            .or(self.spec.source.snapshot_handle.as_deref())
    }
}
