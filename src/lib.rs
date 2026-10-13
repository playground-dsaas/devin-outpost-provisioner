//! # org-provisioner
//!
//! Gives every Devin organization its own Outpost, Kubernetes namespace and
//! the objects you choose, on any cluster that runs the
//! [devin-outpost-k8s](https://github.com/CognitionAI/devin-outpost-k8s)
//! operator. Deployed by `charts/devin-outposts-platform` together with that
//! operator; a user who belongs to a Devin organization gets that org's pool
//! with no cluster account or manual step.
//!
//! ## The loop
//!
//! Every [`config::Config::poll_interval`] the provisioner lists the Devin
//! organizations and Outposts with its service-user token and, for each
//! organization not in [`config::Config::exclude_org_ids`]:
//!
//! 1. finds the Outpost named `<outpost_name_prefix><org slug>` restricted to
//!    that org, or creates it ([`reconcile::ensure_outpost`]);
//! 2. server-side-applies the namespace `<namespace_prefix><12 hex of the org
//!    id>` ([`naming::namespace`]) with labels
//!    [`reconcile::LABEL_MANAGED_BY`] and [`reconcile::LABEL_ORG_ID`], and the
//!    token Secret [`reconcile::TOKEN_SECRET_NAME`] into it;
//! 3. applies the documents of the mounted manifests file ([`manifests`])
//!    with the placeholders in [`manifests::VARIABLES`] substituted, when the
//!    namespace is new or the rendered documents changed (hash kept in the
//!    namespace annotation [`reconcile::ANNOTATION_MANIFESTS_HASH`]);
//! 4. makes the Outpost the org's default platform if the org has none
//!    ([`devin::DevinClient::set_default_outpost`]);
//! 5. deletes the namespace of any org that no longer exists in Devin
//!    ([`reconcile::delete_orphan_namespaces`]).
//!
//! That is all it does. Outposts and the objects in the manifests are never
//! deleted; the provisioner does not know what kinds the manifests contain, so
//! adding an object to every org namespace is a values change plus the
//! matching RBAC rule (`provisioner.rbac.rules` in the chart).
//!
//! ## Permissions
//!
//! Cluster-wide: namespaces get/list/create/patch (+ delete when
//! [`config::Config::delete_orphan_namespaces`]), Secrets get/create/patch,
//! plus one rule per kind in the manifests. A `ValidatingAdmissionPolicy` from
//! the chart confines every write to namespaces named `<namespace_prefix>*`
//! and cluster-scoped objects named `<namespace_prefix>*`.
//!
//! ## Crate layout
//!
//! - [`config`]    — process configuration from the environment
//! - [`devin`]     — the three Devin API calls
//! - [`manifests`] — the mounted manifests file: parse, validate, render
//! - [`naming`]    — namespace and Outpost names derived from an organization
//! - [`reconcile`] — one pass of the loop above

pub mod config;
pub mod devin;
pub mod manifests;
pub mod naming;
pub mod reconcile;
