use ambient_core::{CoreError, Result};

use crate::normalizer::{payload_from_bytes, CloudKitPushPayload};

#[derive(Debug, Clone)]
pub struct NativeFetchRequest {
    pub container: String,
    pub zone_name: String,
    pub previous_token: Option<String>,
    pub push_is_structured: bool,
    pub raw_payload: Vec<u8>,
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
        raw_payload: push_payload.to_vec(),
    })
}

#[cfg(all(target_os = "macos", feature = "cloudkit-native"))]
pub fn fetch_changes_native(request: NativeFetchRequest) -> Result<CloudKitPushPayload> {
    if request.push_is_structured {
        return payload_from_bytes(&request.raw_payload);
    }

    let _scope = (&request.container, &request.zone_name, &request.previous_token);
    Err(CoreError::Unsupported("native cloudkit fetch op wiring pending"))
}

#[cfg(not(all(target_os = "macos", feature = "cloudkit-native")))]
pub fn fetch_changes_native(request: NativeFetchRequest) -> Result<CloudKitPushPayload> {
    if request.push_is_structured {
        return payload_from_bytes(&request.raw_payload);
    }

    let _scope = (&request.container, &request.zone_name, &request.previous_token);
    Err(CoreError::Unsupported(
        "native cloudkit bridge unavailable in this build",
    ))
}

#[cfg(test)]
mod tests {
    use super::{build_fetch_request, fetch_changes_native};

    #[test]
    fn build_request_keeps_raw_payload() {
        let payload = br#"{"records":[]}"#;
        let req = build_fetch_request(payload, "iCloud.dev.ambient.private", "AmbientZone", None)
            .expect("request");
        assert_eq!(req.raw_payload, payload);
    }

    #[test]
    fn structured_payload_falls_back_to_json_mapping() {
        let req = build_fetch_request(
            br#"{"new_change_token":"next","records":[]}"#,
            "iCloud.dev.ambient.private",
            "AmbientZone",
            Some("prev"),
        )
        .expect("request");
        let mapped = fetch_changes_native(req).expect("mapping");
        assert_eq!(mapped.new_change_token.as_deref(), Some("next"));
        assert!(mapped.records.is_empty());
    }
}
