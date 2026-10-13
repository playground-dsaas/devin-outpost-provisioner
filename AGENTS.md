# devin-outpost-provisioner

- This repository is the hand-off artifact for running Devin Outposts on a customer cluster (OpenShift first): a Helm chart, an optional golden-home chart and the org provisioner. Nothing in `charts/*/values.yaml`, the templates or `src/` may assume a cloud, a registry, a node label or a storage driver; cluster-specific choices are values (`examples/values-openshift.yaml` and `examples/values-persistent-home.yaml` are the worked examples).
- `devin-outpost-k8s` (https://github.com/CognitionAI/devin-outpost-k8s) is consumed unmodified at the commit in `charts/devin-outposts-platform/OPERATOR_REF`. Its chart is not committed: `hack/operator-chart.sh` copies it into `charts/devin-outposts-platform/charts/` (gitignored) as the `operator` subchart; helm lint/template/package need it. Bump `OPERATOR_REF` and `operator.image.tag` in the platform `values.yaml` together and re-check `manifests.pool.spec` against the CRD. Never enable `operator.defaultPool`: pools are created per org by the provisioner.
- Keep the provisioner small. It knows about organizations, Outposts, the org namespace and the token Secret; everything else an org namespace gets is an opaque document under `manifests` in values, applied with server-side apply. New per-org behaviour goes into values (and `provisioner.rbac.rules`), not into Rust. Compatibility with the reference operator comes first: never require `resume.volumeDataSource` or any other optional CRD field.
- `.github/workflows/ci.yaml` is the whole check list (cargo fmt/clippy/test, helm lint/template + kubeconform, image build); everything in it runs offline against this checkout.
- The crate follows `devin-outpost-k8s` conventions: Rust 1.96 / edition 2024, `kube` server-side apply (field managers `org-provisioner` and `org-provisioner-manifests`), `tests/wire.rs` against the in-memory mock of the Devin API in `tests/mock_server`. Verify Devin API shapes against a live host before changing `src/devin.rs`.
- Worker security contract (in `manifests.pool`): UID/GID 1000, all capabilities dropped, no privilege escalation, `RuntimeDefault` seccomp, `shareProcessNamespace: true`, worker command `/usr/local/bin/outpost-entrypoint.sh`. On OpenShift the uid is allowed by the SCC RoleBinding in `examples/values-openshift.yaml`. The worker image is Cognition's `devin-outpost-prod` unmodified.
- Secrets never enter git: the Devin token is a Kubernetes Secret (`provisioner.token.existingSecret`).

## Documentation and Commenting Style

All documentation for this codebase exists in the actual code as a rustdoc, with only a minimal `README.md` that shouldn't need to evolve very much (if at all) over time. Documentation, just like code, should have good abstractions. Some general rules:

- A piece of information should exist as a comment in exactly one place, and referenced from other places.
- Variable names should be self-descriptive as much as possible, to avoid needing a comment to begin with.
- If a human said something to you in the way as for reasoning for a change, it's probably a good idea to write down in one place, minimally, as close to the way they said it as possible.

Comments should explain specific constraints that were part of the spec for the software.

Example (cartoonishly BAD, do not do this):
```rust
// user id stores a unique random id (UUIDv7) for each user
let user_id = Uuidv7::generate();
```

Example (GOOD, do this):
```rust
let user_id = Uuidv7::generate();
```

Example (GOOD, do this):
```rust
// using v7 for db performance reasons: <reference to some issue or thread maybe>
let user_id = Uuidv7::generate();
```

**If unsure**: it is better to leave no comment or doc than one that is useless, as it makes the code harder for humans to understand and review.

## Golden Rule

Existing code or documentation in this codebase may not follow the above rules. That is not an excuse to add new things that also don't follow the rules. Leave the codebase a better place than you found it!
