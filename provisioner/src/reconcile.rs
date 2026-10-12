//! One pass: for every organization make sure an Outpost, a namespace, the
//! token Secret and the configured manifests exist, then make the Outpost the
//! org's default platform if it has none. Nothing is ever deleted.

use anyhow::Context;
use kube::api::{Api, DynamicObject, GroupVersionKind, Patch, PatchParams};
use kube::discovery::Scope;
use kube::{Client, Discovery};
use serde_json::json;

use crate::config::Config;
use crate::devin::{DevinClient, Organization, Outpost};
use crate::manifests::{Manifests, Vars};

pub const FIELD_MANAGER: &str = "org-provisioner";
/// Configured manifests use their own manager so a manifest that adds labels to
/// the org Namespace does not take ownership of the fields set above.
pub const MANIFESTS_FIELD_MANAGER: &str = "org-provisioner-manifests";
pub const LABEL_MANAGED_BY: &str = "app.kubernetes.io/managed-by";
pub const LABEL_ORG_ID: &str = "outposts.cognition.com/org-id";
pub const ANNOTATION_ORG_NAME: &str = "outposts.cognition.com/org-name";
pub const ANNOTATION_OUTPOST_ID: &str = "outposts.cognition.com/outpost-id";
pub const TOKEN_SECRET_NAME: &str = "devin-pool-token";
pub const TOKEN_SECRET_KEY: &str = "token";

pub async fn run_pass(
    config: &Config,
    devin: &DevinClient,
    kube: &Client,
    manifests: &Manifests,
    token: &str,
) -> anyhow::Result<()> {
    let orgs = devin
        .organizations()
        .await
        .context("listing organizations")?;
    let mut outposts = devin.outposts().await.context("listing outposts")?;
    let discovery = Discovery::new(kube.clone())
        .run()
        .await
        .context("API discovery")?;
    let mut failed = 0;
    for org in orgs
        .iter()
        .filter(|o| !config.exclude_org_ids.contains(&o.id))
    {
        if let Err(e) = reconcile_org(
            config,
            devin,
            kube,
            &discovery,
            manifests,
            token,
            org,
            &mut outposts,
        )
        .await
        {
            failed += 1;
            tracing::error!(org = org.id, error = format!("{e:#}"), "org not reconciled");
        }
    }
    tracing::info!(
        orgs = orgs.len(),
        failed,
        manifests = manifests.len(),
        "pass complete"
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn reconcile_org(
    config: &Config,
    devin: &DevinClient,
    kube: &Client,
    discovery: &Discovery,
    manifests: &Manifests,
    token: &str,
    org: &Organization,
    outposts: &mut Vec<Outpost>,
) -> anyhow::Result<()> {
    let namespace = crate::naming::namespace(&config.namespace_prefix, &org.id);
    let outpost = ensure_outpost(config, devin, org, outposts).await?;

    let mut vars = Vars::new();
    vars.insert("namespace", namespace.clone());
    vars.insert("org_id", org.id.clone());
    vars.insert("org_name", org.name.clone());
    vars.insert("org_slug", crate::naming::slug(&org.name));
    vars.insert("outpost_id", outpost.id.clone());
    vars.insert("outpost_name", outpost.name.clone());

    apply(
        kube,
        discovery,
        &namespace,
        FIELD_MANAGER,
        namespace_manifest(&namespace, org, &outpost),
    )
    .await?;
    apply(
        kube,
        discovery,
        &namespace,
        FIELD_MANAGER,
        token_secret(&namespace, token),
    )
    .await?;
    for doc in manifests.render(&vars)? {
        apply(kube, discovery, &namespace, MANIFESTS_FIELD_MANAGER, doc).await?;
    }

    if devin.default_outpost(&org.id).await?.is_none() {
        devin.set_default_outpost(&org.id, &outpost.id).await?;
        tracing::info!(
            org = org.id,
            outpost = outpost.id,
            "set as default platform"
        );
    }
    Ok(())
}

/// Reuse the Outpost named for this org and restricted to it; otherwise create one.
async fn ensure_outpost(
    config: &Config,
    devin: &DevinClient,
    org: &Organization,
    outposts: &mut Vec<Outpost>,
) -> anyhow::Result<Outpost> {
    let name = crate::naming::outpost_name(&config.outpost_name_prefix, &org.name);
    let mine = |o: &Outpost| o.allowed_org_ids.len() == 1 && o.allowed_org_ids[0] == org.id;
    if let Some(o) = outposts.iter().find(|o| mine(o) && o.name == name) {
        return Ok(o.clone());
    }
    if let Some(o) = outposts
        .iter()
        .find(|o| mine(o) && o.name.starts_with(&config.outpost_name_prefix))
    {
        return Ok(o.clone()); // org was renamed; keep the existing Outpost
    }
    let created = devin
        .create_outpost(&name, &org.id)
        .await
        .context("creating outpost")?;
    tracing::info!(org = org.id, outpost = created.id, name, "created outpost");
    outposts.push(created.clone());
    Ok(created)
}

fn namespace_manifest(name: &str, org: &Organization, outpost: &Outpost) -> serde_json::Value {
    json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": {
            "name": name,
            "labels": { LABEL_MANAGED_BY: FIELD_MANAGER, LABEL_ORG_ID: org.id },
            "annotations": { ANNOTATION_ORG_NAME: org.name, ANNOTATION_OUTPOST_ID: outpost.id },
        }
    })
}

fn token_secret(namespace: &str, token: &str) -> serde_json::Value {
    json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": TOKEN_SECRET_NAME,
            "namespace": namespace,
            "labels": { LABEL_MANAGED_BY: FIELD_MANAGER },
        },
        "type": "Opaque",
        "stringData": { TOKEN_SECRET_KEY: token },
    })
}

/// Server-side apply of any object. Namespaced kinds land in `namespace`
/// unless the manifest says otherwise.
async fn apply(
    kube: &Client,
    discovery: &Discovery,
    namespace: &str,
    manager: &str,
    mut doc: serde_json::Value,
) -> anyhow::Result<()> {
    let gvk = gvk_of(&doc)?;
    let (ar, caps) = discovery
        .resolve_gvk(&gvk)
        .with_context(|| format!("{gvk:?} is not served by this cluster"))?;
    let name = doc["metadata"]["name"]
        .as_str()
        .context("metadata.name is required")?
        .to_string();
    let api: Api<DynamicObject> = match caps.scope {
        Scope::Cluster => Api::all_with(kube.clone(), &ar),
        Scope::Namespaced => {
            let ns = doc["metadata"]["namespace"]
                .as_str()
                .unwrap_or(namespace)
                .to_string();
            doc["metadata"]["namespace"] = json!(ns);
            Api::namespaced_with(kube.clone(), &ns, &ar)
        }
    };
    let params = PatchParams::apply(manager).force();
    api.patch(&name, &params, &Patch::Apply(&doc))
        .await
        .with_context(|| format!("applying {}/{name}", gvk.kind))?;
    tracing::debug!(kind = gvk.kind, name, "applied");
    Ok(())
}

fn gvk_of(doc: &serde_json::Value) -> anyhow::Result<GroupVersionKind> {
    let api_version = doc["apiVersion"]
        .as_str()
        .context("apiVersion is required")?;
    let kind = doc["kind"].as_str().context("kind is required")?;
    let (group, version) = api_version.split_once('/').unwrap_or(("", api_version));
    Ok(GroupVersionKind::gvk(group, version, kind))
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{Namespace, Secret};
    use kube::discovery::ApiResource;

    fn org() -> Organization {
        Organization {
            id: "org-2f9990d15a5d4f139af863bdff50b3ae".into(),
            name: "DSaaS".into(),
        }
    }

    #[test]
    fn namespace_and_secret_are_well_formed() {
        let outpost = Outpost {
            id: "op-1".into(),
            name: "outpost-dsaas".into(),
            allowed_org_ids: vec![],
        };
        let ns: Namespace = serde_json::from_value(namespace_manifest(
            "devin-org-2f9990d15a5d",
            &org(),
            &outpost,
        ))
        .unwrap();
        let labels = ns.metadata.labels.unwrap();
        assert_eq!(labels[LABEL_ORG_ID], org().id);
        let secret: Secret = serde_json::from_value(token_secret("ns", "tok")).unwrap();
        assert_eq!(secret.string_data.unwrap()[TOKEN_SECRET_KEY], "tok");
    }

    #[test]
    fn gvk_handles_core_and_grouped_kinds() {
        let core = gvk_of(&json!({"apiVersion": "v1", "kind": "Secret"})).unwrap();
        assert_eq!((core.group.as_str(), core.version.as_str()), ("", "v1"));
        let crd = gvk_of(
            &json!({"apiVersion": "outposts.cognition.com/v1alpha1", "kind": "OutpostPool"}),
        )
        .unwrap();
        assert_eq!(crd.group, "outposts.cognition.com");
        let _ = ApiResource::from_gvk(&crd);
    }
}
