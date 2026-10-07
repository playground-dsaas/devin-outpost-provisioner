//! Loading the Outposts service-user token.

use crate::config::TokenSource;
use crate::error::{Error, Result};

/// Resolve the token from its configured source. The value is never logged.
pub async fn load(source: &TokenSource) -> Result<String> {
    let token = match source {
        TokenSource::File(path) => std::fs::read_to_string(path)?,
        TokenSource::SecretsManager(id) => {
            let aws = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            let client = aws_sdk_secretsmanager::Client::new(&aws);
            let out = client
                .get_secret_value()
                .secret_id(id)
                .send()
                .await
                .map_err(|e| Error::SecretsManager(e.to_string()))?;
            out.secret_string()
                .map(str::to_string)
                .ok_or_else(|| Error::SecretsManager(format!("{id} has no SecretString")))?
        }
    };
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err(Error::Config("Outposts token is empty".into()));
    }
    Ok(token)
}
