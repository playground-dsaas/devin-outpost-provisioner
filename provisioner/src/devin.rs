//! The three Devin API calls the provisioner needs.

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct Organization {
    #[serde(rename = "organization_id")]
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Outpost {
    #[serde(rename = "outpost_id")]
    pub id: String,
    pub name: String,
    /// Empty when the Outpost is visible to every org (not what we create).
    #[serde(default, rename = "allowed_org_ids")]
    pub allowed_org_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Page<T> {
    #[serde(alias = "organizations", alias = "outposts", alias = "items")]
    items: Vec<T>,
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Platform {
    #[serde(default)]
    outpost_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DefaultPlatform {
    #[serde(default)]
    default_platform: Option<Platform>,
}

#[derive(Serialize)]
struct CreateOutpost<'a> {
    name: &'a str,
    allowed_org_ids: [&'a str; 1],
}

pub struct DevinClient {
    http: reqwest::Client,
    base: String,
}

impl DevinClient {
    pub fn new(api_url: &str, token: &str) -> anyhow::Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        let mut auth = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
            .context("token is not a valid header value")?;
        auth.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, auth);
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            base: format!("{api_url}/v3/enterprise"),
        })
    }

    async fn send<T: serde::de::DeserializeOwned>(
        &self,
        req: reqwest::RequestBuilder,
    ) -> anyhow::Result<T> {
        let resp = req.send().await?;
        let status = resp.status();
        let url = resp.url().to_string();
        let body = resp.text().await?;
        anyhow::ensure!(status.is_success(), "{status} from {url}: {body}");
        serde_json::from_str(&body).with_context(|| format!("decoding response from {url}"))
    }

    async fn list_all<T: serde::de::DeserializeOwned>(&self, path: &str) -> anyhow::Result<Vec<T>> {
        let mut items = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut req = self
                .http
                .get(format!("{}{path}", self.base))
                .query(&[("limit", "100")]);
            if let Some(c) = &cursor {
                req = req.query(&[("cursor", c)]);
            }
            let page: Page<T> = self.send(req).await?;
            items.extend(page.items);
            match page.next_cursor {
                Some(c) if !c.is_empty() => cursor = Some(c),
                _ => return Ok(items),
            }
        }
    }

    pub async fn organizations(&self) -> anyhow::Result<Vec<Organization>> {
        self.list_all("/organizations").await
    }

    pub async fn outposts(&self) -> anyhow::Result<Vec<Outpost>> {
        self.list_all("/outposts").await
    }

    pub async fn create_outpost(&self, name: &str, org_id: &str) -> anyhow::Result<Outpost> {
        let body = CreateOutpost {
            name,
            allowed_org_ids: [org_id],
        };
        self.send(
            self.http
                .post(format!("{}/outposts", self.base))
                .json(&body),
        )
        .await
    }

    /// The org's default platform Outpost id, if one is set.
    pub async fn default_outpost(&self, org_id: &str) -> anyhow::Result<Option<String>> {
        let got: DefaultPlatform = self
            .send(self.http.get(format!(
                "{}/organizations/{org_id}/default-platform",
                self.base
            )))
            .await?;
        Ok(got
            .default_platform
            .and_then(|p| p.outpost_id)
            .filter(|id| !id.is_empty()))
    }

    pub async fn set_default_outpost(&self, org_id: &str, outpost_id: &str) -> anyhow::Result<()> {
        let body = serde_json::json!({ "outpost_id": outpost_id });
        let _: serde_json::Value = self
            .send(
                self.http
                    .put(format!(
                        "{}/organizations/{org_id}/default-platform",
                        self.base
                    ))
                    .json(&body),
            )
            .await?;
        Ok(())
    }
}
