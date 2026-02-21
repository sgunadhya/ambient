use ambient_core::{CoreError, Result};

use crate::normalizer::CloudKitPushPayload;
#[cfg(all(target_os = "macos", feature = "cloudkit-native"))]
use crate::normalizer::payload_from_bytes;
use crate::transport::CloudKitChangeFetcher;

pub struct NativeCloudKitFetcher {
    container: String,
    zone_name: String,
}

impl NativeCloudKitFetcher {
    pub fn new(container: String, zone_name: String) -> Self {
        Self {
            container,
            zone_name,
        }
    }
}

impl CloudKitChangeFetcher for NativeCloudKitFetcher {
    fn fetch_changes(
        &self,
        push_payload: &[u8],
        previous_token: Option<&str>,
    ) -> Result<CloudKitPushPayload> {
        self.fetch_changes_native(push_payload, previous_token)
    }

    fn mode_label(&self) -> &'static str {
        "native"
    }

    fn is_available(&self) -> bool {
        cfg!(all(target_os = "macos", feature = "cloudkit-native"))
    }
}

impl NativeCloudKitFetcher {
    #[cfg(all(target_os = "macos", feature = "cloudkit-native"))]
    fn fetch_changes_native(
        &self,
        push_payload: &[u8],
        previous_token: Option<&str>,
    ) -> Result<CloudKitPushPayload> {
        let _ = (&self.container, &self.zone_name, previous_token);
        payload_from_bytes(push_payload).map_err(|_| {
            CoreError::Unsupported("native cloudkit bridge enabled, fetch op wiring pending")
        })
    }

    #[cfg(not(all(target_os = "macos", feature = "cloudkit-native")))]
    fn fetch_changes_native(
        &self,
        _push_payload: &[u8],
        _previous_token: Option<&str>,
    ) -> Result<CloudKitPushPayload> {
        let _ = (&self.container, &self.zone_name);
        Err(CoreError::Unsupported(
            "native cloudkit bridge unavailable in this build",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::NativeCloudKitFetcher;
    use crate::transport::CloudKitChangeFetcher;

    #[test]
    fn availability_matches_build_flags() {
        let fetcher = NativeCloudKitFetcher::new(
            "iCloud.dev.ambient.private".to_string(),
            "AmbientZone".to_string(),
        );
        assert_eq!(
            fetcher.is_available(),
            cfg!(all(target_os = "macos", feature = "cloudkit-native"))
        );
    }
}
