# devin-outpost-provisioner

Everything needed to run Devin Outposts on a customer cluster with one system
namespace and one namespace + OutpostPool per Devin organization, created
automatically: two Helm charts (`charts/`) and the org provisioner they deploy
(`provisioner/`). A user who belongs to a Devin organization gets that org's
pool with no cluster account or manual step; the cluster team touches the
cluster once, at install. OpenShift is the primary target
(`values-openshift.yaml`); nothing in `values.yaml` or the templates assumes a
cloud, registry or storage driver.

```
charts/devin-outposts-platform   operator + org provisioner + pool template + worker-image rules
charts/devin-golden-home         golden home PVC + VolumeSnapshot, one release per worker image
provisioner/                     org provisioner (Rust) and its Dockerfile
Makefile                         lint / package / provisioner-check / provisioner-image / operator-image
```

```
<system namespace>                       (helm release: devin-outposts-platform)
├─ outposts operator        watches every namespace, runs worker pods
├─ org-provisioner          polls Devin orgs, creates everything below
└─ worker-home-<tag>        golden home PVC + VolumeSnapshot (helm release: devin-golden-home)

<namespacePrefix><org-id>                (created by the provisioner, not Helm)
├─ OutpostPool "org"        resume.volumeDataSource -> worker-home-<tag>
├─ Secret devin-pool-token
├─ ResourceQuota / LimitRange / NetworkPolicy
├─ VolumeSnapshot worker-home-<tag>   (binding to the golden snapshot)
└─ worker pods              created by the operator per session
```

| chart | what it installs | how often |
|---|---|---|
| `devin-outposts-platform` | operator (`devin-outposts-k8s` subchart, fetched) + org provisioner + pool template | once; upgrade to change settings |
| `devin-golden-home` | PVC seeded from the worker image + VolumeSnapshot of it | once per distinct worker image (default + each `workerImages.images` entry) |

## Prerequisites

- Kubernetes 1.28+ / OpenShift 4.14+, with the CSI snapshot controller, a
  `VolumeSnapshotClass`, and a StorageClass whose driver supports snapshots and
  clone-from-snapshot (EBS, PowerScale/Isilon, ...).
- Three images in a registry the cluster can pull from (set the repository/tag
  and `imagePullSecrets` values for each):
  - operator: `ghcr.io/cognitionai/devin-outposts-k8s:sha-<OPERATOR_REF>`, published
    by https://github.com/CognitionAI/devin-outpost-k8s for the commit in
    `charts/devin-outposts-platform/OPERATOR_REF` (the chart's default; mirror it
    if the cluster cannot reach ghcr.io). To build it yourself instead:
    `make operator-image OPERATOR_IMAGE=<registry>/devin/devin-outposts-k8s:<tag>`.
    Any image built from that commit or a later one works; older operators do
    not know `resume.volumeDataSource`/`homeDir` and reject the pools.
  - org-provisioner: `make provisioner-image PROVISIONER_IMAGE=<registry>/devin/org-provisioner:<tag>`
    (`provisioner/Dockerfile`).
  - worker: Cognition's `devin-outpost-prod:<release tag>`, unchanged, mirrored
    if the cluster cannot reach `public.ecr.aws`.
- A Devin service-user token that can list organizations and manage Outposts,
  stored as a Kubernetes Secret in the system namespace (below).
- The operator subchart, which is not committed: `make operator-chart` copies
  `charts/devin-outposts-k8s` from the operator repository at `OPERATOR_REF`
  into `charts/devin-outposts-platform/charts/`. `make package` does this and
  writes both charts to `dist/*.tgz` with the subchart bundled, which is the
  form to hand to a cluster team. Without make: clone that repo, check out the
  commit, and copy the directory.

## Install

```sh
NS=devin-system
kubectl create namespace $NS
kubectl label namespace $NS pod-security.kubernetes.io/enforce=restricted   # optional, not on OpenShift
kubectl -n $NS create secret generic devin-outposts-token --from-literal=token="$DEVIN_TOKEN"

# 1. operator + provisioner. Copy values-openshift.yaml, fill in registry paths,
#    Devin API URL, acceptorId, storage class and image tags, and keep it in git.
helm upgrade --install outposts charts/devin-outposts-platform -n $NS -f my-values.yaml
# On clusters that enforce the restricted PSS level (not OpenShift) add:
#   --post-renderer charts/devin-outposts-platform/post-render.sh

# 2. golden home volume for the worker image named in poolTemplate.pool.worker.overrides.image
helm install golden-home-a0f71d0e6a charts/devin-golden-home -n $NS \
  --set image=registry.example.com/devin/devin-outpost-prod:release-a0f71d0e6a-20261005061956 \
  --set storageClassName=isilon --set volumeSnapshotClassName=<class> \
  --wait --wait-for-jobs --timeout 20m
```

Order does not matter: until the snapshot is `readyToUse`, each provisioner
pass logs that no golden volume is ready and retries on the next pass. Once it
is, every org gets its namespace, Outpost and OutpostPool within one poll
interval. Selecting that Outpost as the org's default platform is still done
once in the org's Devin settings; the pool carries the annotation
`devin.cognition.com/default-platform: pending` until then.

## Changing the worker image

1. `helm install golden-home-<newtag> charts/devin-golden-home --set image=<new image> --wait --wait-for-jobs`
2. `helm upgrade outposts charts/devin-outposts-platform --reuse-values --set poolTemplate.pool.worker.overrides.image=<new image>`
3. The provisioner rebinds the new snapshot into every org namespace and
   updates each pool on its next pass. New sessions clone the new golden home;
   existing session volumes are untouched.
4. `helm uninstall golden-home-<oldtag>` once no pool references it. The
   snapshot itself is a Helm hook resource and is kept; only the seed PVC and
   Job are removed.

## Per-org worker images

Every org gets `poolTemplate.pool.worker.overrides.image` unless a rule in
`workerImages` says otherwise, so only the exceptions are listed and a few
rules cover thousands of orgs:

```yaml
workerImages:
  images:
    data-science: registry.example.com/devin/devin-outpost-ds:2026.10
  rules:                       # in order, first match wins
    - { org: "Data Science", image: data-science }   # display name
    - { org: "ds-*",         image: data-science }   # glob on the slug
    - { org: org-2f9990d15a5d4f139af863bdff50b3ae, image: data-science }
```

`org` is a glob (`*`, `?`) tested against the org's id, display name and
slug (lowercase, dashes). The chart ships this as its own ConfigMap,
`<provisioner>-worker-images`, which the provisioner re-reads every pass and
which does not restart the Deployment when it changes, so moving an org is
`kubectl -n $NS edit configmap org-provisioner-worker-images` and waiting one
poll interval (the next `helm upgrade` re-renders it from values; set
`workerImages.managed: false` to own the ConfigMap outside Helm). Each image
needs its own golden home first (`helm install golden-home-<x> charts/devin-golden-home
--set image=<that image>`); an org pointed at an image without a ready
snapshot keeps its current pool, and only that org is reported as an error.
When an org moves, its existing sessions keep their volumes; new sessions use
the new image and its golden home.

The pool template is validated by the provisioner's tests
(`PoolTemplate::parse_helm_values` reads `values.yaml` + `values-openshift.yaml`), so
`make provisioner-check` catches a mistyped key before a deploy does.

## Values worth knowing

`devin-outposts-platform`

| key | default | |
|---|---|---|
| `provisioner.devinApiUrl` | `https://api.devin.ai` | enterprise hosts use their own URL; also written to every pool's `apiUrl` |
| `provisioner.token.existingSecret` / `.awsSecretId` | — | exactly one; Kubernetes Secret (key `token`), or AWS Secrets Manager on EKS |
| `provisioner.namespacePrefix` | `outpost-namespace-` | org namespace = prefix + org id |
| `provisioner.outpostNamePrefix` | `outpost-` | Outpost name = prefix + org slug |
| `provisioner.pollIntervalSeconds` | `60` | |
| `provisioner.deprovisionGraceSeconds` | `3600` | how long an org must be gone before its namespace/Outpost are deleted |
| `provisioner.excludeOrgIds` | `[]` | orgs never provisioned (e.g. the enterprise-level org) |
| `warmWorkers.enabled` / `.replicas` | `false` / `1` | standby pods running the worker image on worker nodes; image, requests and scheduling come from `poolTemplate` |
| `poolTemplate.pool` | see values.yaml | becomes each org's `OutpostPool.spec`; worker pod spec, resume policy, image |
| `poolTemplate.namespace` | see values.yaml | per-org PSA labels, ResourceQuota, LimitRange, NetworkPolicy |
| `workerImages.images` / `.rules` | `{}` / `[]` | named images and ordered org → image rules (see above); `workerImages.managed: false` leaves the ConfigMap to you |
| `operator.*` | | passed straight to the `devin-outposts-k8s` subchart; `operator.defaultPool.enabled` must stay `false` |

`devin-golden-home`

| key | default | |
|---|---|---|
| `image` | devin-outpost-prod release | must equal `poolTemplate.pool.worker.overrides.image` or a `workerImages.images` entry exactly |
| `storageClassName`, `volumeSnapshotClassName` | `""` (cluster defaults) | |
| `size` | `20Gi` | must be <= the pool's `resume.volumeSize` |

## OpenShift notes

- Provisioner and operator run under `restricted-v2` as is (`values-openshift.yaml`
  unsets the fixed uid so the SCC can assign one).
- Worker pods run as uid 1000 (the image's user), which `restricted-v2`
  rejects. `values-openshift.yaml` sets `poolTemplate.namespace.roleBindings`
  so the provisioner binds `system:openshift:scc:nonroot-v2` to every
  ServiceAccount of each org namespace as it creates it; no `oc adm policy`
  step per org. Use a custom SCC's `system:openshift:scc:<name>` ClusterRole
  instead if `nonroot-v2` is not allowed.
- The default NetworkPolicy's DNS rule targets `kube-system`/`kube-dns`;
  OpenShift's resolver is in `openshift-dns`. `values-openshift.yaml` uses an
  allow-all egress instead; tighten to taste.
- Access to a pool is governed by Devin organization membership. Users never
  need an OpenShift account.

## Validation

Offline, what CI runs on every change (no cluster):

- `make lint`: fetches the operator subchart at `OPERATOR_REF`, then
  `helm lint` + `helm template` + `kubeconform` (with the OutpostPool CRD
  schema) on both charts with the default and OpenShift values.
- `make provisioner-check`: `cargo fmt`, `clippy -D warnings`, and the tests,
  which drive the provisioner and the verifier against an in-memory Devin API
  and cluster and parse the shipped chart values as the pool template.
- `make package`: both charts as `dist/*.tgz`, operator subchart included.

On a cluster, after the install steps above and once the golden home release
has finished:

```sh
helm test outposts -n $NS --logs
```

runs a Pod from the provisioner image (`org-provisioner verify`,
`provisioner/src/verify.rs`) with the Deployment's ServiceAccount, token and
ConfigMaps. It re-reads the cluster every 15 s for up to 4 min
(`provisioner.test.*`; keep that under `helm test --timeout`, 5 m by default)
and then prints one line per check, failing the test if any `FAIL` remains:

| check | passes when |
|---|---|
| `devin-api/organizations`, `devin-api/outposts` | the token lists the enterprise's organizations and the account's Outposts at `provisioner.devinApiUrl` |
| `golden-snapshot <image>` | a `readyToUse` golden VolumeSnapshot for the default worker image, and for every image an org resolves to, exists in the system namespace |
| `<ns>/namespace` | the org's namespace exists and is not marked orphaned |
| `<ns>/pool` | the OutpostPool exists and its `poolId` is an Outpost of the account restricted to that org (`WARN` if unrestricted) |
| `<ns>/pool/image` | the pool runs the image the rules resolve for that org |
| `<ns>/operator` | the operator has synced the pool: `status.phase: Ready` (`Unauthorized`/`Degraded` carry the operator's error message) |
| `<ns>/token-secret` | `devin-pool-token` exists with its `token` key |
| `<ns>/golden-binding` | the org's VolumeSnapshot binding is `readyToUse` and the pool clones session volumes from it |
| `<ns>/rolebinding/<name>` | each `poolTemplate.namespace.roleBindings` entry binds its ClusterRole to `system:serviceaccounts:<ns>` (OpenShift: the SCC that admits uid 1000) |
| `<ns>/default-platform` | `WARN` until the org's default platform has been pointed at its Outpost in the Devin UI (manual; no API) |
| `<ns>/orphaned` | `WARN` for a namespace whose org has left the enterprise and is inside the grace period |

An org with no namespace fails `<ns>/namespace`: either the provisioner has not
run yet, or Devin refuses to restrict an Outpost to that org (the
enterprise-level org), in which case add it to `provisioner.excludeOrgIds`.
Everything a `FAIL` says is also in `kubectl -n $NS logs deploy/org-provisioner`
(provisioner side), `kubectl get opool -A` (operator side, `Phase` column) and
`kubectl -n $NS get volumesnapshot` (storage side). A failed test Pod is kept
until the next run: `kubectl -n $NS logs org-provisioner-verify`.

What a clean report does not prove: that a worker pod is admitted (the SCC
actually allows uid 1000), starts the desktop and reaches Devin. The last
acceptance step is a real session in one of the orgs while watching
`kubectl -n <ns> get pods -w`.

This layout (operator and provisioner in one system namespace, a namespace and
pool per org, golden home volumes per image, per-org image rules, sleep/wake
persistence) has been run end to end from these charts on an EKS cluster. It
has not yet been installed on OpenShift; the SCC binding, CSI snapshot and DNS
points above are the known differences, and `helm test` is what should be run
there first.

## Upstream

The operator is https://github.com/CognitionAI/devin-outpost-k8s, consumed
unmodified at the commit in `charts/devin-outposts-platform/OPERATOR_REF` (its
chart as the `operator` subchart, its crate for the OutpostPool types in
`provisioner/Cargo.toml`). Bump the three together.
