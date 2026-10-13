#!/usr/bin/env bash
# Copies charts/devin-outposts-k8s of CognitionAI/devin-outpost-k8s at the
# commit in OPERATOR_REF into the platform chart as its `operator` subchart.
# The copy is gitignored; run this before helm lint/template/install.
set -euo pipefail
cd "$(dirname "$0")/.."
ref=$(cat charts/devin-outposts-platform/OPERATOR_REF)
repo=${OPERATOR_REPO:-https://github.com/CognitionAI/devin-outpost-k8s.git}
src=.cache/devin-outpost-k8s
rm -rf "$src" charts/devin-outposts-platform/charts/devin-outposts-k8s
git init --quiet "$src"
git -C "$src" fetch --quiet --depth 1 "$repo" "$ref"
git -C "$src" checkout --quiet FETCH_HEAD
mkdir -p charts/devin-outposts-platform/charts
cp -r "$src/charts/devin-outposts-k8s" charts/devin-outposts-platform/charts/
