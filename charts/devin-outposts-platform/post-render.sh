#!/usr/bin/env bash
# Helm post-renderer: adds a restricted-PSS-compliant security context to the
# operator Deployment, which the devin-outposts-k8s chart does not expose.
# Usage: helm upgrade --install ... --post-renderer ./post-render.sh
set -euo pipefail
dir=$(mktemp -d); trap 'rm -rf "$dir"' EXIT
cat > "$dir/all.yaml"
cat > "$dir/kustomization.yaml" <<'K'
resources: [all.yaml]
patches:
  - target:
      kind: Deployment
      labelSelector: "app.kubernetes.io/name in (operator, devin-outposts-k8s)"
    patch: |-
      - op: add
        path: /spec/template/spec/securityContext
        value:
          runAsNonRoot: true
          runAsUser: 65532
          runAsGroup: 65532
          seccompProfile: { type: RuntimeDefault }
      - op: add
        path: /spec/template/spec/containers/0/securityContext
        value:
          allowPrivilegeEscalation: false
          readOnlyRootFilesystem: true
          capabilities: { drop: [ALL] }
K
kubectl kustomize "$dir"
