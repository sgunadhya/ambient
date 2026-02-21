use ambient_core::{CoreError, Result};

use crate::normalizer::CloudKitPushPayload;

#[derive(Debug, Clone)]
pub struct NativeFetchRequest {
    pub container: String,
    pub zone_name: String,
    pub previous_token: Option<String>,
    pub push_is_structured: bool,
}

pub fn build_fetch_request(
    push_payload: &[u8],
    container: &str,
    zone_name: &str,
    previous_token: Option<&str>,
) -> Result<NativeFetchRequest> {
    let push_is_structured = serde_json::from_slice::<serde_json::Value>(push_payload).is_ok();
    Ok(NativeFetchRequest {
        container: container.to_string(),
        zone_name: zone_name.to_string(),
        previous_token: previous_token.map(|v| v.to_string()),
        push_is_structured,
    })
}

#[cfg(all(target_os = "macos", feature = "cloudkit-native"))]
pub fn fetch_changes_native(_request: NativeFetchRequest) -> Result<CloudKitPushPayload> {
    Err(CoreError::Unsupported(
        "native cloudkit fetch op wiring pending",
    ))
}

#[cfg(not(all(target_os = "macos", feature = "cloudkit-native")))]
pub fn fetch_changes_native(_request: NativeFetchRequest) -> Result<CloudKitPushPayload> {
    Err(CoreError::Unsupported(
        "native cloudkit bridge unavailable in this build",
    ))
}
