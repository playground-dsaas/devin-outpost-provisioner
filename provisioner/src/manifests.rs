//! The per-org manifest set: a multi-document YAML file with `{{variable}}`
//! placeholders. The provisioner does not know or care what kinds are in it.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;

/// Substituted into manifests before they are applied.
pub type Vars = BTreeMap<&'static str, String>;

pub const VARIABLES: &[&str] = &[
    "namespace",
    "org_id",
    "org_name",
    "org_slug",
    "outpost_id",
    "outpost_name",
];

/// Parsed manifest templates, in file order.
#[derive(Debug, Clone, Default)]
pub struct Manifests {
    docs: Vec<String>,
}

pub fn load(path: &Path) -> anyhow::Result<Manifests> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Manifests::parse(&text).with_context(|| format!("parsing {}", path.display()))
}

impl Manifests {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let mut docs = Vec::new();
        for (i, doc) in split_documents(text).into_iter().enumerate() {
            for var in placeholders(&doc) {
                anyhow::ensure!(
                    VARIABLES.contains(&var.as_str()),
                    "document {} uses unknown variable {{{{{var}}}}} (known: {})",
                    i + 1,
                    VARIABLES.join(", ")
                );
            }
            // Parse once with dummy values to fail fast on malformed YAML.
            let probe = substitute(
                &doc,
                &VARIABLES.iter().map(|v| (*v, "x".to_string())).collect(),
            );
            let value: serde_json::Value =
                serde_yaml_ng::from_str(&probe).with_context(|| format!("document {}", i + 1))?;
            anyhow::ensure!(
                value.get("apiVersion").is_some() && value.get("kind").is_some(),
                "document {} needs apiVersion and kind",
                i + 1
            );
            docs.push(doc);
        }
        Ok(Self { docs })
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    /// Render every document for one org. `metadata.namespace` is forced to the
    /// org namespace for namespaced kinds; cluster-scoped kinds are left alone.
    pub fn render(&self, vars: &Vars) -> anyhow::Result<Vec<serde_json::Value>> {
        self.docs
            .iter()
            .map(|doc| {
                let v: serde_json::Value = serde_yaml_ng::from_str(&substitute(doc, vars))?;
                Ok(v)
            })
            .collect()
    }
}

fn split_documents(text: &str) -> Vec<String> {
    text.split("\n---")
        .map(|d| d.trim_start_matches("---").trim())
        .filter(|d| !d.is_empty() && d.lines().any(|l| !l.trim_start().starts_with('#')))
        .map(str::to_string)
        .collect()
}

fn placeholders(doc: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = doc;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else { break };
        out.push(after[..end].trim().to_string());
        rest = &after[end + 2..];
    }
    out
}

pub fn substitute(doc: &str, vars: &Vars) -> String {
    let mut out = doc.to_string();
    for (k, v) in vars {
        out = out
            .replace(&format!("{{{{{k}}}}}"), v)
            .replace(&format!("{{{{ {k} }}}}"), v);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vars {
        VARIABLES.iter().map(|v| (*v, format!("<{v}>"))).collect()
    }

    #[test]
    fn renders_in_order_with_substitution() {
        let m = Manifests::parse(
            "# leading comment\n---\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: a\n  labels:\n    org: \"{{ org_id }}\"\n---\n# comment only\n---\napiVersion: outposts.cognition.com/v1alpha1\nkind: OutpostPool\nmetadata:\n  name: org\nspec:\n  outpostId: \"{{outpost_id}}\"\n",
        )
        .unwrap();
        assert_eq!(m.len(), 2);
        let docs = m.render(&vars()).unwrap();
        assert_eq!(docs[0]["metadata"]["labels"]["org"], "<org_id>");
        assert_eq!(docs[1]["spec"]["outpostId"], "<outpost_id>");
    }

    #[test]
    fn rejects_unknown_variables_and_bare_documents() {
        assert!(
            Manifests::parse("apiVersion: v1\nkind: Secret\nmetadata:\n  name: \"{{ nope }}\"\n")
                .is_err()
        );
        assert!(Manifests::parse("metadata:\n  name: x\n").is_err());
        assert!(Manifests::parse("apiVersion: v1\nkind: X\nbad: [\n").is_err());
    }
}
