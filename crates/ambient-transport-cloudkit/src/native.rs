use ambient_core::Result;

use crate::apple_bridge::{build_fetch_request, fetch_changes_native};
use crate::normalizer::CloudKitPushPayload;
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
        let request =
            build_fetch_request(push_payload, &self.container, &self.zone_name, previous_token)?;
        fetch_changes_native(request)
    }

    #[cfg(not(all(target_os = "macos", feature = "cloudkit-native")))]
    fn fetch_changes_native(
        &self,
        push_payload: &[u8],
        previous_token: Option<&str>,
    ) -> Result<CloudKitPushPayload> {
        let request =
            build_fetch_request(push_payload, &self.container, &self.zone_name, previous_token)?;
        fetch_changes_native(request)
    }
}

#[cfg(test)]
mod tests {
    use super::NativeCloudKitFetcher;
    use crate::apple_bridge::build_fetch_request;
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

    #[test]
    fn build_fetch_request_marks_structured_payload() {
        let req = build_fetch_request(
            br#"{"cloudkit_push":{"records":[]}}"#,
            "iCloud.dev.ambient.private",
            "AmbientZone",
            Some("tok-1"),
        )
        .expect("request");
        assert!(req.push_is_structured);
        assert_eq!(req.previous_token.as_deref(), Some("tok-1"));
    }
}
