//! Build the module id and daemon-issued launch nonce used to prove which process
//! is making a vault call or reading activity.

use std::env;
use subc_client_rs::ConsumerIdentity;
use subc_protocol::SUBC_MODULE_ID_ENV;

/// The SDK caches the nonce from the inherited launch-nonce descriptor or the
/// platform's launch environment. Share that accessor with automatic activity
/// request stamping instead of reading a one-use descriptor a second time.
pub fn supervised_identity() -> Result<Option<ConsumerIdentity>, String> {
    let launch_nonce = subc_client_rs::launch_nonce()
        .map_err(|error| format!("ck-bus cannot read its launch nonce: {error}"))?;
    Ok(match (env::var(SUBC_MODULE_ID_ENV), launch_nonce) {
        (Ok(module_id), Some(launch_nonce)) if !module_id.is_empty() => Some(ConsumerIdentity {
            module_id,
            launch_nonce: launch_nonce.value().to_string(),
        }),
        _ => None,
    })
}
