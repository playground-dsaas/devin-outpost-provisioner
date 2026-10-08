//! Build the Kubernetes objects for one organization from the pool template.

use std::collections::BTreeMap;

use devin_outposts_k8s::crd::{OutpostPool, OutpostPoolSpec, SecretKeyRef};
use k8s_openapi::api::core::v1::{
    LimitRange, LimitRangeItem, LimitRangeSpec, Namespace, ObjectReference, ResourceQuota,
    ResourceQuotaSpec, Secret, TypedLocalObjectReference,
};
use k8s_openapi::api::networking::v1::{NetworkPolicy, NetworkPolicySpec};
use k8s_openapi::api::rbac::v1::{RoleBinding, RoleRef, Subject};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use kube::ResourceExt;

use crate::devin::Organization;
use crate::naming::{POOL_NAME, TOKEN_SECRET_KEY, TOKEN_SECRET_NAME};
use crate::snapshot::{
    VolumeSnapshot, VolumeSnapshotContent, VolumeSnapshotContentSource, VolumeSnapshotContentSpec,
    VolumeSnapshotSource, VolumeSnapshotSpec,
};
use crate::template::PoolTemplate;

/// Field manager name used for server-side apply.
pub const FIELD_MANAGER: &str = "org-provisioner";

/// Label on every managed object.
pub const LABEL_MANAGED_BY: &str = "app.kubernetes.io/managed-by";
/// Label holding the owning organization ID.
pub const LABEL_ORG_ID: &str = "devin.cognition.com/org-id";
/// Label holding a slug of the display name, for `kubectl get ns -L`.
pub const LABEL_ORG_SLUG: &str = "devin.cognition.com/org-slug";
/// Annotation holding the organization display name at last reconcile.
pub const ANNOTATION_ORG_NAME: &str = "devin.cognition.com/org-name";
/// Annotation holding the Devin Outpost ID bound to this namespace.
pub const ANNOTATION_OUTPOST_ID: &str = "devin.cognition.com/outpost-id";
/// Annotation holding the Devin Outpost name.
pub const ANNOTATION_OUTPOST_NAME: &str = "devin.cognition.com/outpost-name";
/// `"true"` when the Outpost is restricted to this org via `allowed_org_ids`.
pub const ANNOTATION_OUTPOST_RESTRICTED: &str = "devin.cognition.com/outpost-org-restricted";
/// Whether the org's default platform points at this Outpost. Always
/// `"pending"` until an API for it exists; flip manually after setting it in
/// the Devin UI.
pub const ANNOTATION_DEFAULT_PLATFORM: &str = "devin.cognition.com/default-platform";
/// [`ANNOTATION_DEFAULT_PLATFORM`] values: the org's default platform is this
/// Outpost; points elsewhere by choice and is left alone; the API refused to
/// set it (token lacks `ManageOrgSettings`), so the Devin UI is the way.
pub const DEFAULT_PLATFORM_SET: &str = "set";
pub const DEFAULT_PLATFORM_OTHER: &str = "other";
pub const DEFAULT_PLATFORM_PENDING: &str = "pending";
/// RFC 3339 time the org first went missing from the enterprise list.
pub const ANNOTATION_ORPHANED_SINCE: &str = "devin.cognition.com/orphaned-since";
/// Annotation on a golden `VolumeSnapshot` naming the worker image whose
/// home directory it holds (set by `make golden-snapshot`).
pub const ANNOTATION_WORKER_IMAGE: &str = "devin.cognition.com/worker-image";
/// `app.kubernetes.io/name` of the golden volume objects in the system namespace.
pub const GOLDEN_VOLUME_NAME: &str = "worker-home";

/// Name of the per-namespace quota/limit/network objects.
pub const POLICY_NAME: &str = "org";

/// Outpost as bound to an organization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundOutpost {
    pub outpost_id: String,
    pub name: String,
    /// Whether `allowed_org_ids` was accepted for this org.
    pub org_restricted: bool,
}

/// The ready golden snapshot for the template's worker image, as found in the
/// system namespace. `VolumeSnapshot`s are namespaced and a PVC can only clone
/// one from its own namespace, so each org gets a pre-provisioned
/// `VolumeSnapshotContent` + `VolumeSnapshot` pair pointing at the same
/// storage-side snapshot, and the pool clones session volumes from that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoldenSnapshot {
    pub name: String,
    pub driver: String,
    /// Storage-side ID (an EBS `snap-...`).
    pub snapshot_handle: String,
}

impl GoldenSnapshot {
    /// Name of the org's `VolumeSnapshotContent`; cluster-scoped, so it carries
    /// the namespace.
    pub fn content_name(&self, namespace: &str) -> String {
        format!("{}-{namespace}", self.name)
    }
}

/// Inputs for one org's objects.
#[derive(Debug, Clone)]
pub struct RenderInput<'a> {
    pub org: &'a Organization,
    pub namespace: &'a str,
    pub outpost: &'a BoundOutpost,
    pub api_url: &'a str,
    pub template: &'a PoolTemplate,
    /// Worker image for this org: the template's or a per-org profile's.
    pub image: &'a str,
    /// Preserved from the existing pool when set to anything other than
    /// `pending`, so a manual flip is not undone.
    pub default_platform: Option<&'a str>,
    /// Bound into the namespace and set as the pool's `resume.volumeDataSource`.
    pub golden: &'a GoldenSnapshot,
}

/// All objects for one organization namespace.
#[derive(Debug, Clone)]
pub struct Bundle {
    pub namespace: Namespace,
    pub resource_quota: Option<ResourceQuota>,
    pub limit_range: Option<LimitRange>,
    pub network_policy: Option<NetworkPolicy>,
    pub role_bindings: Vec<RoleBinding>,
    /// The golden snapshot bound into this namespace, applied before the pool.
    pub volume_snapshot_content: VolumeSnapshotContent,
    pub volume_snapshot: VolumeSnapshot,
    pub pool: OutpostPool,
}

fn managed_labels(org: &Organization) -> BTreeMap<String, String> {
    BTreeMap::from([
        (LABEL_MANAGED_BY.to_string(), FIELD_MANAGER.to_string()),
        (LABEL_ORG_ID.to_string(), org.org_id.clone()),
    ])
}

fn meta(name: &str, namespace: Option<&str>, org: &Organization) -> ObjectMeta {
    ObjectMeta {
        name: Some(name.to_string()),
        namespace: namespace.map(str::to_string),
        labels: Some(managed_labels(org)),
        ..Default::default()
    }
}

/// Render every object for an organization. Secrets are rendered separately
/// by [`render_token_secret`] so the token never travels with the bundle.
pub fn render(input: &RenderInput<'_>) -> Bundle {
    let RenderInput {
        org,
        namespace,
        outpost,
        api_url,
        template,
        image,
        default_platform,
        golden: g,
    } = input;
    let ns_policy = &template.namespace;

    let mut ns_labels = managed_labels(org);
    ns_labels.insert(LABEL_ORG_SLUG.to_string(), crate::naming::slug(&org.name));
    ns_labels.extend(ns_policy.labels.clone());
    let annotations = BTreeMap::from([
        (ANNOTATION_ORG_NAME.to_string(), org.name.clone()),
        (
            ANNOTATION_OUTPOST_ID.to_string(),
            outpost.outpost_id.clone(),
        ),
        (ANNOTATION_OUTPOST_NAME.to_string(), outpost.name.clone()),
        (
            ANNOTATION_OUTPOST_RESTRICTED.to_string(),
            outpost.org_restricted.to_string(),
        ),
        (
            ANNOTATION_DEFAULT_PLATFORM.to_string(),
            default_platform
                .filter(|v| !v.is_empty())
                .unwrap_or(DEFAULT_PLATFORM_PENDING)
                .to_string(),
        ),
    ]);

    let namespace_obj = Namespace {
        metadata: ObjectMeta {
            name: Some(namespace.to_string()),
            labels: Some(ns_labels),
            annotations: Some(annotations.clone()),
            ..Default::default()
        },
        ..Default::default()
    };

    let resource_quota = ns_policy.resource_quota.clone().map(|hard| ResourceQuota {
        metadata: meta(POLICY_NAME, Some(namespace), org),
        spec: Some(ResourceQuotaSpec {
            hard: Some(hard),
            ..Default::default()
        }),
        ..Default::default()
    });

    let limit_range = ns_policy.limit_range.clone().map(|limits| LimitRange {
        metadata: meta(POLICY_NAME, Some(namespace), org),
        spec: Some(LimitRangeSpec {
            limits: vec![LimitRangeItem {
                type_: "Container".to_string(),
                default: limits.default,
                default_request: limits.default_request,
                max: limits.max,
                min: limits.min,
                ..Default::default()
            }],
        }),
    });

    let network_policy = ns_policy.network_policy.clone().map(|np| NetworkPolicy {
        metadata: meta(POLICY_NAME, Some(namespace), org),
        spec: Some(NetworkPolicySpec {
            pod_selector: Some(LabelSelector::default()),
            policy_types: Some(vec!["Ingress".to_string(), "Egress".to_string()]),
            ingress: Some(np.ingress),
            egress: Some(np.egress),
        }),
    });

    let role_bindings = ns_policy
        .role_bindings
        .iter()
        .map(|b| RoleBinding {
            metadata: meta(&b.name, Some(namespace), org),
            role_ref: RoleRef {
                api_group: "rbac.authorization.k8s.io".to_string(),
                kind: "ClusterRole".to_string(),
                name: b.cluster_role.clone(),
            },
            subjects: Some(vec![Subject {
                api_group: Some("rbac.authorization.k8s.io".to_string()),
                kind: "Group".to_string(),
                name: format!("system:serviceaccounts:{namespace}"),
                namespace: None,
            }]),
        })
        .collect();

    // Retain: the storage-side snapshot belongs to the golden VolumeSnapshot
    // in the system namespace; deleting an org's binding must not delete it.
    let volume_snapshot_content = VolumeSnapshotContent {
        metadata: meta(&g.content_name(namespace), None, org),
        spec: VolumeSnapshotContentSpec {
            deletion_policy: "Retain".to_string(),
            driver: g.driver.clone(),
            source: VolumeSnapshotContentSource {
                snapshot_handle: Some(g.snapshot_handle.clone()),
                volume_handle: None,
            },
            volume_snapshot_ref: ObjectReference {
                name: Some(g.name.clone()),
                namespace: Some(namespace.to_string()),
                ..Default::default()
            },
            volume_snapshot_class_name: None,
            source_volume_mode: Some("Filesystem".to_string()),
        },
        status: None,
    };
    let volume_snapshot = VolumeSnapshot {
        metadata: meta(&g.name, Some(namespace), org),
        spec: VolumeSnapshotSpec {
            source: VolumeSnapshotSource {
                persistent_volume_claim_name: None,
                volume_snapshot_content_name: Some(g.content_name(namespace)),
            },
            volume_snapshot_class_name: None,
        },
        status: None,
    };

    let mut worker = template.pool.worker.clone();
    worker.overrides.image = Some(image.to_string());
    let mut resume = template.pool.resume.clone();
    resume.volume_data_source = Some(TypedLocalObjectReference {
        api_group: Some("snapshot.storage.k8s.io".to_string()),
        kind: "VolumeSnapshot".to_string(),
        name: g.name.clone(),
    });

    let mut pool = OutpostPool::new(
        POOL_NAME,
        OutpostPoolSpec {
            pool_id: outpost.outpost_id.clone(),
            token_secret_ref: SecretKeyRef {
                name: TOKEN_SECRET_NAME.to_string(),
                key: TOKEN_SECRET_KEY.to_string(),
            },
            api_url: Some(api_url.to_string()),
            max_concurrent_sessions: template.pool.max_concurrent_sessions,
            worker,
            resume,
        },
    );
    pool.metadata.namespace = Some(namespace.to_string());
    pool.metadata.labels = Some(managed_labels(org));
    pool.metadata.annotations = Some(annotations);

    Bundle {
        namespace: namespace_obj,
        resource_quota,
        limit_range,
        network_policy,
        role_bindings,
        volume_snapshot_content,
        volume_snapshot,
        pool,
    }
}

/// The token `Secret` the operator reads for this org's pool.
pub fn render_token_secret(org: &Organization, namespace: &str, token: &str) -> Secret {
    Secret {
        metadata: meta(TOKEN_SECRET_NAME, Some(namespace), org),
        type_: Some("Opaque".to_string()),
        string_data: Some(BTreeMap::from([(
            TOKEN_SECRET_KEY.to_string(),
            token.to_string(),
        )])),
        ..Default::default()
    }
}

/// The org ID a managed object belongs to, if labelled.
pub fn org_id_of<K: ResourceExt>(obj: &K) -> Option<&str> {
    obj.labels().get(LABEL_ORG_ID).map(String::as_str)
}

/// Whether an object carries the managed-by label.
pub fn is_managed<K: ResourceExt>(obj: &K) -> bool {
    obj.labels().get(LABEL_MANAGED_BY).map(String::as_str) == Some(FIELD_MANAGER)
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use super::*;

    fn org() -> Organization {
        Organization {
            org_id: "org-2f9990d15a5d4f139af863bdff50b3ae".into(),
            name: "Primary".into(),
            created_at: None,
            updated_at: None,
        }
    }

    fn template() -> PoolTemplate {
        PoolTemplate::parse_helm_values(&[include_str!(
            "../../charts/devin-outposts-platform/values.yaml"
        )])
        .unwrap()
    }

    fn input<'a>(
        org: &'a Organization,
        outpost: &'a BoundOutpost,
        template: &'a PoolTemplate,
    ) -> RenderInput<'a> {
        RenderInput {
            org,
            namespace: "devin-org-primary-2f9990d1",
            outpost,
            api_url: "https://api.devin.ai",
            template,
            image: template.pool.worker.overrides.image.as_deref().unwrap(),
            default_platform: None,
            golden: &GOLDEN,
        }
    }

    static GOLDEN: LazyLock<GoldenSnapshot> = LazyLock::new(|| GoldenSnapshot {
        name: "worker-home-a0f71d0e6a-20261005061956".into(),
        driver: "ebs.csi.aws.com".into(),
        snapshot_handle: "snap-0123456789abcdef0".into(),
    });

    #[test]
    fn pool_binds_outpost_and_secret() {
        let org = org();
        let outpost = BoundOutpost {
            outpost_id: "outpost_abc".into(),
            name: "eks-primary".into(),
            org_restricted: true,
        };
        let t = template();
        let b = render(&input(&org, &outpost, &t));

        assert_eq!(
            b.pool.metadata.namespace.as_deref(),
            Some("devin-org-primary-2f9990d1")
        );
        assert_eq!(b.pool.metadata.name.as_deref(), Some(POOL_NAME));
        assert_eq!(b.pool.spec.pool_id, "outpost_abc");
        assert_eq!(b.pool.spec.token_secret_ref.name, TOKEN_SECRET_NAME);
        assert_eq!(b.pool.spec.token_secret_ref.key, TOKEN_SECRET_KEY);
        assert_eq!(b.pool.spec.api_url.as_deref(), Some("https://api.devin.ai"));
        assert_eq!(b.pool.spec.max_concurrent_sessions, 50);
        assert_eq!(
            b.pool.spec.worker.overrides.command.as_deref(),
            Some(&["/usr/local/bin/outpost-entrypoint.sh".to_string()][..])
        );
        assert_eq!(org_id_of(&b.pool), Some(org.org_id.as_str()));
        assert!(is_managed(&b.pool));
        let ann = b.pool.metadata.annotations.unwrap();
        assert_eq!(ann[ANNOTATION_OUTPOST_ID], "outpost_abc");
        assert_eq!(ann[ANNOTATION_OUTPOST_RESTRICTED], "true");
        assert_eq!(ann[ANNOTATION_DEFAULT_PLATFORM], "pending");
    }

    #[test]
    fn worker_security_contract_is_preserved() {
        let org = org();
        let outpost = BoundOutpost {
            outpost_id: "o".into(),
            name: "n".into(),
            org_restricted: false,
        };
        let t = template();
        let b = render(&input(&org, &outpost, &t));
        let spec = b.pool.spec.worker.template.spec.unwrap();
        let psc = spec.security_context.unwrap();
        assert_eq!(psc.run_as_user, Some(1000));
        assert_eq!(psc.run_as_group, Some(1000));
        assert_eq!(psc.run_as_non_root, Some(true));
        assert_eq!(psc.seccomp_profile.unwrap().type_, "RuntimeDefault");
        let c = spec
            .containers
            .iter()
            .find(|c| c.name == "devin-worker")
            .unwrap();
        let csc = c.security_context.clone().unwrap();
        assert_eq!(csc.allow_privilege_escalation, Some(false));
        assert_eq!(
            csc.capabilities.unwrap().drop,
            Some(vec!["ALL".to_string()])
        );
        assert_eq!(spec.automount_service_account_token, Some(false));
    }

    #[test]
    fn namespace_carries_psa_labels_and_policies() {
        let org = org();
        let outpost = BoundOutpost {
            outpost_id: "o".into(),
            name: "n".into(),
            org_restricted: false,
        };
        let t = template();
        let b = render(&input(&org, &outpost, &t));
        let labels = b.namespace.metadata.labels.unwrap();
        assert_eq!(labels[LABEL_ORG_ID], org.org_id);
        assert_eq!(labels["pod-security.kubernetes.io/enforce"], "restricted");
        assert_eq!(
            b.namespace.metadata.annotations.unwrap()[ANNOTATION_OUTPOST_RESTRICTED],
            "false"
        );
        assert!(b.resource_quota.is_some());
        assert!(b.limit_range.is_some());
        let np = b.network_policy.unwrap().spec.unwrap();
        assert_eq!(np.ingress.unwrap().len(), 0);
        assert_eq!(np.egress.unwrap().len(), 2);
        assert_eq!(np.policy_types.unwrap(), ["Ingress", "Egress"]);
    }

    #[test]
    fn preserves_manual_default_platform_flag() {
        let org = org();
        let outpost = BoundOutpost {
            outpost_id: "o".into(),
            name: "n".into(),
            org_restricted: false,
        };
        let t = template();
        let mut i = input(&org, &outpost, &t);
        i.default_platform = Some("set");
        let b = render(&i);
        assert_eq!(
            b.pool.metadata.annotations.unwrap()[ANNOTATION_DEFAULT_PLATFORM],
            "set"
        );
    }

    #[test]
    fn golden_snapshot_is_bound_into_namespace_and_pool() {
        let org = org();
        let outpost = BoundOutpost {
            outpost_id: "o".into(),
            name: "n".into(),
            org_restricted: false,
        };
        let t = template();
        let g = &*GOLDEN;
        let b = render(&input(&org, &outpost, &t));

        let content = b.volume_snapshot_content;
        assert_eq!(
            content.metadata.name.as_deref(),
            Some("worker-home-a0f71d0e6a-20261005061956-devin-org-primary-2f9990d1")
        );
        assert_eq!(content.metadata.namespace, None);
        assert_eq!(content.spec.deletion_policy, "Retain");
        assert_eq!(content.spec.driver, "ebs.csi.aws.com");
        assert_eq!(
            content.spec.source.snapshot_handle.as_deref(),
            Some("snap-0123456789abcdef0")
        );
        assert_eq!(
            content.spec.volume_snapshot_ref.namespace.as_deref(),
            Some("devin-org-primary-2f9990d1")
        );
        assert_eq!(content.spec.volume_snapshot_ref.name, Some(g.name.clone()));
        assert!(is_managed(&content));

        let snapshot = b.volume_snapshot;
        assert_eq!(snapshot.metadata.name, Some(g.name.clone()));
        assert_eq!(
            snapshot.metadata.namespace.as_deref(),
            Some("devin-org-primary-2f9990d1")
        );
        assert_eq!(
            snapshot.spec.source.volume_snapshot_content_name,
            content.metadata.name
        );

        let source = b.pool.spec.resume.volume_data_source.unwrap();
        assert_eq!(source.api_group.as_deref(), Some("snapshot.storage.k8s.io"));
        assert_eq!(source.kind, "VolumeSnapshot");
        assert_eq!(source.name, g.name);
        assert_eq!(b.pool.spec.resume.volume_size, t.pool.resume.volume_size);
    }

    #[test]
    fn pool_uses_the_resolved_image_not_the_templates() {
        let org = org();
        let outpost = BoundOutpost {
            outpost_id: "o".into(),
            name: "n".into(),
            org_restricted: false,
        };
        let t = template();
        let mut i = input(&org, &outpost, &t);
        i.image = "registry.example/devin-outpost-ds:2026.10";
        let b = render(&i);
        assert_eq!(
            b.pool.spec.worker.overrides.image.as_deref(),
            Some("registry.example/devin-outpost-ds:2026.10")
        );
        assert_eq!(
            b.pool.spec.worker.overrides.command,
            t.pool.worker.overrides.command
        );
    }

    #[test]
    fn token_secret_is_string_data_in_org_namespace() {
        let s = render_token_secret(&org(), "devin-org-x", "cog_secret");
        assert_eq!(s.metadata.namespace.as_deref(), Some("devin-org-x"));
        assert_eq!(s.metadata.name.as_deref(), Some(TOKEN_SECRET_NAME));
        assert_eq!(s.string_data.unwrap()[TOKEN_SECRET_KEY], "cog_secret");
        assert!(s.data.is_none());
    }
}
