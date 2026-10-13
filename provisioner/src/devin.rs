//! The Devin API calls the provisioner needs: enterprise organizations
//! (`/v3/enterprise/organizations`), Outposts (`/opbeta/outposts`) and the
//! per-org default platform (`/v3beta1/organizations/{org_id}/default-platform`).
//! List endpoints share one cursor envelope (`items`, `end_cursor`,
//! `has_next_page`) and take `first` + `after`.

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct Organization {
    #[serde(rename = "org_id")]
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct Outpost {
    pub id: String,
    pub name: String,
    /// Empty when the Outpost is visible to every org (not what we create).
    pub allowed_org_ids: Vec<String>,
}

#[derive(Deserialize)]
struct OutpostWire {
    metadata: OutpostMetadata,
    spec: OutpostSpec,
}

#[derive(Deserialize)]
struct OutpostMetadata {
    outpost_id: String,
}

#[derive(Deserialize)]
struct OutpostSpec {
    name: String,
    #[serde(default)]
    allowed_org_ids: Option<Vec<String>>,
}

impl From<OutpostWire> for Outpost {
    fn from(w: OutpostWire) -> Self {
        Outpost {
            id: w.metadata.outpost_id,
            name: w.spec.name,
            allowed_org_ids: w.spec.allowed_org_ids.unwrap_or_default(),
        }
    }
}

#[derive(Deserialize)]
struct Page<T> {
    items: Vec<T>,
    #[serde(default)]
    end_cursor: Option<String>,
    #[serde(default)]
    has_next_page: bool,
}

/// `platform` is a hosted platform label; at most one of the two is set.
#[derive(Default, Serialize, Deserialize)]
struct DefaultPlatform {
    #[serde(default)]
    platform: Option<String>,
    #[serde(default)]
    outpost_pool_id: Option<String>,
}

#[derive(Serialize)]
struct CreateOutpost<'a> {
    name: &'a str,
    description: String,
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
            base: api_url.to_string(),
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
        let mut after: Option<String> = None;
        loop {
            let mut req = self
                .http
                .get(format!("{}{path}", self.base))
                .query(&[("first", "100")]);
            if let Some(c) = &after {
                req = req.query(&[("after", c)]);
            }
            let page: Page<T> = self.send(req).await?;
            items.extend(page.items);
            match (page.has_next_page, page.end_cursor) {
                (true, Some(c)) if !c.is_empty() => after = Some(c),
                _ => return Ok(items),
            }
        }
    }

    pub async fn organizations(&self) -> anyhow::Result<Vec<Organization>> {
        self.list_all("/v3/enterprise/organizations").await
    }

    pub async fn outposts(&self) -> anyhow::Result<Vec<Outpost>> {
        let wire: Vec<OutpostWire> = self.list_all("/opbeta/outposts").await?;
        Ok(wire.into_iter().map(Outpost::from).collect())
    }

    pub async fn create_outpost(&self, name: &str, org: &Organization) -> anyhow::Result<Outpost> {
        let body = CreateOutpost {
            name,
            description: format!("org-provisioner: {} ({})", org.name, org.id),
            allowed_org_ids: [&org.id],
        };
        let wire: OutpostWire = self
            .send(
                self.http
                    .post(format!("{}/opbeta/outposts", self.base))
                    .json(&body),
            )
            .await?;
        Ok(wire.into())
    }

    fn default_platform_url(&self, org_id: &str) -> String {
        format!(
            "{}/v3beta1/organizations/{org_id}/default-platform",
            self.base
        )
    }

    /// Whether the org already has a default placement (hosted or Outpost).
    pub async fn has_default_platform(&self, org_id: &str) -> anyhow::Result<bool> {
        let got: DefaultPlatform = self
            .send(self.http.get(self.default_platform_url(org_id)))
            .await?;
        Ok(got.platform.is_some() || got.outpost_pool_id.is_some())
    }

    pub async fn set_default_outpost(&self, org_id: &str, outpost_id: &str) -> anyhow::Result<()> {
        let body = DefaultPlatform {
            platform: None,
            outpost_pool_id: Some(outpost_id.to_string()),
        };
        let _: serde_json::Value = self
            .send(self.http.put(self.default_platform_url(org_id)).json(&body))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_api_shapes() {
        let orgs: Page<Organization> = serde_json::from_str(
            r#"{"items":[{"org_id":"org-1","name":"A","created_at":1}],"end_cursor":null,"has_next_page":false,"total":1}"#,
        )
        .unwrap();
        assert_eq!(orgs.items[0].id, "org-1");
        let outposts: Page<OutpostWire> = serde_json::from_str(
            r#"{"items":[{"metadata":{"outpost_id":"outpost_env-1","account_id":"a"},"spec":{"name":"sno-a","platform":null,"allowed_org_ids":["org-1"]},"status":{"queue_depth":0}}],"end_cursor":null,"has_next_page":false}"#,
        )
        .unwrap();
        let o: Outpost = outposts.items.into_iter().next().unwrap().into();
        assert_eq!((o.id.as_str(), o.name.as_str()), ("outpost_env-1", "sno-a"));
        assert_eq!(o.allowed_org_ids, ["org-1"]);
        let unset: DefaultPlatform = serde_json::from_str(
            r#"{"platform":null,"outpost_pool_id":null,"outpost_pool_name":null}"#,
        )
        .unwrap();
        assert!(unset.platform.is_none() && unset.outpost_pool_id.is_none());
    }
}
