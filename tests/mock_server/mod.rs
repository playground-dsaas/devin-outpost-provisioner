//! In-memory mock of the three Devin endpoints the provisioner uses, with the
//! wire shapes observed on an enterprise host: cursor-paginated lists
//! (`items`, `end_cursor`, `has_next_page`; `first`/`after` params) of
//! organizations (`org_id`, `name`) and Outposts (`metadata.outpost_id`,
//! `spec.name`, `spec.allowed_org_ids`), and the per-org default platform
//! (`platform`, `outpost_pool_id`).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};

#[derive(Default)]
struct Inner {
    orgs: Vec<(String, String)>,
    outposts: Vec<(String, String, Vec<String>)>,
    default_platform: BTreeMap<String, Value>,
    page_size: usize,
    requests: Vec<String>,
}

#[derive(Clone)]
pub struct MockDevin {
    inner: Arc<Mutex<Inner>>,
    token: String,
    pub base_url: String,
}

impl MockDevin {
    pub async fn start(token: &str) -> Self {
        let mock = Self {
            inner: Arc::new(Mutex::new(Inner {
                page_size: 100,
                ..Default::default()
            })),
            token: token.to_string(),
            base_url: String::new(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/api/v3/enterprise/organizations", get(list_orgs))
            .route(
                "/api/opbeta/outposts",
                get(list_outposts).post(create_outpost),
            )
            .route(
                "/api/v3beta1/organizations/{org_id}/default-platform",
                get(get_default).put(put_default),
            )
            .with_state(mock.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base_url: format!("http://{addr}/api"),
            ..mock
        }
    }

    pub fn add_org(&self, id: &str, name: &str) {
        self.inner
            .lock()
            .unwrap()
            .orgs
            .push((id.into(), name.into()));
    }

    pub fn add_outpost(&self, id: &str, name: &str, allowed: &[&str]) {
        self.inner.lock().unwrap().outposts.push((
            id.into(),
            name.into(),
            allowed.iter().map(|s| s.to_string()).collect(),
        ));
    }

    pub fn set_page_size(&self, n: usize) {
        self.inner.lock().unwrap().page_size = n;
    }

    pub fn set_default_platform(&self, org_id: &str, body: Value) {
        self.inner
            .lock()
            .unwrap()
            .default_platform
            .insert(org_id.into(), body);
    }

    pub fn default_platform(&self, org_id: &str) -> Option<Value> {
        self.inner
            .lock()
            .unwrap()
            .default_platform
            .get(org_id)
            .cloned()
    }

    pub fn outposts(&self) -> Vec<(String, String, Vec<String>)> {
        self.inner.lock().unwrap().outposts.clone()
    }

    /// `METHOD path?query` of every request that carried a valid token.
    pub fn requests(&self) -> Vec<String> {
        self.inner.lock().unwrap().requests.clone()
    }
}

fn authorized(mock: &MockDevin, headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == format!("Bearer {}", mock.token))
        .unwrap_or(false)
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(json!({"status": 401, "title": "Unauthorized"})),
    )
        .into_response()
}

fn page(items: Vec<Value>, q: &BTreeMap<String, String>, page_size: usize) -> Value {
    let first: usize = q
        .get("first")
        .and_then(|s| s.parse().ok())
        .unwrap_or(page_size)
        .min(page_size);
    let start: usize = q.get("after").and_then(|s| s.parse().ok()).unwrap_or(0);
    let end = (start + first).min(items.len());
    let has_next = end < items.len();
    json!({
        "items": items[start..end],
        "end_cursor": if has_next { Value::String(end.to_string()) } else { Value::Null },
        "has_next_page": has_next,
        "total": items.len(),
    })
}

async fn list_orgs(
    State(mock): State<MockDevin>,
    headers: HeaderMap,
    Query(q): Query<BTreeMap<String, String>>,
) -> Response {
    if !authorized(&mock, &headers) {
        return unauthorized();
    }
    let mut inner = mock.inner.lock().unwrap();
    inner.requests.push(format!("GET organizations {q:?}"));
    let items = inner
        .orgs
        .iter()
        .map(|(id, name)| json!({"org_id": id, "name": name, "created_at": 1, "updated_at": 1}))
        .collect();
    axum::Json(page(items, &q, inner.page_size)).into_response()
}

fn outpost_json(id: &str, name: &str, allowed: &[String]) -> Value {
    json!({
        "metadata": {"outpost_id": id, "account_id": "account-1", "created_at": 1},
        "spec": {"name": name, "platform": null, "description": null, "allowed_org_ids": allowed},
        "status": {"queue_depth": 0, "active_claims": 0},
    })
}

async fn list_outposts(
    State(mock): State<MockDevin>,
    headers: HeaderMap,
    Query(q): Query<BTreeMap<String, String>>,
) -> Response {
    if !authorized(&mock, &headers) {
        return unauthorized();
    }
    let mut inner = mock.inner.lock().unwrap();
    inner.requests.push(format!("GET outposts {q:?}"));
    let items = inner
        .outposts
        .iter()
        .map(|(id, name, allowed)| outpost_json(id, name, allowed))
        .collect();
    axum::Json(page(items, &q, inner.page_size)).into_response()
}

async fn create_outpost(
    State(mock): State<MockDevin>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    if !authorized(&mock, &headers) {
        return unauthorized();
    }
    let mut inner = mock.inner.lock().unwrap();
    inner.requests.push(format!("POST outposts {body}"));
    let name = body["name"].as_str().unwrap_or_default().to_string();
    let allowed: Vec<String> = body["allowed_org_ids"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let id = format!("outpost_env-{}", inner.outposts.len() + 1);
    inner
        .outposts
        .push((id.clone(), name.clone(), allowed.clone()));
    axum::Json(outpost_json(&id, &name, &allowed)).into_response()
}

async fn get_default(
    State(mock): State<MockDevin>,
    headers: HeaderMap,
    Path(org_id): Path<String>,
) -> Response {
    if !authorized(&mock, &headers) {
        return unauthorized();
    }
    let mut inner = mock.inner.lock().unwrap();
    inner
        .requests
        .push(format!("GET default-platform {org_id}"));
    let body = inner
        .default_platform
        .get(&org_id)
        .cloned()
        .unwrap_or(json!({
            "platform": null, "outpost_pool_id": null, "outpost_pool_name": null
        }));
    axum::Json(body).into_response()
}

async fn put_default(
    State(mock): State<MockDevin>,
    headers: HeaderMap,
    Path(org_id): Path<String>,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    if !authorized(&mock, &headers) {
        return unauthorized();
    }
    let mut inner = mock.inner.lock().unwrap();
    inner
        .requests
        .push(format!("PUT default-platform {org_id} {body}"));
    let stored = json!({
        "platform": body["platform"],
        "outpost_pool_id": body["outpost_pool_id"],
        "outpost_pool_name": null,
    });
    inner.default_platform.insert(org_id, stored.clone());
    axum::Json(stored).into_response()
}
