//! Crate error type.

/// Errors raised by the provisioner.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The Devin API rejected `allowed_org_ids` (HTTP 422). Raised for org IDs
    /// that belong to another account or do not exist.
    #[error("devin api rejected organization ids: {0}")]
    InvalidOrgIds(String),

    /// Any other non-2xx Devin API response.
    #[error("devin api returned {status}: {body}")]
    Api {
        /// HTTP status code.
        status: u16,
        /// Response body, truncated.
        body: String,
    },

    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    #[error("kubernetes: {0}")]
    Kube(#[from] kube::Error),

    #[error("configuration: {0}")]
    Config(String),

    #[error("pool template: {0}")]
    Template(String),

    #[error("worker images: {0}")]
    WorkerImages(String),

    #[error("secrets manager: {0}")]
    SecretsManager(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Crate result alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;
