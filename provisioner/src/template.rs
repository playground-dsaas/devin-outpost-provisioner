//! The pool template: everything about an org's namespace and `OutpostPool`
//! that is *not* derived from the organization itself.
//!
//! Loaded from a YAML file (mounted from a `ConfigMap` in the cluster). The
//! `pool` section reuses the operator's own CRD types so a template that
//! deserializes here is guaranteed to be a valid `OutpostPool.spec` at the
//! pinned operator commit.

use std::collections::BTreeMap;
use std::path::Path;

use devin_outposts_k8s::crd::{ResumeConfig, WorkerTemplate};
use k8s_openapi::api::networking::v1::{NetworkPolicyEgressRule, NetworkPolicyIngressRule};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value;

use crate::error::{Error, Result};

/// Top-level template document.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PoolTemplate {
    /// Per-org `OutpostPool.spec` fields other than `poolId`, `tokenSecretRef`
    /// and `apiUrl`, which the provisioner fills in.
    pub pool: PoolSpecTemplate,
    /// Namespace-scoped guard rails applied to every org namespace.
    #[serde(default)]
    pub namespace: NamespacePolicy,
}

/// The org-independent part of `OutpostPool.spec`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PoolSpecTemplate {
    /// Concurrency cap per organization.
    pub max_concurrent_sessions: u32,
    /// Resume/snapshot policy.
    #[serde(default)]
    pub resume: ResumeConfig,
    /// Worker pod template, container name and overrides (image, command).
    pub worker: WorkerTemplate,
}

/// Guard rails stamped into every org namespace.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NamespacePolicy {
    /// Extra labels merged onto the namespace (Pod Security Admission labels
    /// belong here).
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// `ResourceQuota.spec.hard`; omit to create no quota.
    #[serde(default)]
    pub resource_quota: Option<BTreeMap<String, Quantity>>,
    /// `LimitRange` for containers; omit to create none.
    #[serde(default)]
    pub limit_range: Option<ContainerLimits>,
    /// `NetworkPolicy` applied to every pod in the namespace; omit to create
    /// none.
    #[serde(default)]
    pub network_policy: Option<NetworkPolicyTemplate>,
    /// `RoleBinding`s granting a ClusterRole to every ServiceAccount in the
    /// namespace (OpenShift: `system:openshift:scc:<scc>` lets the workers
    /// keep their fixed uid). Omit on clusters without such a requirement.
    #[serde(default)]
    pub role_bindings: Vec<NamespaceRoleBinding>,
}

/// One namespace-wide grant of a ClusterRole.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NamespaceRoleBinding {
    /// `RoleBinding` name.
    pub name: String,
    /// The ClusterRole bound; the provisioner's own ClusterRole must be allowed
    /// to `bind` it.
    pub cluster_role: String,
}

/// One container-type `LimitRangeItem`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContainerLimits {
    #[serde(default)]
    pub default: Option<BTreeMap<String, Quantity>>,
    #[serde(default)]
    pub default_request: Option<BTreeMap<String, Quantity>>,
    #[serde(default)]
    pub max: Option<BTreeMap<String, Quantity>>,
    #[serde(default)]
    pub min: Option<BTreeMap<String, Quantity>>,
}

/// Ingress/egress rules of the namespace-wide `NetworkPolicy`. Empty lists
/// deny that direction entirely.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetworkPolicyTemplate {
    #[serde(default)]
    pub ingress: Vec<NetworkPolicyIngressRule>,
    #[serde(default)]
    pub egress: Vec<NetworkPolicyEgressRule>,
}

impl PoolTemplate {
    /// Parse a template document.
    pub fn parse(yaml: &str) -> Result<Self> {
        let template: Self =
            serde_yaml_ng::from_str(yaml).map_err(|e| Error::Template(e.to_string()))?;
        template.validate()?;
        Ok(template)
    }

    /// Load and parse a template file.
    pub fn load(path: &Path) -> Result<Self> {
        let yaml = std::fs::read_to_string(path)
            .map_err(|e| Error::Template(format!("{}: {e}", path.display())))?;
        Self::parse(&yaml)
    }

    /// Parse the template out of Helm values documents, as the platform chart
    /// ships it (the `poolTemplate` key, see [`helm_values_key`]).
    pub fn parse_helm_values(values: &[&str]) -> Result<Self> {
        Self::parse(&helm_values_key(values, "poolTemplate")?)
    }

    fn validate(&self) -> Result<()> {
        if self.pool.max_concurrent_sessions == 0 {
            return Err(Error::Template(
                "pool.maxConcurrentSessions must be > 0".into(),
            ));
        }
        if self.pool.worker.overrides.image.is_none() {
            return Err(Error::Template(
                "pool.worker.overrides.image is required (the enterprise worker image)".into(),
            ));
        }
        Ok(())
    }
}

/// One top-level key of Helm values documents, as YAML. The documents are
/// deep-merged in order the way `helm -f a -f b` merges them (later mappings
/// win key by key, `null` removes a key, lists and scalars replace).
pub fn helm_values_key(values: &[&str], key: &str) -> Result<String> {
    let mut merged = Value::Null;
    for doc in values {
        let v: Value = serde_yaml_ng::from_str(doc).map_err(|e| Error::Template(e.to_string()))?;
        merged = merge_values(merged, v);
    }
    let section = match merged {
        Value::Mapping(mut m) => m.remove(key),
        _ => None,
    }
    .ok_or_else(|| Error::Template(format!("Helm values have no {key}")))?;
    serde_yaml_ng::to_string(&section).map_err(|e| Error::Template(e.to_string()))
}

fn merge_values(base: Value, over: Value) -> Value {
    match (base, over) {
        (Value::Mapping(mut b), Value::Mapping(o)) => {
            for (k, v) in o {
                let existing = b.remove(&k);
                if v.is_null() {
                    continue;
                }
                let merged = match existing {
                    Some(e) => merge_values(e, v),
                    None => v,
                };
                b.insert(k, merged);
            }
            Value::Mapping(b)
        }
        (_, o) => o,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The platform chart's generic defaults and the OpenShift example overrides.
    const CHART_VALUES: &str = include_str!("../../charts/devin-outposts-platform/values.yaml");
    const OPENSHIFT_VALUES: &str =
        include_str!("../../charts/devin-outposts-platform/values-openshift.yaml");

    const MINIMAL: &str = r#"
pool:
  maxConcurrentSessions: 5
  worker:
    overrides:
      image: registry.example/devin-worker@sha256:0000
"#;

    #[test]
    fn parses_minimal_template() {
        let t = PoolTemplate::parse(MINIMAL).unwrap();
        assert_eq!(t.pool.max_concurrent_sessions, 5);
        assert_eq!(t.pool.worker.container_name, "devin-worker");
        assert!(t.namespace.resource_quota.is_none());
    }

    #[test]
    fn rejects_missing_image_and_zero_concurrency() {
        let err = PoolTemplate::parse("pool:\n  maxConcurrentSessions: 1\n  worker: {}\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("image"), "{err}");
        let err = PoolTemplate::parse(&MINIMAL.replace("5", "0"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("maxConcurrentSessions"), "{err}");
    }

    #[test]
    fn rejects_unknown_fields() {
        let err = PoolTemplate::parse(&format!("{MINIMAL}  poolId: nope\n"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("poolId"), "{err}");
    }

    #[test]
    fn merges_helm_values_like_helm() {
        let base = "poolTemplate:\n  pool:\n    maxConcurrentSessions: 5\n    resume: {storageClassName: a, volumeSize: 1Gi}\n    worker:\n      template: {spec: {tolerations: [{key: x}]}}\n      overrides: {image: img, command: [a, b]}\n";
        let over = "poolTemplate:\n  pool:\n    resume: {storageClassName: b}\n    worker:\n      template: {spec: {tolerations: [{key: y}, {key: z}]}}\n      overrides: {command: null}\n";
        let t = PoolTemplate::parse_helm_values(&[base, over]).unwrap();
        assert_eq!(t.pool.resume.storage_class_name.as_deref(), Some("b"));
        assert_eq!(t.pool.resume.volume_size.as_deref(), Some("1Gi"));
        let spec = t.pool.worker.template.spec.unwrap();
        let keys: Vec<_> = spec
            .tolerations
            .unwrap()
            .into_iter()
            .map(|t| t.key)
            .collect();
        assert_eq!(keys, vec![Some("y".into()), Some("z".into())]);
        assert!(t.pool.worker.overrides.command.is_none());
        assert_eq!(t.pool.worker.overrides.image.as_deref(), Some("img"));

        let err = PoolTemplate::parse_helm_values(&["operator: {}\n"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("poolTemplate"), "{err}");
    }

    #[test]
    fn parses_openshift_values() {
        let t = PoolTemplate::parse_helm_values(&[CHART_VALUES, OPENSHIFT_VALUES]).unwrap();
        assert_eq!(t.pool.resume.storage_class_name.as_deref(), Some("isilon"));
        assert_eq!(
            t.namespace
                .role_bindings
                .iter()
                .map(|b| b.cluster_role.as_str())
                .collect::<Vec<_>>(),
            vec!["system:openshift:scc:nonroot-v2"]
        );
        assert!(
            t.pool
                .worker
                .overrides
                .image
                .unwrap()
                .starts_with("registry.example.com/")
        );
    }

    #[test]
    fn parses_shipped_template() {
        let t = PoolTemplate::parse_helm_values(&[CHART_VALUES]).unwrap();
        let ns = &t.namespace;
        assert!(ns.resource_quota.is_some());
        assert!(ns.limit_range.is_some());
        assert!(ns.network_policy.is_some());
        assert!(ns.role_bindings.is_empty());
        assert_eq!(
            ns.labels
                .get("pod-security.kubernetes.io/enforce")
                .map(String::as_str),
            Some("restricted")
        );
        assert_eq!(
            t.pool.worker.overrides.command.as_deref(),
            Some(&["/usr/local/bin/outpost-entrypoint.sh".to_string()][..])
        );

        // Session persistence: the operator mounts the state volume as the
        // worker's home, so the template needs no init container; it only
        // adds Homebrew's prefix from the same volume.
        let spec = t.pool.worker.template.spec.as_ref().unwrap();
        assert!(spec.init_containers.is_none());
        let worker = spec
            .containers
            .iter()
            .find(|c| c.name == t.pool.worker.container_name)
            .unwrap();
        let mounts: Vec<(&str, Option<&str>)> = worker
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .filter(|m| m.name == "outpost-state")
            .map(|m| (m.mount_path.as_str(), m.sub_path.as_deref()))
            .collect();
        assert_eq!(
            mounts,
            vec![("/home/linuxbrew/.linuxbrew", Some(".linuxbrew"))]
        );
    }
}
