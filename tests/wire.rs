//! Wire-level tests of [`DevinClient`] against [`mock_server`], which mirrors
//! the enterprise API's actual shapes.

mod mock_server;

use mock_server::MockDevin;
use org_provisioner::devin::{DevinClient, Organization};
use serde_json::json;

const TOKEN: &str = "cog_testtoken";

async fn setup() -> (MockDevin, DevinClient) {
    let mock = MockDevin::start(TOKEN).await;
    let client = DevinClient::new(&mock.base_url, TOKEN).unwrap();
    (mock, client)
}

#[tokio::test]
async fn organizations_follow_the_cursor() {
    let (mock, client) = setup().await;
    for i in 0..5 {
        mock.add_org(&format!("org-{i}"), &format!("Org {i}"));
    }
    mock.set_page_size(2);
    let orgs = client.organizations().await.unwrap();
    let ids: Vec<&str> = orgs.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(ids, ["org-0", "org-1", "org-2", "org-3", "org-4"]);
    assert_eq!(orgs[1].name, "Org 1");
    assert_eq!(
        mock.requests()
            .iter()
            .filter(|r| r.starts_with("GET organizations"))
            .count(),
        3
    );
}

#[tokio::test]
async fn outposts_flatten_metadata_and_spec() {
    let (mock, client) = setup().await;
    mock.add_outpost("outpost_env-a", "sno-a", &["org-a"]);
    mock.add_outpost("outpost_env-b", "shared", &[]);
    let outposts = client.outposts().await.unwrap();
    assert_eq!(outposts[0].id, "outpost_env-a");
    assert_eq!(outposts[0].name, "sno-a");
    assert_eq!(outposts[0].allowed_org_ids, ["org-a"]);
    assert!(outposts[1].allowed_org_ids.is_empty());
}

#[tokio::test]
async fn create_outpost_restricts_to_the_org() {
    let (mock, client) = setup().await;
    let org = Organization {
        id: "org-a".into(),
        name: "Org A".into(),
    };
    let created = client.create_outpost("sno-org-a", &org).await.unwrap();
    assert_eq!(created.name, "sno-org-a");
    assert_eq!(created.allowed_org_ids, ["org-a"]);
    assert_eq!(mock.outposts().len(), 1);
    let post = mock
        .requests()
        .into_iter()
        .find(|r| r.starts_with("POST outposts"))
        .unwrap();
    assert!(post.contains("\"allowed_org_ids\":[\"org-a\"]"), "{post}");
}

#[tokio::test]
async fn default_platform_round_trips() {
    let (mock, client) = setup().await;
    assert!(!client.has_default_platform("org-a").await.unwrap());
    client
        .set_default_outpost("org-a", "outpost_env-a")
        .await
        .unwrap();
    assert_eq!(
        mock.default_platform("org-a").unwrap()["outpost_pool_id"],
        "outpost_env-a"
    );
    assert!(client.has_default_platform("org-a").await.unwrap());
    // A hosted platform counts as set too: the provisioner must not override it.
    mock.set_default_platform(
        "org-b",
        json!({"platform": "hosted", "outpost_pool_id": null}),
    );
    assert!(client.has_default_platform("org-b").await.unwrap());
}

#[tokio::test]
async fn bad_token_is_an_error_with_status() {
    let (mock, _) = setup().await;
    let client = DevinClient::new(&mock.base_url, "wrong").unwrap();
    let err = client.organizations().await.unwrap_err();
    assert!(format!("{err:#}").contains("401"), "{err:#}");
}
