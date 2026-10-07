//! Per-organization worker images: named image profiles plus ordered rules
//! that map organizations to a profile.
//!
//! Organizations matched by no rule use the pool template's image, so the
//! file lists only the exceptions and a handful of rules covers thousands of
//! orgs. It is shipped as its own ConfigMap, separate from the pool template,
//! so it can be edited in place: the provisioner re-reads it every pass and
//! the Deployment does not restart on changes to it.
//!
//! ```yaml
//! images:
//!   data-science: registry.example.com/devin/devin-outpost-ds:2026.10
//! rules:
//!   - { org: "Data Science", image: data-science }   # display name
//!   - { org: "ds-*",         image: data-science }   # slug glob
//!   - { org: org-2f9990d15a5d4f139af863bdff50b3ae, image: data-science }
//! ```
//!
//! Every image needs its own golden home snapshot before an org can be moved
//! to it; the reconciler keeps that org's current pool until it exists.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::devin::Organization;
use crate::error::{Error, Result};
use crate::naming;

/// The worker-images document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct WorkerImages {
    /// Profile name → worker image reference.
    pub images: BTreeMap<String, String>,
    /// Evaluated in order; the first rule whose `org` matches wins.
    pub rules: Vec<Rule>,
}

/// One organization → profile rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Glob (`*` any run, `?` one character) tested against the org's id,
    /// display name and slug ([`naming::slug`]).
    pub org: String,
    /// A key of [`WorkerImages::images`].
    pub image: String,
}

impl WorkerImages {
    /// Parse a document. Empty input is the empty configuration.
    pub fn parse(yaml: &str) -> Result<Self> {
        let images: Self = if yaml.trim().is_empty() {
            Self::default()
        } else {
            serde_yaml_ng::from_str(yaml).map_err(|e| Error::WorkerImages(e.to_string()))?
        };
        images.validate()?;
        Ok(images)
    }

    /// Load and parse a file.
    pub fn load(path: &Path) -> Result<Self> {
        let yaml = std::fs::read_to_string(path)
            .map_err(|e| Error::WorkerImages(format!("{}: {e}", path.display())))?;
        Self::parse(&yaml)
    }

    fn validate(&self) -> Result<()> {
        for (name, image) in &self.images {
            if name.trim().is_empty() || image.trim().is_empty() {
                return Err(Error::WorkerImages(format!(
                    "images[{name:?}] = {image:?}: profile names and image references must be non-empty"
                )));
            }
        }
        for (i, rule) in self.rules.iter().enumerate() {
            if rule.org.is_empty() {
                return Err(Error::WorkerImages(format!("rules[{i}].org is empty")));
            }
            if !self.images.contains_key(&rule.image) {
                return Err(Error::WorkerImages(format!(
                    "rules[{i}] (org {:?}) names unknown image {:?}; images: {:?}",
                    rule.org,
                    rule.image,
                    self.images.keys().collect::<Vec<_>>()
                )));
            }
        }
        Ok(())
    }

    /// The worker image for `org`: the first matching rule's profile, else
    /// `default`.
    pub fn resolve<'a>(&'a self, org: &Organization, default: &'a str) -> &'a str {
        let slug = naming::slug(&org.name);
        let candidates = [org.org_id.as_str(), org.name.as_str(), slug.as_str()];
        self.rules
            .iter()
            .find(|r| candidates.iter().any(|c| glob_matches(&r.org, c)))
            .map(|r| self.images[&r.image].as_str())
            .unwrap_or(default)
    }
}

fn glob_matches(pattern: &str, candidate: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let c: Vec<char> = candidate.chars().collect();
    let (mut pi, mut ci) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ci < c.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == c[ci]) {
            pi += 1;
            ci += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ci));
            pi += 1;
        } else if let Some((sp, sc)) = star {
            pi = sp + 1;
            ci = sc + 1;
            star = Some((sp, sc + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::helm_values_key;

    const CHART_VALUES: &str = include_str!("../../charts/devin-outposts-platform/values.yaml");
    const DEFAULT: &str = "registry.example/devin-outpost-prod:release-1";

    const DOC: &str = r#"
images:
  data-science: registry.example/devin-outpost-ds:2026.10
  legacy: registry.example/devin-outpost-prod:release-0
rules:
  - { org: "Data Science", image: data-science }
  - { org: "ds-*", image: data-science }
  - { org: org-2f9990d15a5d4f139af863bdff50b3ae, image: legacy }
  - { org: "*-legacy", image: legacy }
"#;

    fn org(id: &str, name: &str) -> Organization {
        Organization {
            org_id: id.into(),
            name: name.into(),
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn empty_and_shipped_documents_map_everything_to_the_default() {
        for doc in ["", "images: {}\nrules: []\n"] {
            let w = WorkerImages::parse(doc).unwrap();
            assert_eq!(w, WorkerImages::default());
            assert_eq!(w.resolve(&org("org-1", "Anything"), DEFAULT), DEFAULT);
        }
        // The chart renders the section minus its own `managed` switch.
        let shipped = helm_values_key(&[CHART_VALUES], "workerImages").unwrap();
        let mut shipped: serde_yaml_ng::Mapping = serde_yaml_ng::from_str(&shipped).unwrap();
        assert_eq!(
            shipped.remove("managed"),
            Some(serde_yaml_ng::Value::Bool(true))
        );
        let shipped = serde_yaml_ng::to_string(&shipped).unwrap();
        assert_eq!(
            WorkerImages::parse(&shipped).unwrap(),
            WorkerImages::default()
        );
    }

    #[test]
    fn resolves_by_name_slug_id_and_glob_in_rule_order() {
        let w = WorkerImages::parse(DOC).unwrap();
        let ds = "registry.example/devin-outpost-ds:2026.10";
        let legacy = "registry.example/devin-outpost-prod:release-0";
        assert_eq!(w.resolve(&org("org-1", "Data Science"), DEFAULT), ds);
        assert_eq!(w.resolve(&org("org-2", "DS Platform Team"), DEFAULT), ds);
        assert_eq!(
            w.resolve(
                &org("org-2f9990d15a5d4f139af863bdff50b3ae", "Primary"),
                DEFAULT
            ),
            legacy
        );
        assert_eq!(w.resolve(&org("org-3", "Payments Legacy"), DEFAULT), legacy);
        assert_eq!(w.resolve(&org("org-4", "Payments"), DEFAULT), DEFAULT);
        // First match wins: "DS Legacy" matches the ds-* rule before *-legacy.
        assert_eq!(w.resolve(&org("org-5", "DS Legacy"), DEFAULT), ds);
    }

    #[test]
    fn rejects_unknown_profile_unknown_field_and_empty_values() {
        let err = WorkerImages::parse("images: {a: x}\nrules: [{org: o, image: b}]\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown image \"b\""), "{err}");
        let err = WorkerImages::parse("images: {a: x}\norgs: {o: a}\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("orgs"), "{err}");
        let err = WorkerImages::parse("images: {a: ''}\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("non-empty"), "{err}");
        let err = WorkerImages::parse("images: {a: x}\nrules: [{org: '', image: a}]\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("rules[0].org"), "{err}");
    }

    #[test]
    fn glob_semantics() {
        assert!(glob_matches("*", ""));
        assert!(glob_matches("*", "anything"));
        assert!(glob_matches("ds-*", "ds-"));
        assert!(glob_matches("ds-*", "ds-platform"));
        assert!(!glob_matches("ds-*", "ads-platform"));
        assert!(glob_matches("*-ds-*", "team-ds-eu"));
        assert!(glob_matches("org-????", "org-abcd"));
        assert!(!glob_matches("org-????", "org-abcde"));
        assert!(glob_matches("a*b*c", "aXXbYYc"));
        assert!(!glob_matches("a*b*c", "aXXbYY"));
        assert!(glob_matches("Exact Name", "Exact Name"));
        assert!(!glob_matches("exact name", "Exact Name"));
    }
}
