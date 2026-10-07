//! Stable names derived from an organization.
//!
//! The namespace is derived from the immutable `org_id` alone, so renaming an
//! organization never moves it (sessions, PVCs and quotas stay put). The
//! Outpost name is a slug of the display name because it is what humans see
//! in the Devin UI; it is only used at creation time, after which the Outpost
//! is tracked by ID (recorded on the namespace and pool).

use sha2::{Digest, Sha256};

/// Maximum length of a Kubernetes namespace name (DNS label).
const MAX_LABEL: usize = 63;
/// Maximum length of the human-readable slug portion.
const MAX_SLUG: usize = 24;
/// Length of the org-id-derived identifier in namespace names.
const NAMESPACE_ID_LEN: usize = 12;

/// Lowercase DNS-label-safe slug of a display name, at most [`MAX_SLUG`]
/// characters, never empty.
pub fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_dash = true;
    for c in name.chars() {
        let c = c.to_ascii_lowercase();
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
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "org".to_string()
    } else {
        out
    }
}

/// `len` lowercase hex characters uniquely tied to `org_id`.
///
/// Devin org IDs look like `org-<32 hex>`; the leading hex digits are used
/// directly so the result is recognisable. Anything else is hashed.
fn org_hex(org_id: &str, len: usize) -> String {
    let hex = org_id.strip_prefix("org-").unwrap_or("");
    if hex.len() >= len && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return hex[..len].to_ascii_lowercase();
    }
    let digest = Sha256::digest(org_id.as_bytes());
    digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()[..len]
        .to_string()
}

/// Namespace for an organization: `<prefix><12 hex of org id>`. Independent
/// of the display name.
pub fn namespace(prefix: &str, org_id: &str) -> String {
    let mut name = format!("{prefix}{}", org_hex(org_id, NAMESPACE_ID_LEN));
    name.truncate(MAX_LABEL);
    name
}

/// Devin Outpost name for an organization: `<prefix><slug>`.
pub fn outpost_name(prefix: &str, org_name: &str) -> String {
    let mut slug = slug(org_name);
    slug.truncate(MAX_LABEL.saturating_sub(prefix.len()));
    format!("{prefix}{}", slug.trim_end_matches('-'))
}

/// Name of the single `OutpostPool` inside every org namespace.
pub const POOL_NAME: &str = "org";
/// Name of the token `Secret` inside every org namespace.
pub const TOKEN_SECRET_NAME: &str = "devin-pool-token";
/// Key inside [`TOKEN_SECRET_NAME`].
pub const TOKEN_SECRET_KEY: &str = "token";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_normalises() {
        assert_eq!(slug("DSaaS Outposts"), "dsaas-outposts");
        assert_eq!(slug("  Acme, Inc. (EU) "), "acme-inc-eu");
        assert_eq!(slug("!!!"), "org");
        assert_eq!(slug("Ünïcödé Org"), "n-c-d-org");
        assert!(slug("a".repeat(100).as_str()).len() <= MAX_SLUG);
    }

    #[test]
    fn namespace_depends_only_on_org_id() {
        let id = "org-2f9990d15a5d4f139af863bdff50b3ae";
        assert_eq!(namespace("devin-org-", id), "devin-org-2f9990d15a5d");
        assert_eq!(
            namespace("devin-org-", "org-ABCDEF0123456789ffff"),
            "devin-org-abcdef012345"
        );
        assert_ne!(
            namespace("devin-org-", id),
            namespace("devin-org-", "org-2f9990d15a5e")
        );
        let hashed = namespace("devin-org-", "weird id");
        assert_eq!(hashed.len(), "devin-org-".len() + 12);
        assert!(
            hashed
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        );
    }

    #[test]
    fn outpost_name_is_prefixed_slug_and_fits_a_label() {
        assert_eq!(outpost_name("eks-", "Primary"), "eks-primary");
        assert_eq!(outpost_name("eks-", "Acme, Inc."), "eks-acme-inc");
        let long = outpost_name("eks-", &"x".repeat(200));
        assert!(long.len() <= MAX_LABEL);
    }
}
