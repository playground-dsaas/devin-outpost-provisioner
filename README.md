# devin-outpost-provisioner

Runs **Devin Outposts** on a customer Kubernetes or OpenShift cluster with one
namespace and one `OutpostPool` per Devin organization, created automatically.
A Helm chart installs the
[devin-outpost-k8s](https://github.com/CognitionAI/devin-outpost-k8s) operator
and the org provisioner (this crate); users who belong to a Devin organization
get that org's pool with no cluster account or manual step.

> **Note:** Like the operator, this is **not** covered by the same support as
> the rest of the Devin product. Contact your account team before deploying.

## Quick start

```bash
NS=devin-system
kubectl create namespace $NS
kubectl -n $NS create secret generic devin-outposts-token --from-literal=token=<SVC_ACT_TOKEN>

hack/operator-chart.sh  # fetches the operator subchart at charts/devin-outposts-platform/OPERATOR_REF
helm install outposts charts/devin-outposts-platform -n $NS \
  -f examples/values-openshift.yaml       # or your own values; see values.yaml
```

Within one poll interval every organization has a namespace, an `OutpostPool`
and an Outpost set as its default platform. Everything an org namespace gets
beyond the token Secret is a plain Kubernetes document under `manifests` in
`values.yaml`; `examples/` shows OpenShift (SCC, DNS) and persistent home
directories (`charts/devin-golden-home` + per-org snapshot binding).

The token is a Devin **enterprise service-user token** that can list
organizations and manage Outposts. The provisioner image is built from the
`Dockerfile` (`docker build -t <registry>/org-provisioner .`); the operator image is
`ghcr.io/cognitionai/devin-outposts-k8s:sha-<OPERATOR_REF>`.

## Layout

```
charts/devin-outposts-platform   operator + org provisioner + per-org manifests
charts/devin-golden-home         optional: golden home PVC + VolumeSnapshot
examples/                        worked values files
src/, tests/                     the org provisioner (Rust) and its wire tests
```

## Code documentation

All documentation lives in the Rustdoc and in the comments of
`charts/devin-outposts-platform/values.yaml`. Run
`cargo doc --document-private-items --open`. `cargo test` and the helm
lint/template steps in `.github/workflows/ci.yaml` are the checks.

## License

[MIT](LICENSE)
