use ambient_core::{CoreError, Result};
use serde::Serialize;

const NATIVE_BRIDGE_CMD_ENV: &str = "AMBIENT_CLOUDKIT_NATIVE_BRIDGE_CMD";

use crate::normalizer::{payload_from_bytes, CloudKitPushPayload};

#[derive(Debug, Clone)]
pub struct NativeFetchRequest {
    pub container: String,
    pub zone_name: String,
    pub previous_token: Option<String>,
    pub push_is_structured: bool,
    pub raw_payload: Vec<u8>,
    pub bridge_command: Option<String>,
}

#[derive(Debug, Serialize)]
struct BridgeRequest<'a> {
    container: &'a str,
    zone_name: &'a str,
    previous_token: Option<&'a str>,
    push_payload: serde_json::Value,
}

pub fn build_fetch_request(
    push_payload: &[u8],
    container: &str,
    zone_name: &str,
    previous_token: Option<&str>,
    bridge_command: Option<&str>,
) -> Result<NativeFetchRequest> {
    let push_is_structured = serde_json::from_slice::<serde_json::Value>(push_payload).is_ok();
    Ok(NativeFetchRequest {
        container: container.to_string(),
        zone_name: zone_name.to_string(),
        previous_token: previous_token.map(|v| v.to_string()),
        push_is_structured,
        raw_payload: push_payload.to_vec(),
        bridge_command: bridge_command.map(|v| v.to_string()),
    })
}

#[cfg(all(target_os = "macos", feature = "cloudkit-native"))]
pub fn fetch_changes_native(request: NativeFetchRequest) -> Result<CloudKitPushPayload> {
    if let Some(mapped) = run_external_bridge(&request)? {
        return Ok(mapped);
    }

    if request.push_is_structured {
        return payload_from_bytes(&request.raw_payload);
    }

    let _scope = (&request.container, &request.zone_name, &request.previous_token);
    Err(CoreError::Unsupported("native cloudkit fetch op wiring pending"))
}

#[cfg(not(all(target_os = "macos", feature = "cloudkit-native")))]
pub fn fetch_changes_native(request: NativeFetchRequest) -> Result<CloudKitPushPayload> {
    if let Some(mapped) = run_external_bridge(&request)? {
        return Ok(mapped);
    }

    if request.push_is_structured {
        return payload_from_bytes(&request.raw_payload);
    }

    let _scope = (&request.container, &request.zone_name, &request.previous_token);
    Err(CoreError::Unsupported(
        "native cloudkit bridge unavailable in this build",
    ))
}

fn run_external_bridge(request: &NativeFetchRequest) -> Result<Option<CloudKitPushPayload>> {
    let cmd = request
        .bridge_command
        .as_ref()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .or_else(|| {
            std::env::var(NATIVE_BRIDGE_CMD_ENV)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        });
    let cmd = cmd;
    let Some(cmd) = cmd else {
        return Ok(None);
    };

    let mut child = std::process::Command::new(&cmd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| CoreError::Internal(format!("failed to spawn native bridge command: {e}")))?;

    let payload = serde_json::from_slice::<serde_json::Value>(&request.raw_payload)
        .unwrap_or(serde_json::Value::Null);
    let input = serde_json::to_vec(&BridgeRequest {
        container: &request.container,
        zone_name: &request.zone_name,
        previous_token: request.previous_token.as_deref(),
        push_payload: payload,
    })
    .map_err(|e| CoreError::Internal(format!("failed to encode native bridge request: {e}")))?;

    {
        use std::io::Write;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| CoreError::Internal("native bridge stdin unavailable".to_string()))?;
        stdin
            .write_all(&input)
            .map_err(|e| CoreError::Internal(format!("failed writing native bridge request: {e}")))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| CoreError::Internal(format!("native bridge command failed: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(CoreError::Internal(format!(
            "native bridge command exited with {}{}",
            output.status,
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        )));
    }

    let mapped = payload_from_bytes(&output.stdout)?;
    Ok(Some(mapped))
}

#[cfg(test)]
mod tests {
    use ambient_core::CoreError;

    use super::{build_fetch_request, fetch_changes_native};

    #[test]
    fn build_request_keeps_raw_payload() {
        let payload = br#"{"records":[]}"#;
        let req = build_fetch_request(
            payload,
            "iCloud.dev.ambient.private",
            "AmbientZone",
            None,
            None,
        )
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
            None,
        )
        .expect("request");
        let mapped = fetch_changes_native(req).expect("mapping");
        assert_eq!(mapped.new_change_token.as_deref(), Some("next"));
        assert!(mapped.records.is_empty());
    }

    #[test]
    fn non_structured_payload_without_bridge_is_unsupported() {
        std::env::remove_var("AMBIENT_CLOUDKIT_NATIVE_BRIDGE_CMD");
        let req = build_fetch_request(
            b"\x00\x01\x02",
            "iCloud.dev.ambient.private",
            "AmbientZone",
            None,
            None,
        )
        .expect("request");
        let err = fetch_changes_native(req).expect_err("should fail");
        assert!(matches!(err, CoreError::Unsupported(_)));
    }
}
