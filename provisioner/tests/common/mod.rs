//! In-memory Devin account and fixtures shared by the integration tests.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use org_provisioner::cluster::MemCluster;
use org_provisioner::devin::{
    CreateOutpost, DefaultPlatform, DevinApi, Organization, Outpost, OutpostMetadata, OutpostSpec,
};
use org_provisioner::reconcile::Settings;
use org_provisioner::template::PoolTemplate;
use org_provisioner::{Error, Result};

pub const ORG_A: &str = "org-aaaaaaaa11111111aaaaaaaa11111111";
pub const ORG_B: &str = "org-bbbbbbbb22222222bbbbbbbb22222222";
pub const NS_A: &str = "devin-org-aaaaaaaa1111";
pub const NS_B: &str = "devin-org-bbbbbbbb2222";
pub const GRACE: Duration = Duration::from_secs(3600);

/// In-memory Devin account.
#[derive(Default)]
pub struct MemDevin {
    pub orgs: Mutex<Vec<Organization>>,
    pub outposts: Mutex<Vec<Outpost>>,
    /// Org IDs the account rejects in `allowed_org_ids`.
    pub foreign_orgs: Mutex<BTreeSet<String>>,
    pub fail_org_list: Mutex<bool>,
    pub created: Mutex<Vec<CreateOutpost>>,
    pub deleted: Mutex<Vec<String>>,
    pub counter: Mutex<u32>,
    pub default_platforms: Mutex<BTreeMap<String, DefaultPlatform>>,
    /// Make `set_default_platform` fail like a token without ManageOrgSettings.
    pub forbid_default_platform: Mutex<bool>,
}

impl MemDevin {
    pub fn add_org(&self, id: &str, name: &str) {
        self.orgs.lock().unwrap().push(Organization {
            org_id: id.into(),
            name: name.into(),
            created_at: None,
            updated_at: None,
        });
    }
    pub fn remove_org(&self, id: &str) {
        self.orgs.lock().unwrap().retain(|o| o.org_id != id);
    }
    pub fn outposts(&self) -> Vec<Outpost> {
        self.outposts.lock().unwrap().clone()
    }
    pub fn created(&self) -> Vec<CreateOutpost> {
        self.created.lock().unwrap().clone()
    }
    pub fn deleted(&self) -> Vec<String> {
        self.deleted.lock().unwrap().clone()
    }
    pub fn default_platform(&self, org_id: &str) -> DefaultPlatform {
        self.default_platforms
            .lock()
            .unwrap()
            .get(org_id)
            .cloned()
            .unwrap_or_default()
    }
    pub fn set_default_platform_to(&self, org_id: &str, value: DefaultPlatform) {
        self.default_platforms
            .lock()
            .unwrap()
            .insert(org_id.to_string(), value);
    }
}

#[async_trait]
impl DevinApi for MemDevin {
    async fn list_organizations(&self) -> Result<Vec<Organization>> {
        if *self.fail_org_list.lock().unwrap() {
            return Err(Error::Api {
                status: 503,
                body: "unavailable".into(),
            });
        }
        Ok(self.orgs.lock().unwrap().clone())
    }
    async fn list_outposts(&self) -> Result<Vec<Outpost>> {
        Ok(self.outposts())
    }
    async fn create_outpost(&self, req: &CreateOutpost) -> Result<Outpost> {
        self.created.lock().unwrap().push(req.clone());
        if let Some(ids) = &req.allowed_org_ids {
            let foreign = self.foreign_orgs.lock().unwrap();
            let bad: Vec<&String> = ids.iter().filter(|id| foreign.contains(*id)).collect();
            if !bad.is_empty() {
                return Err(Error::InvalidOrgIds(format!(
                    "Invalid organization IDs: {bad:?}"
                )));
            }
        }
        let mut n = self.counter.lock().unwrap();
        *n += 1;
        let outpost = Outpost {
            metadata: OutpostMetadata {
                outpost_id: format!("outpost_{n}"),
                account_id: None,
                created_at: None,
            },
            spec: OutpostSpec {
                name: req.name.clone(),
                platform: None,
                description: req.description.clone(),
                allowed_org_ids: req.allowed_org_ids.clone(),
            },
        };
        self.outposts.lock().unwrap().push(outpost.clone());
        Ok(outpost)
    }
    async fn delete_outpost(&self, outpost_id: &str) -> Result<()> {
        self.deleted.lock().unwrap().push(outpost_id.into());
        self.outposts
            .lock()
            .unwrap()
            .retain(|o| o.metadata.outpost_id != outpost_id);
        Ok(())
    }
    async fn get_default_platform(&self, org_id: &str) -> Result<DefaultPlatform> {
        Ok(self.default_platform(org_id))
    }
    async fn set_default_platform(&self, org_id: &str, outpost_id: &str) -> Result<()> {
        if *self.forbid_default_platform.lock().unwrap() {
            return Err(Error::Api {
                status: 403,
                body: "ManageOrgSettings required".into(),
            });
        }
        let name = self
            .outposts()
            .into_iter()
            .find(|o| o.metadata.outpost_id == outpost_id)
            .map(|o| o.spec.name);
        self.set_default_platform_to(
            org_id,
            DefaultPlatform {
                platform: None,
                outpost_pool_id: Some(outpost_id.to_string()),
                outpost_pool_name: name,
            },
        );
        Ok(())
    }
}

pub fn settings() -> Settings {
    Settings {
        api_url: "https://api.devin.ai".into(),
        namespace_prefix: "devin-org-".into(),
        outpost_name_prefix: "eks-".into(),
        exclude_org_ids: BTreeSet::new(),
        deprovision_grace: GRACE,
        system_namespace: "devin-system".into(),
    }
}

pub const GOLDEN: &str = "worker-home";

/// The shipped template with the OpenShift overlay (SCC role binding included).
pub fn template() -> PoolTemplate {
    PoolTemplate::parse_helm_values(&[
        include_str!("../../../charts/devin-outposts-platform/values.yaml"),
        include_str!("../../../charts/devin-outposts-platform/values-openshift.yaml"),
    ])
    .unwrap()
}

pub fn worker_image() -> String {
    template().pool.worker.overrides.image.unwrap()
}

/// A cluster where `make golden-snapshot` has already run for the template's
/// worker image.
pub fn golden_cluster() -> MemCluster {
    let cluster = MemCluster::default();
    cluster.add_golden_snapshot("devin-system", GOLDEN, &worker_image(), "snap-golden", true);
    cluster
}

pub fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
}

/// The shipped template alone: no role bindings.
pub fn template_default() -> PoolTemplate {
    PoolTemplate::parse_helm_values(&[include_str!(
        "../../../charts/devin-outposts-platform/values.yaml"
    )])
    .unwrap()
}
