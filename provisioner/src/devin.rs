//! Devin API access: enterprise organizations (`/v3/enterprise/organizations`),
//! Outposts (`/opbeta/outposts`) and the per-org default platform
//! (`/v3beta1/organizations/{org_id}/default-platform`).
//!
//! Both list endpoints use the same cursor envelope (`items`, `end_cursor`,
//! `has_next_page`) and take `first` (page size) + `after` (cursor).

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const PAGE_SIZE: u32 = 100;

/// One enterprise organization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Organization {
    /// Stable identifier (`org-<hex>`).
    pub org_id: String,
    /// Display name; may change and may collide across orgs.
    pub name: String,
    #[serde(default)]
    pub created_at: Option<i64>,
    #[serde(default)]
    pub updated_at: Option<i64>,
}

/// Cursor-paginated list envelope shared by both endpoints.
#[derive(Debug, Clone, Deserialize)]
pub struct Page<T> {
    #[serde(default = "Vec::new")]
    pub items: Vec<T>,
    #[serde(default)]
    pub end_cursor: Option<String>,
    #[serde(default)]
    pub has_next_page: bool,
}

/// Identifying metadata of an Outpost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutpostMetadata {
    pub outpost_id: String,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub created_at: Option<i64>,
}

/// Desired state of an Outpost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutpostSpec {
    pub name: String,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Organizations whose sessions may be routed to this Outpost; `None`
    /// means any org in the account.
    #[serde(default)]
    pub allowed_org_ids: Option<Vec<String>>,
}

/// An Outpost (account-scoped session queue).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outpost {
    pub metadata: OutpostMetadata,
    pub spec: OutpostSpec,
}

/// Body of `POST /opbeta/outposts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateOutpost {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_org_ids: Option<Vec<String>>,
}

/// An organization's default session placement: a hosted `platform` label
/// or an Outpost, all `None` when unset. `PUT` takes at most one of the two
/// and needs `ManageOrgSettings` on the org.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefaultPlatform {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outpost_pool_id: Option<String>,
    #[serde(default, skip_serializing)]
    pub outpost_pool_name: Option<String>,
}

impl DefaultPlatform {
    pub fn is_unset(&self) -> bool {
        self.platform.is_none() && self.outpost_pool_id.is_none()
    }

    /// Human-readable target, for logs and the verify report.
    pub fn describe(&self) -> String {
        match (
            &self.platform,
            &self.outpost_pool_name,
            &self.outpost_pool_id,
        ) {
            (Some(p), _, _) => format!("hosted platform {p}"),
            (None, Some(n), Some(id)) => format!("Outpost {n} ({id})"),
            (None, None, Some(id)) => format!("Outpost {id}"),
            _ => "unset".to_string(),
        }
    }
}

/// The subset of the Devin API the reconciler needs. Implemented by
/// [`DevinClient`] and by in-memory fakes in tests.
#[async_trait]
pub trait DevinApi: Send + Sync {
    /// Every organization in the enterprise.
    async fn list_organizations(&self) -> Result<Vec<Organization>>;
    /// Every Outpost in the account.
    async fn list_outposts(&self) -> Result<Vec<Outpost>>;
    /// Create an Outpost. Returns [`Error::InvalidOrgIds`] when
    /// `allowed_org_ids` is rejected.
    async fn create_outpost(&self, req: &CreateOutpost) -> Result<Outpost>;
    /// Delete an Outpost. A missing Outpost is not an error.
    async fn delete_outpost(&self, outpost_id: &str) -> Result<()>;
    async fn get_default_platform(&self, org_id: &str) -> Result<DefaultPlatform>;
    /// Point the org's default placement at an Outpost.
    async fn set_default_platform(&self, org_id: &str, outpost_id: &str) -> Result<()>;
}

/// HTTP implementation of [`DevinApi`] bound to one token + base URL.
#[derive(Clone)]
pub struct DevinClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

impl std::fmt::Debug for DevinClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevinClient")
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .finish()
    }
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    detail: Option<serde_json::Value>,
}

impl DevinClient {
    /// Build a client. `token` is never logged.
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent(concat!("org-provisioner/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(Duration::from_secs(10))
                .build()?,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    async fn check(resp: reqwest::Response) -> Result<reqwest::Response> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        if status.as_u16() == 422 {
            let detail = serde_json::from_str::<ErrorBody>(&body)
                .ok()
                .and_then(|b| b.detail)
                .map(|d| match d {
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                })
                .unwrap_or_else(|| body.clone());
            if detail.contains("organization") {
                return Err(Error::InvalidOrgIds(detail));
            }
        }
        let mut body = body;
        body.truncate(512);
        Err(Error::Api {
            status: status.as_u16(),
            body,
        })
    }

    async fn list_all<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<Vec<T>> {
        let mut items = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let mut req = self
                .http
                .get(self.url(path))
                .bearer_auth(&self.token)
                .timeout(REQUEST_TIMEOUT)
                .query(&[("first", PAGE_SIZE)]);
            if let Some(cursor) = &after {
                req = req.query(&[("after", cursor.as_str())]);
            }
            let page: Page<T> = Self::check(req.send().await?).await?.json().await?;
            items.extend(page.items);
            match (page.has_next_page, page.end_cursor) {
                (true, Some(cursor)) => after = Some(cursor),
                _ => return Ok(items),
            }
        }
    }
}

#[async_trait]
impl DevinApi for DevinClient {
    async fn list_organizations(&self) -> Result<Vec<Organization>> {
        self.list_all("/v3/enterprise/organizations").await
    }

    async fn list_outposts(&self) -> Result<Vec<Outpost>> {
        self.list_all("/opbeta/outposts").await
    }

    async fn create_outpost(&self, req: &CreateOutpost) -> Result<Outpost> {
        let resp = self
            .http
            .post(self.url("/opbeta/outposts"))
            .bearer_auth(&self.token)
            .timeout(REQUEST_TIMEOUT)
            .json(req)
            .send()
            .await?;
        Ok(Self::check(resp).await?.json().await?)
    }

    async fn delete_outpost(&self, outpost_id: &str) -> Result<()> {
        let resp = self
            .http
            .delete(self.url(&format!("/opbeta/outposts/{outpost_id}")))
            .bearer_auth(&self.token)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await?;
        if resp.status().as_u16() == 404 {
            return Ok(());
        }
        Self::check(resp).await.map(|_| ())
    }

    async fn get_default_platform(&self, org_id: &str) -> Result<DefaultPlatform> {
        let resp = self
            .http
            .get(self.url(&format!("/v3beta1/organizations/{org_id}/default-platform")))
            .bearer_auth(&self.token)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await?;
        Ok(Self::check(resp).await?.json().await?)
    }

    async fn set_default_platform(&self, org_id: &str, outpost_id: &str) -> Result<()> {
        let body = DefaultPlatform {
            outpost_pool_id: Some(outpost_id.to_string()),
            ..Default::default()
        };
        let resp = self
            .http
            .put(self.url(&format!("/v3beta1/organizations/{org_id}/default-platform")))
            .bearer_auth(&self.token)
            .timeout(REQUEST_TIMEOUT)
            .json(&body)
            .send()
            .await?;
        Self::check(resp).await.map(|_| ())
    }
}
