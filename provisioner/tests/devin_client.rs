//! `DevinClient` against a mocked Devin API served by axum on localhost.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{delete, get};
use org_provisioner::Error;
use org_provisioner::devin::{CreateOutpost, DevinApi, DevinClient};
use serde_json::{Value, json};

#[derive(Default)]
struct Seen {
    requests: Vec<String>,
    auth: Vec<Option<String>>,
    bodies: Vec<Value>,
}

type Shared = Arc<Mutex<Seen>>;

fn record(state: &Shared, headers: &HeaderMap, line: String) {
    let mut s = state.lock().unwrap();
    s.requests.push(line);
    s.auth.push(
        headers
            .get("authorization")
            .map(|v| v.to_str().unwrap().to_string()),
    );
}

async fn orgs(
    State(state): State<Shared>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    record(
        &state,
        &headers,
        format!("GET orgs after={:?}", q.get("after")),
    );
    assert_eq!(q.get("first").map(String::as_str), Some("100"));
    let body = match q.get("after").map(String::as_str) {
        None => json!({
            "items": [{"org_id": "org-1", "name": "One", "created_at": 1, "updated_at": 2}],
            "end_cursor": "c1",
            "has_next_page": true
        }),
        Some("c1") => json!({
            "items": [{"org_id": "org-2", "name": "Two"}],
            "end_cursor": "c2",
            "has_next_page": true
        }),
        Some("c2") => json!({
            "items": [{"org_id": "org-3", "name": "Three"}],
            "end_cursor": null,
            "has_next_page": false
        }),
        other => panic!("unexpected cursor {other:?}"),
    };
    axum::Json(body)
}

async fn list_outposts(State(state): State<Shared>, headers: HeaderMap) -> impl IntoResponse {
    record(&state, &headers, "GET outposts".into());
    axum::Json(json!({
        "items": [{
            "metadata": {"outpost_id": "op_1", "account_id": "acct", "created_at": 1789766532, "owner_user_id": null},
            "spec": {"name": "k3s", "platform": null, "description": null, "allowed_org_ids": null},
            "status": {"queue_depth": 0, "active_claims": 1}
        }],
        "end_cursor": null,
        "has_next_page": false,
        "total": 1
    }))
}

async fn create_outpost(
    State(state): State<Shared>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    record(&state, &headers, "POST outposts".into());
    state.lock().unwrap().bodies.push(body.clone());
    let ids = body["allowed_org_ids"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if ids.iter().any(|v| v == "org-foreign") {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            axum::Json(json!({"detail": "Invalid organization IDs: org-foreign"})),
        );
    }
    if body["name"] == "boom" {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            axum::Json(json!({"detail": [{"loc": ["body", "name"], "msg": "too short"}]})),
        );
    }
    (
        StatusCode::OK,
        axum::Json(json!({
            "metadata": {"outpost_id": "op_new", "account_id": "acct", "created_at": 1},
            "spec": {"name": body["name"], "platform": null, "description": body["description"], "allowed_org_ids": body["allowed_org_ids"]},
            "status": {"queue_depth": 0, "active_claims": 0}
        })),
    )
}

async fn delete_outpost(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    record(&state, &headers, format!("DELETE outposts/{id}"));
    match id.as_str() {
        "op_gone" => StatusCode::NOT_FOUND,
        "op_denied" => StatusCode::FORBIDDEN,
        _ => StatusCode::NO_CONTENT,
    }
}

async fn start() -> (DevinClient, Shared) {
    let state: Shared = Arc::default();
    let app = Router::new()
        .route("/v3/enterprise/organizations", get(orgs))
        .route("/opbeta/outposts", get(list_outposts).post(create_outpost))
        .route("/opbeta/outposts/{id}", delete(delete_outpost))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = DevinClient::new(format!("http://{addr}/"), "cog_secret").unwrap();
    (client, state)
}

#[tokio::test]
async fn paginates_organizations_with_cursor() {
    let (client, state) = start().await;
    let orgs = client.list_organizations().await.unwrap();
    assert_eq!(
        orgs.iter().map(|o| o.org_id.as_str()).collect::<Vec<_>>(),
        ["org-1", "org-2", "org-3"]
    );
    assert_eq!(orgs[0].created_at, Some(1));
    let s = state.lock().unwrap();
    assert_eq!(s.requests.len(), 3);
    assert!(
        s.auth
            .iter()
            .all(|a| a.as_deref() == Some("Bearer cog_secret"))
    );
}

#[tokio::test]
async fn parses_outposts_ignoring_unknown_fields() {
    let (client, _) = start().await;
    let outposts = client.list_outposts().await.unwrap();
    assert_eq!(outposts.len(), 1);
    assert_eq!(outposts[0].metadata.outpost_id, "op_1");
    assert_eq!(outposts[0].spec.name, "k3s");
    assert_eq!(outposts[0].spec.allowed_org_ids, None);
}

#[tokio::test]
async fn create_sends_allowed_org_ids_and_maps_422() {
    let (client, state) = start().await;
    let ok = client
        .create_outpost(&CreateOutpost {
            name: "eks-one-abcdef01".into(),
            description: Some("d".into()),
            allowed_org_ids: Some(vec!["org-1".into()]),
        })
        .await
        .unwrap();
    assert_eq!(ok.metadata.outpost_id, "op_new");
    assert_eq!(ok.spec.allowed_org_ids, Some(vec!["org-1".to_string()]));
    assert_eq!(
        state.lock().unwrap().bodies[0]["allowed_org_ids"],
        json!(["org-1"])
    );

    let err = client
        .create_outpost(&CreateOutpost {
            name: "x".into(),
            description: None,
            allowed_org_ids: Some(vec!["org-foreign".into()]),
        })
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::InvalidOrgIds(ref d) if d.contains("org-foreign")),
        "{err:?}"
    );

    let err = client
        .create_outpost(&CreateOutpost {
            name: "boom".into(),
            description: None,
            allowed_org_ids: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Api { status: 422, .. }), "{err:?}");
}

#[tokio::test]
async fn delete_treats_404_as_success_and_surfaces_other_errors() {
    let (client, _) = start().await;
    client.delete_outpost("op_1").await.unwrap();
    client.delete_outpost("op_gone").await.unwrap();
    let err = client.delete_outpost("op_denied").await.unwrap_err();
    assert!(matches!(err, Error::Api { status: 403, .. }), "{err:?}");
}

#[test]
fn debug_redacts_token() {
    let client = DevinClient::new("https://api.devin.ai", "cog_secret").unwrap();
    let dbg = format!("{client:?}");
    assert!(!dbg.contains("cog_secret"));
    assert!(dbg.contains("<redacted>"));
}
