use ambient_core::{CoreError, Result};

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
        _push_payload: &[u8],
        _previous_token: Option<&str>,
    ) -> Result<CloudKitPushPayload> {
        Err(CoreError::Unsupported(Box::leak(
            format!(
                "native cloudkit fetch bridge not enabled for container={} zone={}",
                self.container, self.zone_name
            )
            .into_boxed_str(),
        )))
    }
}
