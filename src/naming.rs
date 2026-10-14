//! Names derived from an organization. The namespace comes from the
//! immutable org id, so renaming an org never moves it; the Outpost name is
//! a slug of the display name because that is what people see in Devin, and
//! it is only used at creation time (afterwards the Outpost is tracked by id).

use sha2::{Digest, Sha256};

const MAX_LABEL: usize = 63;
const MAX_SLUG: usize = 40;
const NAMESPACE_ID_LEN: usize = 12;

/// Lowercase DNS-label-safe slug, at most 40 characters, never empty.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true;
    for c in name.chars().map(|c| c.to_ascii_lowercase()) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
        if out.len() >= MAX_SLUG {
            break;
        }
    }
    let out = out.trim_matches('-');
    if out.is_empty() {
        "org".into()
    } else {
        out.into()
    }
}

/// `<prefix><12 hex>`: the hex of an `org-<hex>` id, otherwise a hash of it.
pub fn namespace(prefix: &str, org_id: &str) -> String {
    let hex = org_id.strip_prefix("org-").unwrap_or("");
    let id = if hex.len() >= NAMESPACE_ID_LEN && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        hex[..NAMESPACE_ID_LEN].to_ascii_lowercase()
    } else {
        format!("{:x}", Sha256::digest(org_id.as_bytes()))[..NAMESPACE_ID_LEN].to_string()
    };
    let mut name = format!("{prefix}{id}");
    name.truncate(MAX_LABEL);
    name
}

/// `<prefix><slug>`, trimmed to fit a label.
pub fn outpost_name(prefix: &str, org_name: &str) -> String {
    let mut slug = slug(org_name);
    slug.truncate(MAX_LABEL.saturating_sub(prefix.len()));
    format!("{prefix}{}", slug.trim_end_matches('-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_normalises() {
        assert_eq!(slug("DSaaS Outposts"), "dsaas-outposts");
        assert_eq!(slug("  Acme, Inc. (EU) "), "acme-inc-eu");
        assert_eq!(slug("!!!"), "org");
        assert!(slug(&"a".repeat(100)).len() <= MAX_SLUG);
    }

    #[test]
    fn namespace_depends_only_on_org_id() {
        let id = "org-2f9990d15a5d4f139af863bdff50b3ae";
        assert_eq!(namespace("devin-org-", id), "devin-org-2f9990d15a5d");
        let hashed = namespace("devin-org-", "weird id");
        assert_eq!(hashed.len(), "devin-org-".len() + NAMESPACE_ID_LEN);
        assert!(
            hashed
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        );
    }

    #[test]
    fn outpost_name_fits_a_label() {
        assert_eq!(outpost_name("ocp-", "Acme, Inc."), "ocp-acme-inc");
        assert!(outpost_name("ocp-", &"x".repeat(200)).len() <= MAX_LABEL);
    }
}
