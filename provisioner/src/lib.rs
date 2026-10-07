//! # org-provisioner
//!
//! Keeps one Devin Outpost, one Kubernetes namespace and one `OutpostPool`
//! per enterprise organization, so that a newly created org is serviceable
//! by the `devin-outposts-k8s` operator within one poll interval and a
//! deleted org is torn down after a grace period.
//!
//! ## Crate layout
//!
//! - [`devin`]     — enterprise org + Outposts API client (`DevinApi` trait)
//! - [`cluster`]   — Kubernetes access (`Cluster` trait, real + in-memory)
//! - [`template`]  — the pool template loaded from the ConfigMap
//! - [`images`]    — per-org worker images (profiles + rules), reloaded every pass
//! - [`naming`]    — stable org-derived names
//! - [`render`]    — Kubernetes objects for one org
//! - [`snapshot`]  — `VolumeSnapshot`/`VolumeSnapshotContent` types for the golden volume
//! - [`reconcile`] — the pass itself
//! - [`metrics`], [`token`], [`config`], [`error`]

pub mod cluster;
pub mod config;
pub mod devin;
pub mod error;
pub mod images;
pub mod metrics;
pub mod naming;
pub mod reconcile;
pub mod render;
pub mod snapshot;
pub mod template;
pub mod token;

pub use error::{Error, Result};
