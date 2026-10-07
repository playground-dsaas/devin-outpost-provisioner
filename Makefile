# Everything here runs offline against this checkout: no cluster or cloud
# credentials. Installing is `helm`, see README.md.

SHELL := /bin/bash
.SHELLFLAGS := -euo pipefail -c

CACHE_DIR      := .cache
DIST_DIR       := dist
PLATFORM_CHART := charts/devin-outposts-platform
GOLDEN_CHART   := charts/devin-golden-home
OPERATOR_REPO  ?= https://github.com/CognitionAI/devin-outpost-k8s.git
OPERATOR_REF   := $(shell cat $(PLATFORM_CHART)/OPERATOR_REF)
# Checkout of the operator at OPERATOR_REF: its chart is copied in as the
# platform chart's `operator` subchart and its image is built from it.
OPERATOR_SRC   := $(CACHE_DIR)/devin-outpost-k8s
OPERATOR_CHART := $(PLATFORM_CHART)/charts/devin-outposts-k8s
CRD_SCHEMAS    := https://raw.githubusercontent.com/datreeio/CRDs-catalog/main/{{.Group}}/{{.ResourceKind}}_{{.ResourceAPIVersion}}.json
KUBECONFORM    := kubeconform -strict -summary -schema-location default -schema-location '$(CRD_SCHEMAS)'

PROVISIONER_DIR   := provisioner
PROVISIONER_TAG   ?= $(shell git rev-parse --short=12 HEAD)
PROVISIONER_IMAGE ?= devin/org-provisioner:$(PROVISIONER_TAG)
OPERATOR_IMAGE    ?= devin/devin-outposts-k8s:sha-$(OPERATOR_REF)

.PHONY: help
help: ## List targets
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  %-18s %s\n", $$1, $$2}'

# ---------------------------------------------------------------- operator

$(OPERATOR_SRC):
	rm -rf $(OPERATOR_SRC)
	git init --quiet $(OPERATOR_SRC)
	git -C $(OPERATOR_SRC) fetch --quiet --depth 1 $(OPERATOR_REPO) $(OPERATOR_REF)
	git -C $(OPERATOR_SRC) checkout --quiet FETCH_HEAD

.PHONY: operator-src
operator-src: $(OPERATOR_SRC) ## Fetch devin-outpost-k8s at charts/devin-outposts-platform/OPERATOR_REF

.PHONY: operator-chart
operator-chart: operator-src ## Copy the operator chart at OPERATOR_REF into the platform chart's charts/
	rm -rf $(OPERATOR_CHART)
	mkdir -p $(dir $(OPERATOR_CHART))
	cp -r $(OPERATOR_SRC)/charts/devin-outposts-k8s $(OPERATOR_CHART)

.PHONY: operator-image
operator-image: operator-src ## Build the operator image for OPERATOR_REF (set OPERATOR_IMAGE to tag it for your registry)
	docker buildx build --platform linux/amd64 --provenance=false --load -t $(OPERATOR_IMAGE) $(OPERATOR_SRC)

# ---------------------------------------------------------------- charts

.PHONY: lint
lint: operator-chart ## helm lint/template + kubeconform on both charts (default and OpenShift values)
	helm lint $(PLATFORM_CHART) --set provisioner.image.repository=example/org-provisioner \
	  --set provisioner.image.tag=example --set provisioner.token.existingSecret=devin-outposts-token
	helm template outposts $(PLATFORM_CHART) -n devin-system \
	  --set provisioner.image.repository=example/org-provisioner --set provisioner.image.tag=example \
	  --set provisioner.token.existingSecret=devin-outposts-token --set warmWorkers.enabled=true \
	  --post-renderer $(PLATFORM_CHART)/post-render.sh | $(KUBECONFORM) -skip ServiceMonitor
	helm lint $(PLATFORM_CHART) -f $(PLATFORM_CHART)/values-openshift.yaml
	helm template outposts $(PLATFORM_CHART) -n devin-system -f $(PLATFORM_CHART)/values-openshift.yaml \
	  | $(KUBECONFORM) -skip ServiceMonitor
	helm lint $(GOLDEN_CHART)
	helm template golden $(GOLDEN_CHART) -n devin-system \
	  --set storageClassName=isilon --set volumeSnapshotClassName=isilon | $(KUBECONFORM)

.PHONY: package
package: operator-chart ## Package both charts (operator subchart bundled) into dist/
	mkdir -p $(DIST_DIR)
	helm package $(PLATFORM_CHART) -d $(DIST_DIR)
	helm package $(GOLDEN_CHART) -d $(DIST_DIR)

# ---------------------------------------------------------------- provisioner

.PHONY: provisioner-fmt
provisioner-fmt: ## cargo fmt --check
	cd $(PROVISIONER_DIR) && cargo fmt --all --check

.PHONY: provisioner-clippy
provisioner-clippy: ## cargo clippy, warnings are errors
	cd $(PROVISIONER_DIR) && cargo clippy --all-targets --locked -- -D warnings

.PHONY: provisioner-test
provisioner-test: ## cargo test (unit + mocked Devin/Kubernetes integration tests)
	cd $(PROVISIONER_DIR) && cargo test --locked

.PHONY: provisioner-check
provisioner-check: provisioner-fmt provisioner-clippy provisioner-test ## All provisioner code checks

.PHONY: provisioner-image
provisioner-image: ## Build the org-provisioner image for linux/amd64 (set PROVISIONER_IMAGE to tag it for your registry)
	docker buildx build --platform linux/amd64 --provenance=false --load -t $(PROVISIONER_IMAGE) $(PROVISIONER_DIR)

.PHONY: clean
clean: ## Remove the cached operator checkout, packaged charts and Rust build output
	rm -rf $(CACHE_DIR) $(DIST_DIR) $(OPERATOR_CHART) $(PROVISIONER_DIR)/target
