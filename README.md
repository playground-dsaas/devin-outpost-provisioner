# devin-outpost-provisioner

Runs Devin Outposts on a customer cluster with one system namespace and one
namespace + OutpostPool per Devin organization, created automatically: a Helm
chart (`charts/devin-outposts-platform`) and the org provisioner it deploys
(`provisioner/`). A user who belongs to a Devin organization gets that org's
pool with no cluster account or manual step; the cluster team touches the
cluster once, at install. OpenShift is the primary target
(`values-openshift.yaml`); nothing in `values.yaml` or the templates assumes a
cloud, registry or storage driver.

```
charts/devin-outposts-platform   operator + org provisioner + per-org manifests
charts/devin-golden-home         optional: golden home PVC + VolumeSnapshot for persistent homes
provisioner/                     org provisioner (Rust, ~750 lines) and its Dockerfile
Makefile                         lint / package / provisioner-check / provisioner-image / operator-image
```

```
<system namespace>                  (helm release: devin-outposts-platform)
├─ outposts operator                watches every namespace, runs worker pods
└─ org-provisioner                  polls Devin orgs, creates everything below

<namespacePrefix><org-id>           (created by the provisioner, not Helm)
├─ Secret devin-pool-token          the Devin token, for the operator
├─ RoleBinding outposts-operator    lets the operator work in the namespace
├─ ... everything under `manifests` in values.yaml:
│    OutpostPool "org", ResourceQuota, LimitRange, NetworkPolicy, PSA labels
└─ worker pods                      created by the operator per session
```

## How the provisioner works

Every `pollIntervalSeconds` it lists the Devin organizations and Outposts
with the service-user token and, for each organization not in
`excludeOrgIds`:

1. finds the Outpost named `<outpostNamePrefix><org slug>` restricted to that
   org, or creates it;
2. server-side-applies the namespace `<namespacePrefix><12 hex of the org id>`
   (labels `app.kubernetes.io/managed-by=org-provisioner`,
   `outposts.cognition.com/org-id`) and the token Secret into it;
3. applies every document in the mounted manifests ConfigMap (the chart's
   `manifests` values) with `{{ namespace }}`, `{{ org_id }}`,
   `{{ org_name }}`, `{{ org_slug }}`, `{{ outpost_id }}`, `{{ outpost_name }}`
   substituted;
4. makes the Outpost the org's default platform if the org has none;
5. deletes the namespace of any org that no longer exists in Devin
   (`deleteOrphanNamespaces`, on by default; an empty org list is treated as
   an API failure and deletes nothing).

That is all it does. Outposts and the objects in `manifests` are never
deleted; drop a key and the objects already created stay. The manifests file is
re-read every pass, so a `helm upgrade` (or `kubectl edit` of the ConfigMap)
changes every org on the next pass. The provisioner does not know what kinds
are in the list; adding an object to every org namespace is a values change
plus the matching RBAC rule.

## Prerequisites

- Kubernetes >= 1.30 / OpenShift >= 4.17 (ValidatingAdmissionPolicy; set
  `provisioner.admissionPolicy.enabled: false` on older clusters and accept
  that RBAC alone scopes the provisioner).
- A Devin **enterprise service-user token** that can list organizations and
  manage Outposts, in a Secret in the system namespace.
- Images reachable from the cluster: the operator
  (`ghcr.io/cognitionai/devin-outposts-k8s:sha-<OPERATOR_REF>`), the
  provisioner (`make provisioner-image`, push to your registry) and the
  worker image in `manifests.pool.spec.worker.overrides.image`.
- Worker nodes with ~3 CPU / 8 GiB free per concurrent session (defaults in
  `manifests.pool`).

## Install

```sh
NS=devin-system
kubectl create namespace $NS
kubectl -n $NS create secret generic devin-outposts-token --from-literal=token="$DEVIN_TOKEN"

make operator-chart            # fetches the operator subchart at OPERATOR_REF
# Copy values-openshift.yaml, fill in registry paths, the Devin API URL (twice:
# provisioner.devinApiUrl and manifests.pool.spec.apiUrl), acceptorId and image
# tags, and keep it in git.
helm install outposts charts/devin-outposts-platform -n $NS -f my-values.yaml
# On clusters that enforce the restricted PSS level (not OpenShift) add:
#   --post-renderer charts/devin-outposts-platform/post-render.sh

kubectl -n $NS logs deploy/org-provisioner -f
kubectl get ns -l app.kubernetes.io/managed-by=org-provisioner
kubectl get outpostpools -A
```

Within one poll interval every organization has a namespace, an OutpostPool
and an Outpost set as its default platform; the first session an org member
starts lands on it.

## Values worth knowing

| key | default | |
|---|---|---|
| `provisioner.devinApiUrl` | `https://api.devin.ai` | enterprise hosts: `https://<host>/api` |
| `provisioner.token.existingSecret` | — | required; Secret in the release namespace, key `token` |
| `provisioner.namespacePrefix` | `devin-org-` | org namespace = prefix + 12 hex of the org id |
| `provisioner.outpostNamePrefix` | `outpost-` | Outpost name = prefix + org slug; give every cluster of one Devin account its own prefix (and `operator.operator.acceptorId`) |
| `provisioner.pollIntervalSeconds` | `60` | |
| `provisioner.excludeOrgIds` | `[]` | orgs never provisioned (e.g. the enterprise-level org) |
| `provisioner.deleteOrphanNamespaces` | `true` | delete the namespace of an org that no longer exists (adds the `delete` verb on namespaces) |
| `provisioner.rbac.rules` / `.bindClusterRoles` | rules for the default manifests | one rule per kind in `manifests`; ClusterRoles any RoleBinding in `manifests` grants |
| `manifests.<key>` | namespace, pool, quota, limits, network-policy | one Kubernetes object each, applied into every org namespace; add a key to add an object, set one to `null` to drop it |
| `manifests.pool.spec` | `StartFresh`, 50 sessions, worker pod spec | `OutpostPool.spec` as the operator defines it; `poolId` is `{{ outpost_id }}` |
| `operator.*` | | passed to the `devin-outposts-k8s` subchart; `operator.defaultPool.enabled` must stay `false` |

Changing the worker image is editing `manifests.pool.spec.worker.overrides.image`
and `helm upgrade`; every pool is updated on the next pass and new sessions
use it (running ones are not replaced).

### Persistent home directories

The default `resume.policy: StartFresh` needs no storage and matches the
operator's own default. The operator's `FilesystemSnapshot` policy gives each
session a volume cloned from a `resume.volumeDataSource` in the pool's
namespace; `charts/devin-golden-home` builds such a seed volume + snapshot
from the worker image. A `VolumeSnapshot` is namespaced, so each org namespace
needs its own, bound to the same storage-side snapshot through a
pre-provisioned `VolumeSnapshotContent`. Two `manifests` entries do that
(keys sort before `pool`, so they are applied first):

```yaml
provisioner:
  rbac:
    rules:
      - apiGroups: [snapshot.storage.k8s.io]
        resources: [volumesnapshotcontents, volumesnapshots]
        verbs: [get, create, patch]
manifests:
  golden-home-content:
    apiVersion: snapshot.storage.k8s.io/v1
    kind: VolumeSnapshotContent
    metadata:
      name: "{{ namespace }}-golden-home"
    spec:
      deletionPolicy: Retain
      driver: <csi driver>                       # e.g. ebs.csi.aws.com
      volumeSnapshotClassName: <class>
      source:
        snapshotHandle: <handle>                 # status.snapshotHandle of the golden snapshot's content
      volumeSnapshotRef:
        name: golden-home
        namespace: "{{ namespace }}"
  golden-home:
    apiVersion: snapshot.storage.k8s.io/v1
    kind: VolumeSnapshot
    metadata:
      name: golden-home
    spec:
      volumeSnapshotClassName: <class>
      source:
        volumeSnapshotContentName: "{{ namespace }}-golden-home"
  pool:
    spec:
      resume:
        policy: FilesystemSnapshot
        volumeSize: 20Gi
        storageClassName: <storage class>
        volumeDataSource:
          apiGroup: snapshot.storage.k8s.io
          kind: VolumeSnapshot
          name: golden-home
```

`VolumeSnapshotContent` is cluster-scoped, so its name starts with the org
namespace: the admission policy only lets the provisioner write cluster-scoped
objects named `<namespacePrefix>*`. The handle comes from the golden-home
release:
`kubectl get volumesnapshotcontent -o jsonpath='{.items[?(@.spec.volumeSnapshotRef.name=="worker-home-<tag>")].status.snapshotHandle}'`.

## OpenShift notes

- Provisioner and operator run under `restricted-v2` as is
  (`values-openshift.yaml` unsets the fixed uid so the SCC assigns one).
- Worker pods run as uid 1000 (the image's user), which `restricted-v2`
  rejects. `values-openshift.yaml` adds a RoleBinding of
  `system:openshift:scc:nonroot-v2` to every org namespace (`manifests.scc`)
  and lists that ClusterRole in `provisioner.rbac.bindClusterRoles`. Use a
  custom SCC's `system:openshift:scc:<name>` ClusterRole instead if
  `nonroot-v2` is not allowed.
- The default NetworkPolicy's DNS rule targets `kube-system`/`kube-dns`;
  `values-openshift.yaml` replaces it with `openshift-dns` on port 5353 and
  allows the public internet only (private ranges and 169.254/16 excluded);
  add `ipBlock` rules for internal git, package mirrors or proxies.
- Access to a pool is governed by Devin organization membership. Users never
  need an OpenShift account.

## Permissions

What the chart grants (`templates/provisioner-rbac.yaml`,
`templates/operator-rbac.yaml`). Nothing outside the system namespace and the
`<namespacePrefix>*` namespaces is writable by either component.

| who | cluster-wide ClusterRole | scoped how |
|---|---|---|
| provisioner | namespaces get/list/create/patch (+delete with `deleteOrphanNamespaces`); Secrets get/create/patch; RoleBindings get/create/patch + `bind` on the operator's org ClusterRole (and `rbac.bindClusterRoles`); `rbac.rules` (default: ResourceQuota, LimitRange, NetworkPolicy, OutpostPool) | ValidatingAdmissionPolicy: every write must be an object in a namespace named `<namespacePrefix>*` or a cluster-scoped object named `<namespacePrefix>*` |
| operator | read OutpostPools and Pods | writes (worker Pods, Secrets, PVCs, pool status) only through the RoleBinding the provisioner puts in each org namespace; leader-election Lease and ConfigMap in the system namespace |

- The operator subchart's own ClusterRole is disabled (`operator.rbac.create: false`).
- Cluster-scoped objects the provisioner may write are the org namespaces and
  anything else named `<namespacePrefix>*` (e.g. a per-org
  `VolumeSnapshotContent`); the admission policy rejects any other
  cluster-scoped write.
- Worker pods mount no ServiceAccount token, so a session has no Kubernetes
  API access at all.

## Validation

Offline, what CI runs:

```sh
make lint               # helm lint/template + kubeconform, default and OpenShift values
make provisioner-check  # cargo fmt, clippy -D warnings, tests
make provisioner-image  # docker buildx, linux/amd64
```

The provisioner validates the manifests file at startup (YAML, `apiVersion` +
`kind`, known placeholders) and exits on error, so a bad values change shows
up as a crash-looping pod right after `helm upgrade`, before any org is
touched. Logs are JSON; `RUN_ONCE=1` runs a single pass and exits.

## Upstream

The operator is `CognitionAI/devin-outpost-k8s` at the commit in
`charts/devin-outposts-platform/OPERATOR_REF`, consumed unmodified. Bump
`OPERATOR_REF` and `operator.image.tag` together, and check `manifests.pool.spec`
against the new `OutpostPool` CRD.
